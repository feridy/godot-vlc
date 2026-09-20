/*
* Copyright (c) 2025 xiSage
*
* This library is free software; you can redistribute it and/or
* modify it under the terms of the GNU Lesser General Public
* License as published by the Free Software Foundation; either
* version 2.1 of the License, or (at your option) any later version.
*
* This library is distributed in the hope that it will be useful,
* but WITHOUT ANY WARRANTY; without even the implied warranty of
* MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the GNU
* Lesser General Public License for more details.
*
* You should have received a copy of the GNU Lesser General Public
* License along with this library; if not, write to the Free Software
* Foundation, Inc., 51 Franklin Street, Fifth Floor, Boston, MA  02110-1301
* USA
*/

use std::{
    ffi::{c_char, c_int, c_uint, c_void},
    ptr::slice_from_raw_parts,
    sync::atomic::{AtomicBool, Ordering},
};

use godot::{classes::native::AudioFrame, prelude::*};
use ringbuf::{HeapProd, traits::Producer};

/// Everything libvlc's audio output thread is given, and the only thing it is
/// allowed to touch.
///
/// This is the `opaque` pointer handed to `libvlc_audio_set_callbacks`, so it is
/// all every callback below receives -- and it deliberately holds no Godot
/// object. libvlc runs these callbacks on its own audio output thread ("the
/// LibVLC media player decodes and post-processes the audio signal
/// asynchronously (in an internal thread)", `libvlc_media_player.h`), while the
/// [`AudioStreamPlayer`][godot::classes::AudioStreamPlayer] that drains the ring
/// buffer is a child of the player node and is destroyed by the engine *before*
/// `Drop for VlcMediaPlayer` runs: the engine frees a node's children from its
/// `NOTIFICATION_PREDELETE` (`scene/main/node.cpp`), and the extension instance
/// is only released afterwards, from `Object::~Object()`.
///
/// So a `Gd<AudioStreamPlayer>` read from here is a use-after-free with a
/// deadline, and reading one is what this file used to do. The first callback
/// after the engine freed the node dereferenced it, `ensure_object_alive` fired,
/// and the panic -- raised inside an `extern "C"` callback, where unwinding is
/// not allowed -- became `thread caused non-unwinding panic. aborting.`:
/// `Aborted (core dumped)`, exit code 134, with
/// `AudioStreamPlayer::upcast_ref: access to instance with ID ... after it has
/// been freed` as the line above it.
///
/// The callbacks therefore only write: samples into the ring buffer, and
/// requests into the flags. `VlcMediaPlayer::service_audio_requests` takes both
/// on the main thread, once a frame, where the objects involved are alive by
/// construction and nothing else is freeing them.
pub(super) struct AudioShared {
    /// Where the decoded samples go. Drained by
    /// `InternalAudioStreamPlayback` on the engine's audio thread.
    pub(super) prod: HeapProd<AudioFrame>,
    /// "Audio is arriving and the player should be running." Set on every block
    /// libvlc hands out and cleared by the main thread only once it could act on
    /// it -- so a request made while the node is outside the scene tree survives
    /// to the frame the node is back, rather than being dropped.
    pub(super) wants_play: AtomicBool,
    /// The pause state libvlc last asked for. A state and not an event: libvlc
    /// calls pause and resume on a transition only ("the pause callback is never
    /// called if the audio is already paused", `libvlc_media_player.h`), so the
    /// last writer holds the answer, and a transition landing between two frames
    /// cannot be lost.
    pub(super) wants_paused: AtomicBool,
    /// "libvlc discarded its pending buffers" -- a seek, or a stop. The main
    /// thread stops the player and empties the ring buffer to match.
    pub(super) wants_flush: AtomicBool,
    /// The engine's mix rate, read on the main thread while the player was
    /// built. `audio_setup_callback` reports it to libvlc; asking
    /// `AudioServer::singleton()` for it there would be one more engine object
    /// reached from libvlc's thread.
    pub(super) mix_rate: u32,
}

impl AudioShared {
    pub(super) fn new(prod: HeapProd<AudioFrame>, mix_rate: u32) -> Self {
        Self {
            prod,
            wants_play: AtomicBool::new(false),
            wants_paused: AtomicBool::new(false),
            wants_flush: AtomicBool::new(false),
            mix_rate,
        }
    }
}

pub(super) unsafe extern "C" fn audio_play_callback(
    data: *mut c_void,
    samples: *const c_void,
    count: c_uint,
    _pts: i64,
) {
    unsafe {
        let shared = (data as *mut AudioShared).as_mut().unwrap();

        let samples_slice = slice_from_raw_parts(samples as *const f32, count as usize * 2)
            .as_ref()
            .unwrap();

        for i in 0..count as usize {
            let left = samples_slice[i * 2];
            let right = samples_slice[i * 2 + 1];
            let frame = AudioFrame { left, right };
            if shared.prod.try_push(frame).is_err() {
                godot_error!("godot-vlc: audio buffer full");
                break;
            }
        }

        // Whether playback needs starting is the main thread's call to make: it
        // is the side that may read `is_playing` at all, and the side allowed to
        // call `play`.
        shared.wants_play.store(true, Ordering::Release);
    }
}

pub(super) unsafe extern "C" fn audio_pause_callback(data: *mut c_void, _pts: i64) {
    unsafe {
        let shared = (data as *mut AudioShared).as_mut().unwrap();
        shared.wants_paused.store(true, Ordering::Release);
    }
}

pub(super) unsafe extern "C" fn audio_resume_callback(data: *mut c_void, _pts: i64) {
    unsafe {
        let shared = (data as *mut AudioShared).as_mut().unwrap();
        shared.wants_paused.store(false, Ordering::Release);
    }
}

pub(super) unsafe extern "C" fn audio_flush_callback(data: *mut c_void, _pts: i64) {
    unsafe {
        let shared = (data as *mut AudioShared).as_mut().unwrap();
        shared.wants_flush.store(true, Ordering::Release);
    }
}

pub(super) unsafe extern "C" fn audio_drain_callback(_data: *mut c_void) {
    // do nothing
}

pub(super) unsafe extern "C" fn audio_setup_callback(
    opaque: *mut *mut c_void,
    format: *mut c_char,
    rate: *mut c_uint,
    channels: *mut c_uint,
) -> c_int {
    unsafe {
        format.copy_from(c"FL32".as_ptr(), 5);
        // `opaque` is the address of the data pointer this player registered
        // with `libvlc_audio_set_callbacks` -- which is where the main thread
        // left the mix rate. `rate` is an in/out parameter, so if libvlc ever
        // stops honouring that contract, its own suggestion is what stands
        // rather than a rate invented here.
        if !opaque.is_null() && !(*opaque).is_null() {
            *rate = (*(*opaque as *const AudioShared)).mix_rate;
        }
        *channels = 2;
        0
    }
}

pub(super) unsafe extern "C" fn audio_cleanup_callback(_opaque: *mut c_void) {
    // do nothing
}

#[cfg(test)]
mod tests {
    use super::*;
    use ringbuf::{
        HeapCons, HeapRb,
        traits::{Consumer, Split},
    };

    /// Builds the state the callbacks are attached with, and the consumer the
    /// engine would drain it through.
    fn shared(capacity: usize) -> (Box<AudioShared>, HeapCons<AudioFrame>) {
        let (prod, cons) = HeapRb::new(capacity).split();
        (Box::new(AudioShared::new(prod, 48000)), cons)
    }

    fn c_ptr(state: &mut AudioShared) -> *mut c_void {
        state as *mut AudioShared as *mut c_void
    }

    fn interleaved(frames: &[(f32, f32)]) -> Vec<f32> {
        frames
            .iter()
            .flat_map(|(left, right)| [*left, *right])
            .collect()
    }

    /// What libvlc hands over has to land in the ring buffer unchanged and be
    /// announced through the flag -- that handover is the whole contract between
    /// its thread and the main one, and it is the part of it that can be checked
    /// without an engine.
    #[test]
    fn delivers_samples_and_asks_for_playback() {
        let (mut state, mut cons) = shared(8);
        let samples = interleaved(&[(0.1, -0.1), (0.2, -0.2)]);

        unsafe {
            audio_play_callback(c_ptr(&mut state), samples.as_ptr() as *const c_void, 2, 0);
        }

        for expected in [(0.1, -0.1), (0.2, -0.2)] {
            let frame = cons.try_pop().expect("both frames were pushed");
            assert_eq!((frame.left, frame.right), expected);
        }
        assert!(state.wants_play.swap(false, Ordering::AcqRel));
        assert!(!state.wants_play.load(Ordering::Acquire));
    }

    /// The pause flags carry a state, not an event: libvlc reports a transition
    /// only, so whichever side reported last is the one the main thread has to
    /// apply. Losing that would leave audio playing while the media is paused.
    #[test]
    fn keeps_the_pause_state_that_was_reported_last() {
        let (mut state, _cons) = shared(8);

        unsafe { audio_pause_callback(c_ptr(&mut state), 0) };
        assert!(state.wants_paused.load(Ordering::Acquire));

        unsafe { audio_resume_callback(c_ptr(&mut state), 0) };
        assert!(!state.wants_paused.load(Ordering::Acquire));
    }

    /// A flush has to survive until the main thread takes it: it is the signal
    /// to stop and drop what is queued, and clearing it here would leave the
    /// stale samples of a seek in the ring buffer.
    #[test]
    fn leaves_a_flush_for_the_main_thread() {
        let (mut state, _cons) = shared(8);

        unsafe { audio_flush_callback(c_ptr(&mut state), 0) };

        assert!(state.wants_flush.swap(false, Ordering::AcqRel));
        assert!(!state.wants_flush.load(Ordering::Acquire));
    }
}
