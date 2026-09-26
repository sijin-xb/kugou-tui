# 维护与排障手册

**什么时候读这份**：要改播放/下载/缓存这条线的时候；用户报「内存越用越大」「进度条
跳回开头」「放着放着跳歌」「缓存里有半首歌」的时候；或者隔了很久回来接手这个项目、
想先弄清模块都在哪的时候。

它记的是**踩过的坑和验证手段**，不是功能说明——功能看
[README](../README.md) 与 [docs/USER_GUIDE.md](USER_GUIDE.md)，设计取舍看
[docs/DESIGN.md](DESIGN.md)。

---

## 1. 地图

### 1.1 目录

```
src/
├─ main.rs            进程入口：解析 CLI → 装配 App → 跑主循环 → 退出
├─ cli.rs             命令行参数（优先级：CLI > 环境变量 > 配置文件 > 默认值）
├─ config.rs          配置读写、路径（~/.config/kugou-tui/、~/.cache/kugou-tui/）
├─ event.rs           全进程唯一的 EventBus 与 Loaded 事件枚举
├─ keymap.rs          按键 → 语义动作（Action）
├─ logger.rs          极简文件日志（`tlog!`），DEBUG 需 KUGOU_TUI_DEBUG=1
├─ error.rs           AppError 与「重试不重试」的判据
├─ mpris.rs           桌面集成：MPRIS（playerctl / DMS 等）
├─ tray.rs            系统托盘
├─ api/               接口层（只跟 KuGouMusicApi 说话）
│  ├─ client.rs       带重试的 HTTP；AppError 归类在这里
│  ├─ catalog.rs      搜索 / 歌单 / 榜单 / 歌手 / 取播放直链
│  ├─ cloud.rs        账号、歌单同步、VIP
│  ├─ lyric.rs        KRC 逐字歌词解析
│  └─ model.rs        Song 等数据模型 + 字段容错解析
├─ audio/             音频子系统（本文重点）
│  ├─ engine.rs       独占音频线程的播放引擎
│  ├─ streaming.rs    边下边播的字节缓冲（实现 Read + Seek）
│  ├─ download.rs     下载：流式 / 整首落盘
│  ├─ cache.rs        磁盘缓存与容量回收
│  ├─ levels.rs       电平采集（环形缓冲 + 原子量）
│  └─ spectrum.rs     FFT 频谱
├─ source/            音源抽象（酷狗 / 酷狗概念版 / 网易云）
├─ app/               状态机（主线程独占）
│  ├─ mod.rs          App 装配、主循环、退出
│  ├─ state.rs        AppState：界面的唯一真相
│  ├─ update.rs       事件 → 状态变更（全程序唯一改状态的地方）
│  ├─ queue.rs        播放队列与播放模式
│  └─ session.rs      会话持久化（上次听到哪）
└─ ui/                ratatui 渲染（只读 state，不改）
```

### 1.2 线程与所有权

```
input thread ──┐
               ├─→ EventBus ─→ main loop(update.rs) ─→ draw
async tasks ───┘                      │
                                      └─→ AppState（主线程独占，无锁）
audio thread(kugou-audio) ───────────→ 原子量（位置/时长/音量/状态）+ EventBus
```

三条铁律：

1. **只有主线程改 `AppState`**，所以状态没有锁。别在异步任务里碰 `AppState`。
2. 异步任务的产物一律**回到主线程**再由 `update.rs` 消费（`bus.emit(Loaded::…)`）。
3. 音频线程与主线程之间**高频数据走原子量**（位置/音量/电平），**离散事件走 EventBus**
   （装载完成、曲目结束、错误）。

### 1.3 播放一首歌的数据流

```
用户 Enter
  └→ App::start_playback(song, start_at_ms)          update.rs
       ├→ 取消上一首还在跑的流式下载（active_stream.cancel()）
       ├→ audio.mark_loading()                       状态立刻变「缓冲中」
       ├→ load_cover() / request_lyric()
       └→ request_stream()
            ├─ cache.find(key) 命中 ──→ audio.load(AudioSource::File(path), 位置)
            └─ 未命中 → 取直链（/song/url，多候选 (hash, 音质) 逐个试）
                 └→ Loaded::StreamReady
                      └→ App::start_download()
                           ├─ start_at_ms > 0 → fetch_to() 整首下完 → StreamCached → load(File)
                           └─ start_at_ms = 0 → start_streaming()（写 .part + 灌缓冲）
                                └→ 攒够 128 KiB → Loaded::StreamPrerolled
                                     └→ audio.load(AudioSource::Stream(buffer))
                                └→ 整首下完 → 改名 .part → Loaded::StreamCompleted（只做收尾）
```

### 1.4 缓存文件命名

`{hash}-{quality}.{ext}`，例如 `b3a52a7a…-128.mp3`；音质不同互不覆盖。
**未下完的是 `同目录同名 + .part`**，`cache.find` 看不到它（这是故意的，见 §3.3）。

### 1.5 打开歌单（两段式加载）

```
打开歌单
  ├→ first_screen_page()          ← 决定先取哪一页（与 sort_descending 一致）
  │     └→ 取一页 → Loaded::PlaylistTracks → set_songs_sorted(…, descending)
  │           （倒序显示时 `set_songs_sorted` 会把这一页 reverse 一遍）
  ├→ needs_full_fetch()           ← 还要不要补齐整表
  └→ 后台并发翻完所有页 → Loaded::PlaylistTracks（整表覆盖）
```

两条规则值得记住：

* **首屏取的页必须与显示方向一致**。默认倒序 → 出现在最上面的是最后一页的内容，
  所以首屏就得取末页；否则用户先看到的是一屏列表中部的歌，几秒后整表到位时画面
  整体翻一次。
* **判"还有没有更多"不能只看这一页满不满**（见 §5「列表分页」）。

---

## 2. 跑起来 / 怎么验

```bash
cargo test                         # 单元测试（约 280 条，秒级）
cargo clippy --all-targets         # 发版脚本会跑
cargo build --release              # 二进制约 7 MB
./target/release/kugou-tui --print-config   # 看一眼生效配置、日志路径、缓存目录
```

**冒烟测试**（本机有真实声卡与 API 服务时）：

```bash
# 1) 起接口服务（首次需要 ./scripts/kugou-api-install）
./scripts/kugou-api              # 或 launchd/systemd 里的那套

# 2) 干净缓存下放一首歌，看日志
KUGOU_TUI_DEBUG=1 ./target/release/kugou-tui --search "周杰伦 晴天" --no-tray \
    --cache-dir /tmp/smoke-cache
# 进去后：Esc → Enter 播放 → n 切歌 → Space 暂停/继续 → q 退出
# 日志：~/.cache/kugou-tui/kugou-tui.log
```

想**隔离**自己的实验、不碰真实配置与会话，用 XDG 变量换个家：

```bash
XDG_CONFIG_HOME=/tmp/xdg/config XDG_CACHE_HOME=/tmp/xdg/cache \
    ./target/release/kugou-tui --print-config
```

> 为什么要这么做：`session.json`（上次听到哪、队列）在
> `XDG_CACHE_HOME/kugou-tui/` 里，会**在退出时被覆写**。拿真实配置跑自动化测试，
> 会把用户的队列游标和播放位置改掉。要么隔离，要么先备份
> `~/.cache/kugou-tui/session.json`。

---

## 3. 两类疑难问题的排查手册

### 3.1 内存只涨不跌

**现象**：连着听、连着切歌之后 RSS 破一百 MB，停下来也不回落。

**先量，别猜**。

```bash
pid=$(pgrep -f 'target/release/kugou-tui' | head -1)
grep -E 'VmRSS|VmHWM|Threads' /proc/$pid/status    # 当前 / 峰值 / 线程数
```

循环采样（看趋势，不看单点）：

```bash
while :; do grep VmRSS /proc/$pid/status; sleep 1; done
```

**这类问题的两个历史根因**（都还在别处可能出现，改之前先确认这里没重犯）：

1. **无上限的缓冲**。`StreamingBuffer` 曾经把整首下到的字节都留在内存里
   （而且是 `Vec` 倍增过的整首），放一首 64 MiB 的 Hi-Res 就是 66.5 MiB。
   现在只留 4 MiB 尾部窗口，超出的从落盘 `.part` `pread` 回来。
   → **要点**：任何"会随媒体体积增长"的内存结构都要问一句「它的上限是什么」。
2. **没人取消的后台任务**。切歌不取消上一首的流式下载，用户每按一次 `n` 就多一条
   在跑的下载把整首往内存灌——实测连切 5 首冲到 323.7 MiB。现在 `App::active_stream`
   里握着当前流的句柄，`start_playback` / `stop_playback` / `shutdown` 都会 `cancel()`。
   → **要点**：新建后台任务时，问「谁负责让它停下来」。

**定位手法**（按性价比排序）：

| 手段 | 能看出什么 |
|---|---|
| 采样 `VmRSS`，按「时间段 + 用户动作」对齐 | 涨是发生在播放中、切歌时、还是空闲时 |
| `VmHWM` 与当前 RSS 的差 | 有没有一次性的大峰值（比如整首下完那一刻） |
| `Threads:` 计数 | 线程/任务有没有泄漏（正常 7–9） |
| `KUGOU_TUI_DEBUG=1` 看 `预取` / `缓存回收` / `流式下载` 日志 | 是哪条链在跑、跑完没有 |
| 对照实验：把 `MAX_KEPT_BYTES` 临时改成 64 KiB | 如果 RSS 跟着掉，说明大头就是那块缓冲 |

**别做的事**：定时 `malloc_trim`、周期性 `shrink_to_fit`、强制 GC 式的"回收线程"。
那些只是把增长盖住，峰值和上限还在。要改就改**上限与所有权**。

### 3.2 播放进度突然回到开头 / 放着放着跳歌

**现象**：边下边播时缓冲一下，然后从头再放一遍；或者听着听着跳下一首；或者十几秒
一循环。

**先分清是"播完了"还是"断了"**。这两件事在播放层长得一模一样，因为：

> `rodio` 的解码器把**任何**读错误都吞成 EOF
> （`symphonia` 的 `format.next_packet().ok()?`），源"结束"既可能是真播完，
> 也可能是流断了。

所以 `engine::Runtime::sync()` 在播放器变空时**不能直接当播完**，要按住那条流的
状态分档（`classify_drain`）：

| 流的真实状态 | 收场 | 用户看到 |
|---|---|---|
| `is_cancelled()` | 静默 | 切歌了，什么都不该弹 |
| 完整下完 | `TrackFinished` | 正常切下一首 |
| 没下完 + 有 `error` | `Failed(原因)` | 报错、停下，不切歌 |
| 没下完 + 没出错（读超时） | `StreamInterrupted { 位置 }` | 停在原处，自动从该位置续播一次 |

**历史上这一族问题的两个根因**：

1. **下完之后重新装载**（`Loaded::StreamCached` 被当成"该播放了"）。歌已经在放，
   重新 `audio.load()` 等于把位置冲回 0。现在流式那条路走 `Loaded::StreamCompleted`，
   只做收尾、不碰播放器。
2. **断流被当成播完**。单曲循环（或队列里只有一首）下，切歌就是"同一首从 0 再放"。
   现在由上面的分档接管；**读超时**那条还会保留位置、自动兜一次。

**还有一个必须知道的细节**：位置要在**曲目结束之前**就记下来。

```rust
// 结束那一帧不能读 get_pos()：源已经没了，rodio 报 0
let position_ms = self.last_position_ms;   // 上一帧还在播时的值
```

不这么做，进度条会在结束瞬间跳回 00:00，之后"续播"也没位置可续。

**复现手法**（真机、可重复）：用一个"掐流"代理把 CDN 的数据在中途断掉 20–30 秒，
剩下交给真实的解码与播放。要点：

1. 音质选小的（`quality = "128"`，一首 4 MB），下载才来得及在播放中途"下完"或"断掉"；
2. 播放模式选 `repeat_one`——它把"从头再放一遍"放大成肉眼可见的现象（顺序播放只会
   表现为"跳过这首歌"）；
3. 观测**用应用自己的状态**（退出时写下的 `session.json` 里的 `position_ms`、
   或日志里的「会话已保存：N 首，位置 X ms」），不要用 MPRIS 的 `Position`：
   **播放器状态是 Stopped 时 `playerctl position` 一律报 0**，那不是真相，会把你带沟里。

**另一个坑**：半首文件。流式下载如果直接写正式缓存文件名，失败/取消后磁盘上就留一个
"看起来完整"的短文件，`cache.find` 只看"文件在不在"，下次播放命中它 → 放到一半 EOF →
被判为播完 → 单曲循环又从头 → 十几秒一循环。**所以`.part` + 下完才改名**这条不能省。

---

## 4. 改这块代码时的不变量

改 `audio/` 下任何东西之前，先确认这些还没被破坏：

1. **先落盘、再 `push`**。缓冲只留窗口，窗口外的字节只能从 `.part` 读回来；
   顺序反了就会读到还没写入的空洞（听感是噪音）。
2. **落盘文件必须是读写打开的**（`OpenOptions::read(true).write(true)`）。
   `File::create` 只给 `O_WRONLY`，对这种句柄 `pread` 直接 `EBADF`。踩过一次。
3. **读位置与写状态要分清**：`is_finished()`（任务收工）≠ `is_complete()`（整首完好）。
   播放引擎靠这个区分"放完了"和"下载失败"。
4. **错误只在真读不到的时候抛**。已经下到的部分照常能播——不该因为后半段失败把用户
   已经听到的也掐掉。
5. **取消要能唤醒阻塞中的读者**。`read()` 阻塞在条件变量上；`cancel()` 必须置标志位
   并 `notify_all`，否则切歌要等下一次读超时（最多 15 秒）才放开，而音频线程正在
   `Player::clear()` 里等它。
6. **`.part` 不是缓存**。回收与"清空缓存"都要跳过它（`cache::is_partial`）。
7. **别在结束后读 `get_pos()`**（见 §3.2）。
8. **结束/失败/取消都要把句柄放掉**：`active_stream`（App）、`Runtime::stream`（引擎）。
   留着一份就会拖住整条缓冲与文件句柄。
9. **不要为了修内存去改 `WAIT_TIMEOUT`**。15 秒是"网络真断了要能报错"的兜底；
   调大只是让卡死更久，调小会误判正常等待。
10. **`Output` 的字段顺序不能动**（播放器必须排在设备前面），也别删那个
    `_stream` 字段——它靠生命周期起作用，删了声音立刻断。

---

## 5. 常见陷阱清单

**播放/rodio**

- 解码器的读错误 = EOF，没有异常能向上冒。任何"流断了"的判断都得自己看缓冲状态。
- `Player::stop()` 之后 `get_pos()` 会归零（`periodic_access` 里显式写了 0）；
  `Player::clear()` 还会顺手 `pause()`。
- `Player::clear()` 会等当前的源退出（`sleep_until_end`）。源要是永久阻塞在
  `read()` 里，音频线程就一起卡住——所以缓冲的读超时必须存在。
- `LevelMeter` 这层包装必须转发 `try_seek`，否则整条音频变成不可跳转，
  而播放本身完全正常（极难察觉）。
- 队列里只有一首 + 单曲循环/列表循环时，"播完"和"从头再播"看起来一样；
  调试时容易把 bug 当成特性。

**下载/缓存**

- `cache.find` 只按"文件在不在"判断，不校验长度——这就是 `.part` 存在的理由。
- `reqwest` 的 `chunk_stream` 与并发 Range 都要带正确的 `Range`；自己写代理做实验时
  别忘了透传它，否则 4 个分块会各拿到整首、拼出一个坏文件。
- 分块下载失败会**退回单连接**重下一次（`fetch_to`），所以"看起来下了两遍"可能是正常的。
- 缓存回收是同步目录扫描，必须在 `spawn_blocking` 里跑，别卡住 UI。

**列表分页**

- `pagesize` 在歌单类接口上是**硬上限 30**，响应里的 `count` 只是当页条数、**不是总数**；
  要算总页数只能用歌单元数据里的 `song_count`。
- 「这一页不满 ⇒ 这就是全部」这个判据**只在第 1 页成立**。首屏改从末页取之后
  （倒序显示时），末页不满是常态——照搬会让"整表补齐"永远不出门，歌单只显示末页
  那几十首。判据抽在 `app::update::needs_full_fetch`，有测试钉住。
- 取哪一页由 `app::update::first_screen_page` 决定：**必须与 `sort_descending` 一致**，
  否则首屏内容与最终列表的头部对不上，画面会整体翻一次（用户能直接看出来）。
- 首屏取到空页（`song_count` 过期）要退回第 1 页，不能让用户对着空列表干等。

**进程/环境**

- 日志默认不记 DEBUG；排查前 `KUGOU_TUI_DEBUG=1`。
- `session.json` 退出时会被整体覆写。自动化测试要么隔离 `XDG_CACHE_HOME`，
  要么先备份。
- MPRIS 的 `Position` 在 `Stopped` 时无意义（`playerctl` 报 0）。
- 同一台机器上跑两个 kugou-tui，后启动的那个抢不到
  `org.mpris.MediaPlayer2.kugou-tui`，别拿 `playerctl` 的读数当唯一证据。
- ALSA 会把插件（`lavrate`/`jack`/`oss`…）都报成设备；设备枚举已经做了两轮过滤
  （能给出默认输出配置 + 排除 `null`），别退回去"全部列出"。
- 概念版（lite）与标准版的 `ppage_id` 处理不同，**取直链时不要自己编 `ppage_id`**
  （详见 `api/catalog.rs` 的注释：编错会直接 31863，表现是"所有歌都听不了"）。

**测试**

- 单元测试跑在同一个进程里，**别用 `VmRSS` 做断言**（并行测试、分配器缓存都会干扰）。
  要断言"内存有上限"就断言**窗口长度**（见
  `streaming::tests::keeps_only_a_bounded_window_in_memory`），
  RSS 级的对照放到进程外的实验里做（§6）。
- 落盘相关的测试要按**读写**打开临时文件（同 §4.2），否则测试会以 `EBADF` 失败。

---

## 6. 内存对照实验（改完缓冲/下载后跑一遍）

思路：把**同一份** `streaming.rs` 的新旧两版分别编译进一个小程序，跑同一套负载，
读 `/proc/self/status` 的 `VmRSS` / `VmHWM`。仓库外的临时工程，不污染仓库：

```bash
mkdir -p /tmp/mem-repro/src && cd /home/xibie/kugou-tui
git show HEAD:src/audio/streaming.rs > /tmp/mem-repro/src/old.rs
cp src/audio/streaming.rs            > /tmp/mem-repro/src/new.rs
# main.rs 里 #[path = "old.rs"] mod old; #[path = "new.rs"] mod new_;
# 各跑三种负载：单曲 / 连切 N 首 / 不取消的连切
```

`streaming.rs` 只依赖 `std`，所以可以直接 `#[path]` 引进来跑——这也是把它写成
"没有 crate 内依赖"的一个额外好处。

**2026-09 的基线数据**（单曲 64 MiB，分块 64 KiB，release，CachyOS）：

| 负载 | 修复前 | 修复后 |
|---|---|---|
| 单曲播完（峰值 RSS） | 66.5 MiB | 12.5 MiB |
| 连切 5 首、都不取消 | 323.7 MiB | 3.0 MiB（取消后）/ 53 MiB（不取消） |
| 连播 24 首 | 66.6 MiB（恒定，不随首数涨） | 12.5 MiB（恒定） |

**真进程**的对照（`--search` + pty 里按键 + 采样 `/proc/<pid>/status`）：
同样 6 次切歌，修复前 85 MiB 且单调上涨，修复后 25 MiB 并稳定；12 次切歌时
修复前峰值 **175.8 MiB**、退出时仍 145.7 MiB（不回落），修复后峰值 **35.3 MiB**、
退出时 30.1 MiB。

判断"有没有持续泄漏"的方法：**跑够多的首歌（≥20）再看 RSS 是否收敛到同一个值**。
修复后连播 24 首 RSS 稳定在 12.5 MiB 不动，就说明每条流的生命周期都收干净了。
