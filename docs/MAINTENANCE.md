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
├─ config.rs          配置读写、路径（Linux: ~/.config/kugou-tui/、~/.cache/kugou-tui/；
│                     Windows: %APPDATA%\\kugou-tui\\、%LOCALAPPDATA%\\kugou-tui\\）
├─ event.rs           全进程唯一的 EventBus 与 Loaded 事件枚举
├─ keymap.rs          按键 → 语义动作（Action）
├─ logger.rs          极简文件日志（`tlog!`），DEBUG 需 KUGOU_TUI_DEBUG=1
├─ error.rs           AppError 与「重试不重试」的判据
├─ mpris.rs           桌面集成：MPRIS（playerctl / DMS 等）——**仅 Unix**，见下
├─ tray.rs            系统托盘——**仅 Unix**，同上
├─ ws.rs              WebSocket 服务：状态 / 歌词推送 + 遥控（仅 127.0.0.1）
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
│  ├─ update.rs       事件 → 状态变更（分派 + 加载层 + 结果处理，见 §1.8）
│  ├─ navigation.rs   导航：切页 / 焦点 / 选择移动 / 数字键（从 update.rs 切出）
│  ├─ playback.rs     播放控制：起播 / 切歌 / 进度 / 音量 / 静音 / 歌词偏移
│  ├─ search.rs       搜索：提交与分页追加
│  ├─ cloud.rs        登录 / 音源切换 / VIP / 云端歌单 / 账号资料
│  ├─ settings.rs     设置页：候选值 + 显示文本 + 三个交互（移动 / 点击 / 改值）
│  ├─ desktop.rs      MPRIS / 系统托盘 / 窗口控制 / WebSocket 快照
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
ws thread(ws.rs) ────────────────────→ 自己的 current_thread runtime（见 1.9）
```

三条铁律：

1. **只有主线程改 `AppState`**，所以状态没有锁。别在异步任务里碰 `AppState`。
2. 异步任务的产物一律**回到主线程**再由 `update.rs` 消费（`bus.emit(Loaded::…)`）。
3. 音频线程与主线程之间**高频数据走原子量**（位置/音量/电平），**离散事件走 EventBus**
   （装载完成、曲目结束、错误）。

WebSocket 线程是这三条的一个例外写法，但**不破坏**它们：它读的是主循环每拍写进去的
`watch` 快照（只读），入站命令则经 `EventBus` 回到主线程——和异步任务同一条路径。

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

未下完的是**同目录同名 + 一个 `.part` 后缀**，两种写法都有，`cache.find` 都看不到
（这是故意的，见 §3.3）：

| 写法 | 谁用 |
|---|---|
| `abc-128.mp3.part` | 整首下载（`fetch_to`：续播、预取下一首） |
| `abc-128.mp3.stream.part` | 边下边播（`start_streaming`） |

**两个写法刻意不同**：同一首歌上可以同时跑一条整首下载（后台预取）和一条流式下载
（用户正好切到这首）。共用一个 `.part` 的话，整首下载那次 `File::create` 会把流式
那条已经落盘的字节截掉，而缓冲窗口之外的数据正是靠 `pread` 这个文件读回来的——
读回空洞就是噪音。两者仍然都以 `.part` 结尾，所以 `cache::is_partial` 照样认得。

同一目录下还有脚本用的运行态文件，命名是**两个平台之间的约定**，改一处就要改另一处：

| 文件 | 内容 | 谁写 / 谁读 |
|---|---|---|
| `api-<实例>.pid` | `<PID> <端口>`（空格分隔） | 启动器与 `kugou-api` 写；`kugou-api stop/status` 读 |
| `api-<实例>.log` | 服务的 stdout（Windows 另有 `.log.err`） | 启动器与 `kugou-api` 写；`kugou-api logs` 读 |

实例名只有两个：`standard`（标准版 / 网易云）与 `lite`（酷狗概念版）。
**为什么 PID 文件要带端口**：只存 PID 时，改了端口没重启的旧进程会让 `status`
拿新端口报一个「运行中」，而它其实监听在旧端口上——实测踩到过（见 `kugou-api`
里 `pid_of` 的注释）。

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

### 1.6 平台分支在哪

代码里 `cfg` 一共没几处，但每一处都对应一个真实差异，改动时容易漏。清单如下：

| 位置 | 差异 | 为什么不能统一 |
|---|---|---|
| `Cargo.toml` 的 `[target.'cfg(windows)']` / `[target.'cfg(unix)']` | TLS 后端（SChannel / rustls）、`zbus`、`libc`、`windows-sys` | Windows 用 rustls 会拖进 `aws-lc-sys`，那要额外装 CMake + NASM；D-Bus 在 Windows 上不存在 |
| `audio/streaming.rs` | `read_at` 包了一层：Unix `pread` / Windows `seek_read` | Windows 没有 `pread` |
| `logger.rs` | stderr 重定向：Unix `dup2` / Windows `SetStdHandle` | Windows 没有 fd 表 |
| `main.rs`、`app/mod.rs`、`app/update.rs` | `mpris` / `tray` 模块、字段、同步逻辑 | 见上（D-Bus） |
| `config.rs`、`app/settings.rs` | 路径都走 `dirs`，只有 `~` 展开要额外兜一层 | Windows 上没有 `HOME` |
| `ui/icons.rs` | Nerd Font 探测走 `fc-list`，Windows 上没有 → 回落 ASCII + `KUGOU_TUI_NERD_FONT` 覆盖 | 那边字体清单在注册表里 |
| `api/model.rs`、`event.rs`、`keymap.rs` | 三处 `#[cfg_attr(not(unix), allow(dead_code))]` | 那些项只由 MPRIS 构造；留 `allow` 而不是 cfg 掉，是为了让枚举/模型在两边形状一致，下游 `match` 不用长平台分支 |
| `scripts/*`（bash） | 不写平台分支，改成**只用两边都有的写法** | 脚本在 Linux 与 macOS 上跑同一份，逐条差异见 §1.7 |
| `scripts/*.ps1` | `$env:OS -eq 'Windows_NT'` 判断（`-WindowStyle` 等只在 Windows 存在） | `$IsWindows` 是 PowerShell 6+ 的自动变量，而 Windows 自带的是 5.1 |

验证 Windows 侧**不需要 Windows 机器**（编译期能查的部分）：

```bash
rustup target add x86_64-pc-windows-msvc
cargo check   --target x86_64-pc-windows-msvc --all-targets
cargo clippy  --target x86_64-pc-windows-msvc --all-targets -- -D warnings
```

`--all-targets` 会把测试代码也过一遍。**最后一步链接需要 Windows 或 MSVC 工具链**，
Linux 上跑到 `error: linker link.exe not found` 就说明 Rust 侧全部通过了
（依赖与自身都编完了，只差链接）。

运行时的部分由 `.github/workflows/ci.yml` 兜住：**Linux / Windows / macOS 三栏**
各跑一遍 `clippy -D warnings + test`。**别把它删了**——路径展开、配置目录、缓存文件
命名这些差异只有真跑起来才露出来。macOS 那栏的意义也是这个：它和 Linux 共用
`cfg(unix)` 分支，但 `dirs` 给的是 `~/Library/...`、音频走 CoreAudio、拿不到
`fc-list`，不跑就只是「理论可用」。

这三栏**只做验证**：不跑 `cargo build --release`、不打发行包、不传 artifact。三平台
发行资产由 `release.yml` 在推 tag 时产出，理由见 `ci.yml` 文件头的注释。

### 1.7 启动器脚本：跨平台约定

`scripts/` 下的 bash 脚本在 Linux 与 macOS 上跑**同一份**，PowerShell 那几份只在
Windows 上有意义（但发行 zip 里也带上 bash 那几份，Git Bash / WSL 下要用）。

| Windows | Unix 侧 | 说明 |
|---|---|---|
| `kugou-tui.ps1` | `kugou-tui` | 启动器。**刻意不写 `param()` 块**，否则 PowerShell 会把 `-s 海阔天空` 当成写错的参数名；参数全部经 `$args` 原样透传。带 `--dry-run` 只打印决策 |
| `kugou-api-install.ps1` | `kugou-api-install` | 只 clone + 装依赖，**不启动**——起服务交给启动器，避免两处各写一份 |
| `kugou-api.ps1` | `kugou-api` | 启停管理（`start` / `stop` / `restart` / `status` / `logs`）。与启动器共用 PID 文件，见 §1.4 |
| `build-windows.ps1` | `make-release-tarball` | 构建 + 打包 zip。**包里带哪几份脚本以「文档提到过的」为准**，别只按平台筛——0.4.3 那版就漏掉了 `.ps1`，而文档让用户跑的正是不存在的那几个 |

#### bash 侧：不许用的写法

macOS 自带的是 BSD 工具链加 **bash 3.2**，下面每一条都会让脚本直接中断（不是
「行为略有不同」）。三个脚本各自独立分发（AUR 装进 `/usr/bin`、发行包放进
`scripts/`、也有人单独软链），所以**没有共享库**：这段限制在每个脚本里重复声明一遍，
改的时候三处一起改。

| 不能用 | 会怎样 | 改成 |
|---|---|---|
| `readlink -f` | macOS 报 `illegal option -- f` | 逐层解软链（见各脚本的 `self_path` / `self_dir`） |
| `dirname --` | BSD 把 `--` 当成参数本身 | 去掉 `--` |
| `seq` | GNU coreutils，BSD 环境没有 | `while [ "$i" -lt N ]` 计数循环 |
| `setsid` | util-linux 专有 | `nohup`（仍在同一会话，但足以活过终端） |
| `ss` | iproute2，macOS 没有 | `lsof -nP -iTCP:<端口> -sTCP:LISTEN -t` |
| `declare -A` | bash 3.2 报 `declare: -A: invalid option` | `case` 分派 + `${!name}` 间接展开 |
| `"${arr[@]}"`（空数组） | bash 3.2 + `set -u` 报 unbound variable | `"${arr[@]+"${arr[@]}"}"` |

**目录必须与 `dirs` crate 对齐**，这不是风格问题：

| | Linux | macOS |
|---|---|---|
| 配置根 | `$XDG_CONFIG_HOME` / `~/.config` | `~/Library/Application Support` |
| 缓存根 | `$XDG_CACHE_HOME` / `~/.cache` | `~/Library/Caches` |

macOS 上 `dirs` **不读** XDG 变量，所以脚本也不能读。不对齐的后果很实在：启动器会去
`~/.config` 找一个永远不存在的 `config.toml`，于是永远按默认值（`:3000`、标准版）探活，
概念版用户只会看到「服务启动失败」。配置根可以用 `KUGOU_TUI_CONFIG_DIR` 整体覆盖
（三个脚本 + 两个 ps1 都认它，语义与 `config.rs` 的 `config_root()` 一致）。

#### PowerShell 侧：四条注意

* **只用 PowerShell 5.1 的语法**（Windows 自带的就是它）。别用 `$IsWindows`
  （6.0 才有）、`Start-Process -Environment`（7.4 才有）。平台判断统一用
  `$env:OS -eq 'Windows_NT'`。
* **文件必须带 UTF-8 BOM**，而且**只能靠手工保证**——CI 发现不了。
  5.1 在文件没有 BOM 时按当前 ANSI 代码页（中文 Windows 上是 GBK）解码 `.ps1`，
  而这几个脚本里有几十条面向用户的中文提示，全部会变成乱码。PowerShell 7
  默认按 UTF-8 读，而 CI 用的正是 `shell: pwsh`（7.x），所以**绿了也不代表
  用户那边正常**。新增或改写 `.ps1` 之后，用
  `head -c 3 scripts/*.ps1 | od -An -tx1` 确认每个文件都以 `ef bb bf` 开头；
  行尾保持 LF 即可（5.1 读 LF 的脚本没问题）。
* `Start-Process` **不允许**把 stdout 与 stderr 重定向到同一个文件，所以服务日志
  是两个：`api-<实例名>.log` 与 `api-<实例名>.log.err`（实例名 `standard`/`lite`/`netease`），
  报错时两个都打。
* 启动器里那份配置解析（读 `sources.active` 与 `[sources.<kind>].api_base`）是手写的
  正则，不是 TOML 解析器。改配置结构时要同步改它——同理，`kugou-api-install.ps1`
  里钉住的提交必须和 bash 版的一致（一处钉、一处跟 master 是最坏的组合）。

`docs/INSTALL.md` 的「在 Windows 上构建与运行」一节列了平台能力对照表
（哪些是降级、哪些是缺失），改动平台分支后记得同步那张表。

### 1.8 拆 `update.rs`：进度与规矩

`update.rs` 曾经是一个 4700 行的 `impl App`，职责太多。拆的目标**不是减少行数**，
是降低「改一处要读多少上下文」的成本。按审计给的顺序，可靠性问题先修完（`download.rs`
的 Range 校验、`accepts_*` 那套陈旧结果判据），再动结构——先修可靠性才知道真正的边界在哪。

已完成（每块都是**纯搬移**，逻辑零改动，搬完逐字符比对过）：

| 模块 | 行数 | 内容 |
|---|---|---|
| `navigation.rs` | 275 | 切页、焦点、选择移动、`go_back`、数字键、`v` 键打开音源页 |
| `playback.rs` | 320 | 起播 / 切歌 / 进度 / 音量 / 静音 / 歌词偏移 |
| `search.rs` | 142 | 提交搜索、加载更多 |
| `desktop.rs` | 106 | MPRIS / 托盘快照、窗口控制 |
| `cloud.rs` | 937 | 登录 / 音源切换 / VIP / 云端歌单 / 账号资料 |
| `settings.rs` | 728 | 设置页的候选值与三个交互（`move_settings` / `click_setting` / `adjust_setting`） |

`update.rs`：4728 → 4144 → 2958 行。

**边界修正（切完 playback 之后发现并修掉的）**：`activate()` 与 `startup_search()` 搬回了
`update.rs` 的分派层。`activate` 原本跟着「导航」那一节切进了 `navigation.rs`，但它是
「Enter 键按 `(Tab, Focus)` 决定做什么」——横跨 search / playback / 数据加载三个方向。
结果就是 navigation 去调播放、去调搜索，`navigation ↔ search` 变成**叶子互依**。
`startup_search`（`--search` 的启动胶水：切页 + 填词 + 提交）同理，留在 `search.rs`
会让 search 反过来依赖 navigation。

搬完的依赖形状（**这就是要守的目标**）：

```text
update ──→ navigation / search / playback / cloud / desktop / settings / mod   （分派器）
navigation ──→ update（请加载层拉数据）、settings（设置页的光标移动）
playback ──→ update（client_for / load_cover / request_lyric / sync_queue_cursor）
settings ──→ update（refresh_cache_usage）
cloud ──→ update（client_for）、mod（ensure_device_fingerprint）
desktop ──→ （无）
search ──→ （无）
```

叶子之间零调用，且**没有互相调用的一对**。`update.rs` 里留下的是三类东西：分派
（`handle_event` / `handle_action` / `activate`）、加载层（`load_*` / `open_selected_*`，
`navigation.rs` 只请它们拉数据）、结果与收尾（`handle_loaded` / `handle_audio_event` /
`tick`）。

**「播放控制」那一节并不干净，所以没有整节搬走。** 它里面挤着五个不属于播放的方法，
前三个后来去了 `desktop.rs`，另两个留在 `update.rs`：

| 不在 playback.rs | 为什么不算播放 |
|---|---|
| `sync_mpris` / `sync_tray` → `desktop.rs` | 输出适配器，由 `tick()` 每拍调一次把状态推给 D-Bus 组件 |
| `toggle_window` → `desktop.rs` | 走 niri 的 compositor IPC，与音频无关 |
| `client_for` | 全文件共用的客户端构造辅助（8 处调用），搬走只会让调用方到处写 `pub(super)` |
| `request_lyric` | 「取歌词」这个网络请求；它的结果处理在 `handle_loaded` 里，请求与处理同文件才省上下文（`load_cover` 同理，两者现在同在 `update.rs` 的「封面与歌词」小节） |

**这是判断拆分的标准**：一节里的方法**调用方在哪、结果谁处理**，比它写在哪个标题下面更重要。
宁可模块小一点、边界干净，也不要为了凑体积把邻居一起搬走。

一轮拆完之后还有一处**故意接受**的跨模块调用：`navigation.rs` 的 `move_selection`
调 `settings.rs` 的 `move_settings`。设置页的光标移动归 `settings.rs`（「设置页的三个
交互」应当在一起），所以导航这一侧只是请它挪一下；它不反过来调导航，也没有把
`settings_cursor` 的细节散到两个文件里。改之前想清楚这一点，别为了「图上一个边」
把 `move_settings` 搬回 `update.rs`——那才是真的把一个设置页的行为扔进了
「什么都放」的文件。

切 `cloud.rs` 之前那一大段的归类（登录 / 音源 / VIP / 云端歌单 / 通用加载辅助）
已经做完，落在 `cloud.rs` 的四类各有小节标题；`client_for`、`request_lyric`
这类**谁都在用**的辅助留在 `update.rs`。

切的时候会撞上一件事，先知道就不用惊讶：**Rust 的方法私有性是按「写 impl 块的模块」
算的**，不是按类型。方法一搬走，另一侧调用它就报 `E0624 method is private`，
得逐个改成 `pub(super)`。这不是设计问题，是拆分的固有代价——编译器会把清单列全，
照它改就行（导航 13 处、播放 16 处、桌面 3 处、云端 15 处、设置 1 处）。
注意**两个方向都要改**：搬走的要被原来的调用方调到，留在原地的要被搬走的那块调到。

几条规矩：

* **一次只切一块，切完立刻 `clippy -D warnings` + `cargo test`**。搬移不改逻辑，
  所以全部测试（当前 328 个）必须全绿；绿不了说明搬错了，不是「顺手改好了」。
* **搬移要留证据**。做法：搬之前把原始文件复制到 `/tmp`，搬完跑一遍
  「按函数名比对签名 + 函数体（空白折叠成一个空格、`pub(super)` 归一化）」的脚本，
  逐字符确认每个函数都还活着。这一轮的结果是 116 个函数全部逐字符存活，
  唯一被删的是 `update.rs` 里那份重复的音质标签函数（见 §6）。
* **别在搬移的那次提交里夹带改动**。混在一起就没人能看出哪一行是行为变化。
* `handle_loaded`（事件分发）**留在 `update.rs`**：它是调度器，判断「这条结果该不该
  采纳」的判据也留在那里（见 §4 第 11 条），只有纯业务动作搬走。
* **切口容易吃掉小节标题**。`sed -i 'A,Bd'` 的边界多一行少一行，症状是留下一个孤立的
  `// =====` 或把 `// 播放控制` 一起带走。切完立刻 `sed -n` 看接缝，并
  `grep -n '^    // [^=]'` 点一遍剩下的小节标题。
* **切完跑一遍模块调用图，要的是星形。** 搬移等价不代表边界正确——边界错了不报错，
  只是把耦合从文件里挪到模块之间。做法：取出各模块定义的方法名，再数
  `src/app/X.rs` 里出现多少次 `self.<Y 的方法>(`。出现**叶子 ↔ 叶子**就是边界错了，
  回去找那个函数（通常是「按 Tab 分派」的那种）。上面的 `navigation ↔ search`
  就是这么查出来的。
* **一节里的方法不一定属于同一件事**。判断标准是「调用方在哪、结果谁处理」，
  不是「它写在哪个标题下面」（见上面那张表）。
* 常量跟着用它的人走：`RESTART_THRESHOLD_MS` 搬进 `playback.rs`，
  `SEARCH_MAX_PAGES` 搬进 `search.rs`——它们只被那一块用。

### 1.9 WebSocket 服务（`ws.rs`）

协议对齐 MoeKoeMusic（<https://music.moekoe.cn/zh-CN/websocket-api.html>），默认监听
`127.0.0.1:6520`，`--no-ws` / `ws = false` 关闭，`--ws-port` 改端口。

```
main loop(tick) ──sync_ws()──→ watch::Sender<Arc<Snapshot>>   （只写快照）
                                        │
ws thread ──watch::Receiver─────────────┘  diff_messages() → broadcast → 各连接
    │
    └── 入站 control ──EventBus──→ main loop 派发成已有 Action
```

几条设计取舍，改之前先读：

* **自己起线程 + `current_thread` runtime**，不往主 runtime 里塞。它只做两件事：
  收发 WebSocket 帧、比对快照。主循环的 tick 节奏不该被网络 I/O 影响。
* **`sync_ws` 每拍写一份完整快照**（含 `Song` 克隆），`send_if_modified` 用
  `Snapshot` 的自定义 `PartialEq` 判等——它只比较「会改变推送内容」的字段，
  刻意不比整个 `Song`（`privilege` 是个大嵌套 JSON，每拍比一遍不值）。
  快照没变就不唤醒广播任务，暂停期间这条调用近乎免费。
* **`playerState` 只在播放 / 暂停翻转时推**，不是每拍推。上游
  `updatePlayerState` 的唯一调用点是 `electron/main.js` 的 `play-pause-action`；
  文档写的是「播放状态发生变化时」。连接建立时单独补发一次。
* **`lyrics` 每拍推**（上游 `server-lyrics` 路径同样不防抖；只有桌面歌词那条 IPC
  按行去抖）。`lyricsData` 是**原始歌词文本**，即 `Lyric::text`——不是解析后的
  `lines`，因为逐字标记与 `[language:]` 标签正是第三方客户端要自己排版的。
* **welcome 的 `data` 是纯字符串**，不是文档样例里的嵌套对象。以源码为准。
* **未知 `control` 命令回一条 `error`**，不像上游那样静默忽略。
* **握手校验 `Origin`**：浏览器里任何页面都能连本机端口，不校验等于把播放控制
  开放给当时打开的每个标签页。没有 `Origin` 的原生客户端放行。
* **入站消息限 64 KiB**（tungstenite 默认 64 MiB 太宽）。出站不限，KRC 可能几十 KB。
* **`lyrics` 变体是 `Box<LyricsData>`**：不装箱时枚举每个实例都 264 字节，而它是
  按值在 `Vec` 里传的。

### 1.10 WebSocket 的验证方式

没有自动化集成测试（要真起播放器），靠一个外部客户端手工验：

```bash
# 1) 起播放器（隔离配置目录！别碰 ~/.config/kugou-tui）
KUGOU_TUI_CONFIG_DIR=/tmp/kt-ws ./target/debug/kugou-tui --search 周杰伦 --no-tray
# 2) 另开一个终端，用任意 WS 客户端连 ws://127.0.0.1:6520/
#    期望：先收 welcome + playerState，播放后每 200ms 一条 lyrics；
#    发 {"type":"control","data":{"command":"toggle"}} 应切换播放并收到 playerState。
```

`node --experimental-websocket` 或浏览器控制台都行，不需要额外依赖。


---

## 2. 跑起来 / 怎么验

```bash
cargo test                         # 单元测试（约 290 条，秒级）
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

改 `app/update.rs` 的 `handle_loaded` 之前，这条也要守住：

11. **每一条异步结果都要自证身份**。请求都是 `tokio::spawn` 出去的，回来的顺序不保证；
    用户完全可以在这中间再搜一次、再开一个歌单、再点一位歌手。结果照单全收就会变成
    「输入框写着 B、列表是 A」这类**看起来就是数据错了**的状态，而且不报错。

    判据是「身份比对」，不是给每次请求编号：播放那一侧本来就这么做
    （`App::is_current` 比 `song.hash`、歌词比 `hash`），其余照抄同一套思路即可。
    已经有的比对点：

    | 结果 | 拿什么比 |
    |---|---|
    | `StreamReady` / `StreamPrerolled` / `StreamCached` | `song.hash`（`App::is_current`） |
    | `Lyric` / `LyricFailed` / `CoverReady` | `hash` |
    | `Search` | 关键词（`state.search.submitted`） |
    | `PlaylistTracks` | 当前打开的歌单 id（`open_playlist`） |
    | `ArtistSongs` / `RankTracks` | 当前打开的歌手 / 榜单 id |
    | `Playlists` / `Artists` | 请求时的分类 id（`category` / `kind`） |

    判据本身抽成了自由函数（`accepts_search_result` / `accepts_open_item`），
    因为它们能单独测，而 `App` 构造需要音频引擎与运行时、单测里搭不出来。
    **新增一个带网络请求的 `Loaded` 变体时，先想清楚它回来时拿什么比。**
    另外，身份字段要**在 `spawn` 之前**写进状态（`open_playlist` / `open_artist` /
    `open_board` 都是这么做的）——写在结果里就晚了，那条结果永远对不上。

改 `audio/downmix.rs`（多声道下混）之前，这条也要守住：

12. **多声道文件必须先下混，再交给 rodio。** rodio 的 `ChannelCountConverter`
    在降声道时是「保留每帧前 N 个样本、其余直接丢弃」（`conversions/channels.rs`，
    单测 `remove_channels` 里 4→1 得到 `[1.0, 5.0]`）。而酷狗**存在多声道无损文件**，
    且可能是「前两个声道不是这首歌」的那种：实测《东京不太热 (DJ Z新豪版)》的 `flac`
    档是 4.0，歌在后两个声道里（前两个电平低 6–10 dB、与正确混音相关性仅 +0.35）。
    少了这一层，用户听到的不是这首歌，表现成「音频版本和其他客户端不一致」。
    判据与实测数据在 `downmix` 模块顶部；`build_decoder` 的两个出口都必须过
    `downmix::to_stereo`。两条守护测试会在拆掉这层时失败：
    `decoded_multichannel_files_are_downmixed_to_stereo`（自造 4 声道 WAV 走
    `build_decoder`，断言 `channels() <= 2`）与
    `the_mixer_hears_the_song_only_because_we_downmixed_first`（用
     `rodio::mixer::mixer` 搭不依赖声卡的 mixer，对照证明「直接进 mixer 只剩前两个声道、
     先下混再进才对」——钉的正是 bug 发生的那一层）。

改 `app/desktop.rs`（把播放状态同步给桌面集成）之前，这条也要守住：

13. **每拍先比对、再克隆。** `tick` 每 200ms 调一次 `sync_ws` / `sync_mpris` / `sync_tray`，
     它们只拿 `&self.state`：**不要**把当前曲目、歌手列表、整份歌词 `clone()` 出来再交给
     句柄判等。`WsHandle::update_from` / `MprisHandle::update_track` / `TrayHandle::update_track`
     都是「先比 `hash` / `track_id` 与进度，变了才重建元数据」，传引用 + 闭包即可；曲目比的
     是**原始 hash 字符串**（不是摘要），逐字节相等才算没变。守护测试：
     `update_from_wakes_only_when_the_snapshot_changes`、
     `update_track_skips_metadata_rebuild_for_the_same_track`。

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
- **多声道文件在 rodio 里会被「丢声道」。** 降声道只保留每帧前 N 个样本、其余丢弃
  （`conversions/channels.rs`）。酷狗有多声道无损文件，而且可能是「歌在后两个声道」的
  那种（见 §4 第 12 条）。`build_decoder` 现在统一过 `downmix::to_stereo`，
  别把那层去掉。诊断手法：`ffprobe` 看 `channels`，再逐声道算与正确混音的相关性。
- **`/song/url` 会静默降级，且 `status` 仍是 1。** 实测请求 `quality=flac` 对没有无损档的
  歌会回 `extName=mp3` + `bitRate=128000`。只判 `status` 的话界面会一直标着 flac、
  实际放 128 kbps。判据在 `catalog::downgrade_note`（按容器 + 码率粗排，`super` 与
  蝰蛇系列不参与比较），原因经 `StreamUrl::reason` 透到界面——注意
  `handle_loaded` 里 `reason` 的**两个分支都要看**，只处理试听那支会把降级提示吞掉。

**下载/缓存**

- `cache.find` 只按"文件在不在"判断，不校验长度——这就是 `.part` 存在的理由。
- **分块下载的每一步都要自证**：状态码必须是 `206`、`Content-Range` 必须与请求范围
  逐字节一致、每块要写满自己的长度、总和要等于总长度。少任何一条，服务端（或中间
  CDN / 代理）忽略 `Range`、回 `200 OK` 加整首时就会让 4 个分块互相覆盖，拼出一个
  "长度对、内容全错"的文件——而它照常改名进缓存。自己写代理做实验时也别忘透传
  `Range`。回归测试见 `audio/download.rs` 的 `server_ignoring_range_*` 与
  `truncated_transfer_*`（用一个 std `TcpListener` 起的假服务端，没引新依赖）。
- `.part` 是 `set_len(total)` 预分配过的，**文件大小证明不了完整性**，只有写满的
  字节数能。所以"下载任务结束"不等于"下载成功"。
- 分块下载失败会**退回单连接**重下一次（`fetch_to`），所以"看起来下了两遍"可能是正常的。
- 缓存回收是同步目录扫描，必须在 `spawn_blocking` 里跑，别卡住 UI。
- **同一个目标上不能同时跑两条流**。`start_streaming` 在一个按临时文件路径索引的
  登记表里查重：第二条直接复用第一条的缓冲，不再起任务。原因是两条流会互相破坏——
  后开的那次 `truncate(true)` 抹掉先写的字节，失败/取消时的 `remove_file` 删掉对方
  的文件。触发它不需要异常操作：**连按两次 Enter** 就是两条 `start_streaming`
  落在同一个目标上（`App::active_stream` 要等攒够开头才被握住，中间那段窗口谁也
  拦不住第二条）。回归测试 `audio::download::tests::a_second_stream_for_the_same_target_reuses_the_first_buffer`。
- 整首下载与流式下载**不共用**临时文件（`x.mp3.part` vs `x.mp3.stream.part`，
  见 §1.4）：后台预取下一首时，用户完全可能正好切到那一首。

**列表分页**

- `pagesize` 在歌单类接口上是**硬上限 30**，响应里的 `count` 只是当页条数、**不是总数**；
  要算总页数只能用歌单元数据里的 `song_count`。
- 「这一页不满 ⇒ 这就是全部」这个判据**只在第 1 页成立**。首屏改从末页取之后
  （倒序显示时），末页不满是常态——照搬会让"整表补齐"永远不出门，歌单只显示末页
  那几十首。判据抽在 `app::update::needs_full_fetch`，有测试钉住。
- 更根本的一条：**判"还有没有下一页"只能看这一页是不是空**。解析会过滤条目
  （缺 hash、字段类型不对），一页 30 条剩 29 条是常事，按"不满页"停会把列表静默
  截断。**两侧现在是同一个判据**：酷狗在
  `catalog.rs::collect_all_pages`、网易云在
  `source/netease.rs::should_continue_paging`（2026-09-28 改的，见 §7）。
  网易云的判据抽成了自由函数，因为它单测里测不了整条翻页（要 HTTP），
  而判据本身必须能钉住——见那条测试的注释。
- **并发翻页的结果必须按页码拼，不能按完成顺序拼。** `JoinSet::join_next()` 给的是
  任务完成顺序，一批页同时发出去谁先回来谁先 append，整表页序就是随机的——榜单
  尤其致命，它的"顺序"就是榜单内容本身。回归测试
  `api::catalog::tests::concurrent_pages_are_assembled_in_page_order`（用手写的假
  服务端让页码**必然**倒序返回，所以这条测试不是靠运气过的）。
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

**歌词解析**

- **「这一行算不算歌词」的判据只能有一处**（`api/lyric.rs::has_lyric_content`）。
  `parse_lrc` 用它决定哪些行进 `Lyric`，`attach_translations` 用它算这些行在
  **原始定时行**里的序号、好去语言轨（`[language:base64]`）里取译文。
  两边各判一次、判据稍有不同，序号就整体错开一位，**之后每一行的译文都往后串**——
  表现是「译文和原文对不上」，不报错、不崩溃，只能靠盯着歌词看。
- **`<` 之前的字也是正文**。KRC 的标准写法是每行以 `<偏移,持续,0>` 开头，所以
  标记之前通常是空的；但歌里出现「有 `<` 却没有配对 `>`」的文本时（比如 `宝贝<3`），
  只收标记之后的字就会把整行丢掉。`parse_krc_words` 现在先收前缀，
  而前缀没有逐字时间，于是 `words` 被那条「字数与标记数必须一致」的检查清空、
  退回整行高亮——这是设计好的兜底，不是 bug。
- 逐字信息宁可**没有**也不要**猜**：字数与标记数对不上时清空 `words`，
  界面退回整行高亮。猜出来的时间会让整行唱得和声音对不上，比没效果更糟。

**渲染 / 布局**

- **`Ord::clamp` 在 `min > max` 时 panic**（`assert!(min <= max)`）。`x.clamp(1, area.height)`
  这种写法在 `area.height == 0` 时是一颗定时炸弹，而它长得完全像防御性代码。
  退化区域要先 `if area.width == 0 || area.height == 0 { return ...; }` 挡掉，
  **不要**靠 `max(1)` 硬凑——凑出来的框会跑到区域外面，`area.width - columns`
  接着下溢。踩过一次：`ui/views/player.rs::fit_box`。
- 反过来也要注意：`area.width - columns` 这类减法在「框比区域大」时下溢，
  所以算完框要**同时**断言 `框 ⊆ 区域`，别只断言「不 panic」。
- **UI 层目前没有任何生产代码里的 `unwrap()` / `expect()`**（全都在 `#[cfg(test)]` 里）。
  一次 panic 就是界面没了、终端留在 raw 模式，是最贵的一类故障——保持这个状态。
- **宽而矮**的终端（比如 100×8）是布局最容易出问题的形状：宽屏分支开了、
  高度却不够，`Min` 与 `Length` 抢空间。改布局后拿几个这样的尺寸过一遍
  （`TestBackend` + `terminal.draw`，见 `render_home_in_a_short_wide_area_does_not_panic`）。
  实测 `[Min(6), Length(8)]` 在空间不够时是 `Min` 赢，所以封面栏最低仍有 4 行——
  但那是**实测出来的行为，不是文档承诺**，升级 ratatui 后要重新测。
- **登录二维码不要每帧重编码**：`render_login` 直接复用 `login.qr`（收到二维码时就按
  `config.qr_aspect` 渲染好了，见 `Loaded::LoginQr`），`qr_lines_fitted` 只对已渲染的行做
  「装不装得下」判断、不再自己编码；只有终端装不下时才用 `qr_needed_size` 编一次。
  一次编码实测约 0.9ms，在 30fps 下是很大一块预算（基准：`bench_qr_encoding_cost`、
  `bench_login_render_cost`）。

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

---

## 7. 已知但**没有动**的地方

这一节记的是「审查过、确认有问题或可疑，但这次故意没改」的东西。留着它们不是
忘了，是改的代价或风险大于收益——动之前先读这里的理由。

| 位置 | 问题 | 为什么没动 |
|---|---|---|
| ~~`source/netease.rs::playlist_tracks_all`~~**（2026-09-28 已修，留着记结论）** | ~~用「不满一页 ⇒ 结束」停翻页，与酷狗那侧「只有空页才停」的判据冲突；某页只要有一条被解析过滤掉，整张表就被静默截断~~ | 当时的阻碍是「没法实测」，已消除：起了 NeteaseCloudMusicApi（`:3002`）实测越界 offset——`offset=500`（该歌单 200 首）与 `offset=999999` 都返回 `{"songs":[],"code":200}`，**空数组、不是报错**，也没把 offset 夹回末页返回重复内容。判据抽成 `should_continue_paging`（`got > 0`），与酷狗侧一致；回归测试 `paging_stops_only_on_an_empty_page` 在退回旧判据时确实失败。代价：整表加载末尾多一个请求 |
| `source/netease.rs::plaza_playlists` / `artist_list` | 分类（`_category_id`）与地区（`_kind`）参数被忽略，但界面照常显示选择器 | 界面传下来的是**酷狗**那套分类 id，网易云要的是 `cat` 字符串 / `area`，两套对不上。补映射是新增功能，不是修 bug；先承认它不生效，别让用户以为筛选坏了 |
| `api/catalog.rs::request_song_url_with_hash` | `Song.album_id` / `album_audio_id` 解析了但没发给 `/song/url` | 2026-09-28 在概念版服务上实测四种参数组合（不带 / 带 `ppage_id` / 带搜索给的 id / 带 `/privilege/lite` 给的 id）**都拿到了直链**，而 `status_reason()` 里记着「带 privilege 那个 id 会 status=3」的反例。传了没有可证明的收益、传错有明确代价，所以一个都不传；结论写在那里的注释里了 |
| `api/model.rs::normalize_duration` | 用 10000 猜单位：9 秒的短曲（毫秒值 9000）会被当成 9000 秒；2.8 小时的长合集（秒值 10800）会被当成 10.8 秒 | 两个方向都在 1000–9999 这个区间里撞车，而实际接口各自的单位是**已知**的（搜索 `Duration` 秒、歌单 `timelen` 毫秒），这个兜底只在遇到没见过的形状时才生效。改阈值是把错从这个区间挪到那个区间，不是修好；有测试钉着当前行为，等真撞上再说 |
| `config.rs::save` | 直接 `fs::write` 覆盖 `config.toml`，不是「写临时文件 + 改名」，崩溃时理论上会留下半个配置文件 | 配置文件只有几百字节，一次 `write` 系统调用写不满一个块，实际拿到的是旧内容或新内容，不是混合的；而且 `load()` 对坏文件是退默认配置并告警，最坏是「设置丢了要重新登录」。真要改得处理 Windows 上 `rename` 不能覆盖已存在文件的差异，收益配不上这个复杂度 |
| `audio/download.rs` | 两条**整首**下载落在同一个目标上时（预取 vs 续播）仍会争用同一个 `.part` | 两条写的是同一个 URL 的同一份字节，最坏是后完的那次改名失败、报一次下载错误。触发链条很长（预取在飞 → 切歌 → 那一首又断流），先把「流式 × 整首」这个更容易撞的组合隔开就够了 |

**改其中任何一条之前**，先看有没有办法真机验证——这几条的共同点就是「改了之后
对不对，光靠单元测试看不出来」。
