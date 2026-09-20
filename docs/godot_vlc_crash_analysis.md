# Linux 崩溃归因：`AudioStreamPlayer::upcast_ref: access to instance ... after it has been freed`

对应日志（Linux，退出码 134）：

```
ERROR: Playback can only happen when a node is inside the scene tree
   at: play_basic (scene/audio/audio_stream_player_internal.cpp:145)
ERROR: Playback can only happen when a node is inside the scene tree
   at: play_basic (scene/audio/audio_stream_player_internal.cpp:145)
ERROR: [panic .../godot-core-0.5.2/src/classes/class_runtime.rs:279]
       AudioStreamPlayer::upcast_ref: access to instance with ID 1780448498250 after it has been freed
ERROR: [panic .../library/core/src/panicking.rs:225]  panic in a function that cannot unwind
thread caused non-unwinding panic. aborting.
Aborted (core dumped)        # 退出码 134 (SIGABRT)
```

## 1. 结论

崩在**本插件自己的音频回调**里，不是 GDScript、也不是引擎本体。

`audio_play_callback` 运行在 **libvlc 的音频输出线程**上，它对一个 `Gd<AudioStreamPlayer>` 做了
解引用，而那个对象已经被引擎释放：gdext 的 `ensure_object_alive` 断言失败抛出 Rust panic，
但这个 panic 发生在 `extern "C"` 回调里 —— Rust 不允许 unwind 穿过 FFI 边界 ——
于是变成 `thread caused non-unwinding panic. aborting.`，进程 `SIGABRT`，退出码 **134**。

三条 `play_basic` 错误和那条 abort 是**同一个窗口的前后两半**：先"节点已离树、还没释放"（`play()`
被引擎拒绝，刷 ERROR），后"节点已释放、libvlc 还在回调"（解引用 → panic）。

根因不是"抢那一瞬间"的竞态，而是**所有权顺序**：`AudioStreamPlayer` 是 `VLCMediaPlayer` 的
子节点，引擎在 `NOTIFICATION_PREDELETE` 里就把子节点删掉了，而 `Drop for VlcMediaPlayer`
（里面才调用 `libvlc_media_player_release`，真正停掉 libvlc 音频线程的那一步）跑在那之后。
所以"节点已死、libvlc 还在回调"这段窗口在结构上**必然存在**，不是偶发。

## 2. 先说清楚：这份日志不是当前源码编出来的

日志里嵌的源码路径是

```
/home/runner/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/godot-core-0.5.2/...
```

`/home/runner/` 是 GitHub Actions 的 Linux runner。逐提交核对本仓库锁定的 `godot-core` 版本：

| 提交 | Cargo.lock 里的 godot-core | 说明 |
| --- | --- | --- |
| `e74df32` | 0.5.2 | Migrating to gdext v0.5（Cargo.toml 写死 `"0.5.2"`） |
| `58ca796` | 0.5.2 | **v1.2.0** |
| `261e0ef` | 0.5.3 | building: simplify dependency version（改成 `"0.5"`） |
| `256e375` / `2c1064d` | 0.5.3 | |
| `263c664` | **0.5.5** | build(deps): bump the cargo-patches group with 3 updates |
| `df41943` | 0.5.5 | **release: 1.3.0** |
| `c99c639` / `eb4c02f` | 0.5.5 | 当前 master / HEAD |

也就是说：**崩掉的那份 `.so` 是 v1.2.0 及更早的构建**（0.5.2 那一代），不是这份工作区编出来的。

两点实际影响：

- 如果你手上有"Windows 不崩、Linux 崩"的对比，先确认两边**加载的是不是同一份 addon**。
  平台不同的两个二进制之间比不出平台差异。
- "升级到 1.3.0 就好了"**不成立**：第 4 节列出的缺陷在 1.3.0 和当前 master 里**一模一样地存在**。
  必须改代码重编。

## 3. 栈怎么读

- `class_runtime.rs:279` —— gdext 的 `ensure_object_alive`：`Gd` 的每次解引用都会先查一次
  "这个实例 ID 还活着吗"。**这个断言不在 `cfg(debug_assertions)` 里**，release 导出一照样崩。
- `AudioStreamPlayer::upcast_ref` —— 被解引用的那个类型，等价于"元凶是音频播放器句柄"。
- `panic in a function that cannot unwind` + `aborting` —— 前面那条 panic 抛出时正处在
  `extern "C"` 帧里，Rust 只能 abort。所以 134 不是引擎在崩，是 Rust 在自杀。

## 4. 元凶代码（修复前的 `src/vlc_media_player/audio_callbacks.rs`）

四条回调里**只有 flush 做了有效性检查**，另外三条直接解引用：

```rust
// 第 58-59 行：这一行的 is_playing() 就是 panic 落点
if !player.is_playing() {
    player.call_thread_safe("play", &[]);
}

// 第 66-69 行 / 73-78 行：pause / resume，同样无检查
player.set_stream_paused(true);   // / false

// 第 87-88 行：只有这里判了
if player.is_instance_valid() {
    player.call_thread_safe("stop", &[]);
```

`player` 来自 `libvlc_audio_set_callbacks` 的 opaque 指针，指向
`Box<(HeapProd<AudioFrame>, Gd<AudioStreamPlayer>)>` —— **一个 Godot 对象就这样被交给了 libvlc 的线程**。

线程归属是 libvlc 文档写明的（`thirdparty/vlc/win-x64/include/vlc/libvlc_media_player.h`）：

```
 * The LibVLC media player decodes and post-processes the audio signal
 * asynchronously (in an internal thread). Whenever audio samples are ready
 * to be queued to the output, this callback is invoked.
```

`call_thread_safe` 本身也救不了：它是**Node 的方法**，调用时同样要先解引用接收者
（gdext 生成代码里是 `self.__validated_obj()`），所以对象一旦被释放，`is_playing()` 和
`call_thread_safe(...)` 两处都会 panic。另外 `is_instance_valid()` 也不是安全屏障 ——
它是一次真实的引擎往返查询，语义正确，但"查询通过"和"下一行解引用"之间仍有一个几指令的窗口，
而 libvlc 的线程正持续往里撞（gdext 自己的文档就写着不要用它来判断能否安全访问）。

## 5. 完整时序（引擎侧已逐条核对）

引擎相关行为取自上游源码（`scene/main/node.cpp`、`scene/audio/audio_stream_player_internal.cpp`、
`servers/audio/audio_server.cpp`）：

**`Node::_notification(NOTIFICATION_PREDELETE)`** —— 子节点先死：

```cpp
			if (data.parent) {
				data.parent->remove_child(this);
			}

			// kill children as cleanly as possible
			while (data.children.size()) {
				Node *child = data.children.last()->value;
				memdelete(child);
			}
```

**扩展实例（也就是 Rust 结构体和 `Drop`）在这一步之后才释放** —— `NOTIFICATION_PREDELETE` 由
`Object::~Object()` 发出，而 GDExtension 实例的释放排在它之后。

**`AudioStreamPlayerInternal::notification`**：

```cpp
		case Node::NOTIFICATION_EXIT_TREE: {
			set_stream_paused(true);
		} break;
		...
		case Node::NOTIFICATION_PREDELETE: {
			for (Ref<AudioStreamPlayback> &playback : stream_playbacks) {
				AudioServer::get_singleton()->stop_playback_stream(playback);
			}
			stream_playbacks.clear();
		} break;
```

**`AudioServer::is_playback_active` 只在状态为 `PLAYING` 时为真**：

```cpp
	AudioStreamPlaybackListNode *playback_node = _find_playback_list_node(p_playback);
	if (!playback_node) {
		return false;
	}
	return playback_node->state.load() == AudioStreamPlaybackListNode::PLAYING;
```

于是（`AudioStreamPlayerInternal::is_playing()` 遍历 `stream_playbacks` 问 `is_playback_active`）：
**节点一离树就被置为 paused，`is_playing()` 立刻变 false。**

拼起来：

1. 应用把播放器节点移出场景树、或切场景把它 `queue_free()`。
2. libvlc 的音频线程照旧每来一块数据就回调一次。此刻 `AudioStreamPlayer` 已离树 → 引擎把它
   `set_stream_paused(true)` → `is_playing()` 为 false → 回调走进
   `if !player.is_playing()` → `call_thread_safe("play")` → 投递到主线程 → 主线程执行 `play()` →
   `play_basic` 里 `ERR_FAIL_COND_V_MSG(!node->is_inside_tree(), ...)` 拒绝 →
   **就是日志里那两行 ERROR**（两条说明这个窗口大约持续一到两个音频块）。
3. 帧末引擎 `memdelete` 这个节点：`Node::_notification(PREDELETE)` → 子节点先被
   `remove_child` 再被 `memdelete` —— **`AudioStreamPlayer` 到这里就没了**。
4. 但 `Drop for VlcMediaPlayer`（→ `libvlc_media_player_release`）**还没跑**，libvlc 的音频线程
   仍在回调。
5. 下一个回调执行 `player.is_playing()` → 句柄背后的实例已被释放 → `ensure_object_alive` 断言
   → panic → 不能 unwind → **abort，134**。`libvlc_media_player_release` 再也没有机会执行。

所以 `play_basic` 的刷屏其实是一个**有效的早期告警**：它在说"libvlc 还在跑，而宿主节点已经
不在场景树里了"，紧接着就是 use-after-free。不要为了日志干净去屏蔽它。

## 6. 当前源码里的修复（已实现）

原则：**让 libvlc 的线程一个 Godot 对象都别碰**。对节点要做的事从"调用"改成"数据"，
由主线程每帧取走 —— 和 `events.rs` 里 `EventPark` 已经在用的那套模式一致。

- `src/vlc_media_player/audio_callbacks.rs`：新增 `AudioShared`（环形缓冲生产者 +
  三个原子标志 + 主线程读好的 `mix_rate`），四条音频回调只往里写：
  `wants_play` / `wants_paused` / `wants_flush`。**这个模块现在一个引擎类型都不 import**
  （除 `native::AudioFrame`，那是 POD）。
- `src/vlc_media_player.rs`：新增 `service_audio_requests()`，在
  `on_notification(INTERNAL_PROCESS)` 里每帧调用 —— 也就是在主线程上，节点按定义还活着，
  "检查通过 → 下一行解引用"之间不存在别的线程在释放它。
- `wants_play` / `wants_flush` 做成**只有能执行时才消费**（`is_inside_tree()` 不成立就原地留着），
  于是"节点离树期间的 play"不再是引擎报错，请求会留到回到树的那一帧 —— 顺手消掉了
  `play_basic` 那类 ERROR 行。
- `wants_paused` 是**状态不是事件**（libvlc 的 pause/resume 只在状态翻转时回调，见头文件
  "The pause callback is never called if the audio is already paused"），所以最后一个写入者
  就是正确答案，两帧之间发生的翻转也不会丢。应用时**只在自己变化时才动**（主体上有一个
  `audio_paused` 镜像），因为 `stream_paused` 不是插件独占的属性 —— 引擎在树暂停 / 节点离树时
  会自己把它置真。另外 `play()` 之后立刻重新应用一次 ——
  引擎在"没有任何 playback 注册"时并不保存这个 bool
  （`AudioStreamPlayerInternal::set_stream_paused` 里明写着），不重新应用的话，
  在环形缓冲被取空期间到来的暂停会在重播时丢掉，声音会自己响起来。
- `audio_setup_callback` 不再跨线程调 `AudioServer::singleton()`：mix rate 在主体的 `init()`
  （主线程）读好存进 `AudioShared`，setup 回调通过 libvlc 交回的 `opaque`
  （"pointer to the data pointer passed to libvlc_audio_set_callbacks()"）取回。

没有改的东西：环形缓冲的生产/消费路径、`InternalAudioStream(Playback)`、音量/总线/mix target、
事件与信号路径。flush 原本那套 `stop()` + `clear_buffer()` 语义原样保留，只是搬到了主线程。

## 7. 影响面：这次改动会不会波及别的东西

逐条对照"改前 / 改后"的**可观测行为**，不是只看代码结构。

### 7.1 保持不变（语义等价）

| 行为 | 说明 |
| --- | --- |
| 环形缓冲的生产/消费 | 一行没动。生产者仍由 libvlc 线程写、`InternalAudioStreamPlayback::mix_rawptr` 仍由引擎音频线程读 |
| `flush` = 停止 + 清空缓冲 | 仍是 `stop()` 然后 `clear_buffer()`，只是换到主线程 |
| `play()` 的看门狗角色 | 保留。`is_playing()` 为 false 就重发 `play()`（环形缓冲被取空时引擎会把 playback 从列表移除，见 5 节末） |
| 视频路径、事件与信号、GPU D3D11 后端、音量/总线/mix target | 未改动 |
| `Cargo.toml` / `Cargo.lock` | 未改动 |
| 跨平台 | 改动里没有 `#[cfg]`，Windows / Linux / Android 走同一份代码 |

顺带一个**变干净**的地方：`AudioStreamPlayer` 原来在 `Box<(HeapProd, Gd<..>)>` 里还有一份 clone，
释放时机是"字段 drop 顺序"的一部分；现在这个 Box 里没有 Godot 对象了，少一个引用计数变量。

### 7.2 有差异，但方向是改善

- **`flush` 的顺序变对了**：改前 `stop` 是 `call_thread_safe`（延迟执行），而 `clear_buffer` 是立即执行
  —— 实际顺序是"先清后停"。现在两者在同一处按顺序执行。
- **起播不再被"离树"卡住**：请求只在能执行时才消费，回到树的那一帧就会被服务；改前要靠 VLC 线程
  下一个音频块重试。
- **暂停状态不再因欠载丢失**：`play()` 之后重新应用一次 libvlc 要的暂停状态。改前如果暂停事件正好
  落在"没有 playback 注册"的时刻，这个 bool 不会被引擎保存，声音会自己响起来。
- **`play_basic` 那类 ERROR 不再刷屏**（见 5 节）。

### 7.3 有差异，需要知情

- **最多 1 帧（≈16 ms）的执行延迟。** 改前 `call_thread_safe` 从 libvlc 线程投递、由主线程尽快执行；
  现在统一在每帧的 `INTERNAL_PROCESS` 里处理。对 pause/resume/flush 来说不可感知；
  重试 `play()` 的频率上限也从"音频块率"降到"帧率"，同量级。
- **`service_audio_requests` 依赖 `INTERNAL_PROCESS` 在跑**，而它受 `process_mode` 与
  `get_tree().paused` 影响：
  - 这是**有意对齐**的 —— 节点不处理时本来也不该由插件单方面推进音频（引擎在树暂停时自己会把
    `stream_paused` 置真）。
  - 但如果你把该节点的 `process_mode` 显式设成 `PROCESS_MODE_DISABLED`，**音频将不会自动起播**
    （改前能靠跨线程投递拉起）。要用 `PROCESS_MODE_ALWAYS`。
  - 同理，若你依赖"树暂停期间 VLC 继续解码、恢复后立刻有声"，现在是恢复后**第一帧**才起播。
- **`audio_setup_callback` 的采样率来源换了**：从跨线程读 `AudioServer::singleton()` 改成读
  主线程存下的 `mix_rate`，经 libvlc 交回的 `opaque` 取（头文件写明 "pointer to the data pointer
  passed to libvlc_audio_set_callbacks()"）。这是本次改动里**唯一依赖外部库契约**的地方，
  且**未实测**：如果契约不成立，`*rate` 会保留 libvlc 自己的建议值（`rate` 是 in/out 参数），
  采样率与 Godot 混音率不一致会表现为**音调/速度不对**。验证方法：播放一段有人声的素材听音调；
  或在 setup 回调里把 `*rate` 与 `AudioServer` 的 mix rate 一起打出来比对。
- **暂停状态改成"只在自己变化时应用"**：`stream_paused` 不是插件独占的属性，引擎会在
  `NOTIFICATION_PAUSED` / 离开场景树时自己置真。所以不能每帧拿它跟 libvlc 的期望值对齐
  （那样会把引擎的暂停撤销掉）。现在用一个 `audio_paused` 镜像，只有 libvlc 的要求变了才动它 ——
  这一点也与改前行为一致（改前只在 libvlc 的事件回调里设置）。

### 7.4 既有风险，本次**没有**引入也没有消除

- `clear_buffer()` 走 `bind_mut()` 拿 `InternalAudioStream` 与 `InternalAudioStreamPlayback` 的
  可变借用。如果此刻引擎音频线程正在 `mix_rawptr` 里持有同一个 playback 的借用，gdext 的借用
  检查会 panic。改前这段代码在 libvlc 线程上跑（同样有这个风险），现在改到主线程、触发点变成
  "flush 请求被服务的那一帧"。flush 本身很罕见，但这条是真实的残余风险 —— gdext 0.5.5 没有
  `try_bind_mut()`，想彻底消掉得让 playback 自己在 `mix_rawptr` 里处理清空请求。
- `AudioShared` 是 `Box`，地址交给了 libvlc。正常路径下 `Drop` 里先 `release`（同步销毁 aout、
  join 线程）、字段之后才释放，顺序正确；若 libvlc 在 `release` 返回后仍回调，就是写到已释放内存。
  这是库侧契约问题，插件无法用"再加一个守卫"解决（详见 8 节末的 `Arc` 方案）。
- 环形缓冲被取空 → 引擎把 playback FADE_OUT 并移出列表，于是 `play()` 必须反复调用。
  这是既有设计，本次只是把它搬到了主线程，没有改变频率量级。

## 8. 验证状态

| 项目 | 状态 |
| --- | --- |
| 崩溃行、崩溃线程（libvlc 音频输出线程） | **已确认**：日志 + 回调注册处的 opaque 指针 + libvlc 头文件说明 |
| 崩掉的二进制是 0.5.2 世代（≤ v1.2.0） | **已确认**：日志内嵌路径 + 逐提交核对 Cargo.lock |
| 子节点先于扩展实例释放；`is_playing()` 在离树后为 false | **已确认**（上游源码原文，见第 5 节），未在本机跑起来实证 |
| 修复可编译 / 过 clippy / 过 fmt | **已确认**：`cargo check --all-targets`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo fmt --all -- --check` 均通过（Windows，本地） |
| 修复后 Linux 上不再崩 | **未验证**：本机是 Windows，且没有 Linux 侧的 LibVLC 运行时。需要按第 8 节复现 |
| 修复后音频听感（起播、暂停、seek 后不播旧数据） | **未验证**：需要在有声卡的环境上听 |

残余风险（本次未处理，属于既有设计）：`AudioShared` 是一个 `Box`，它的地址被交给了 libvlc。
正常路径下 `Drop` 先 `libvlc_media_player_release()`（同步销毁 aout、join 其线程）、之后这个
`Box` 才随结构体字段释放，顺序是对的。但如果 libvlc 在 `release` 返回之后仍回调（不该发生），
回调会写到已释放的内存上 —— 那是 libvlc 侧的契约问题，插件这边无法用"再加一个守卫"解决，
只能靠"外部库线程先停、再拆 Godot 对象"这个顺序。若之后仍观测到，可考虑把 `AudioShared`
改成 `Arc`，`Drop` 时 `Arc::into_raw` 故意泄漏一份引用（换取"永远可写但无人读"的内存），
用几 KB 换确定性。

## 9. 现场怎么确认 / 怎么复现

1. 先看你 Linux 项目实际加载的是哪个二进制：

   ```bash
   strings addons/godot_vlc/bin/linux-x64/libgodot_vlc.so | grep -o 'godot-core-[0-9.]*' | sort -u
   ```

   出现 `godot-core-0.5.2` 就是第 2 节说的旧构建；重编后再复现才有意义。
2. 打开 backtrace 再跑一次：`RUST_BACKTRACE=1 ./your_game`（gdext 会打 `[panic backtrace]`，
   外部库的帧名可读，能进一步确认是 libvlc 自己创建的线程）。
3. 最小复现条件只有两条：**素材有音轨**（没有音轨 libvlc 不建 audio output，音频回调根本不触发），
   以及**在 libvlc 正在播放时把播放器节点移出场景树 / 释放**。把这两条做成一个独立工程，
   用 `mode` 参数分别跑"直接 free / 先 remove_child 再 free / 先 stop 再 free / 常驻不释放"，
   一次跑批就能得到对照表。
4. 判定"崩没崩"时别用宽松正则匹配 `after it has been freed` —— 复现器自己的横幅里若出现同样的
   字样会被算成崩溃。另外"没崩"必须带上跑了多少轮/多少秒的样本量，否则不算结论。
