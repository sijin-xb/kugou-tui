# 使用指南

## 运行环境

kugou-tui 支持 Linux、Windows 和 macOS。
Linux 构建需要 `alsa-lib` 与 `pkg-config`。
Windows 使用 MSVC 工具链；macOS 使用 Xcode 命令行工具。

默认的酷狗接口运行在进程内，不需要 Node.js。
网易云音源和酷狗 Node 回退需要本机 API 服务。

## 首次运行

```bash
kugou-tui
```

按 `2` 进入搜索页，输入关键词后按 `Enter`。
选中歌曲后按 `Enter` 播放，按 `Space` 暂停或继续。

未登录可以播放无需认证的曲目。
搜索、云端歌单等操作在需要认证时会提示登录。
云端歌单、收藏和部分歌单操作需要登录。

按 `?` 或 `F1` 查看程序内帮助。
`--help` 显示命令行参数，`--print-config` 显示当前生效配置。

## 页面与焦点

- `1`：首页。
- `2`：搜索。
- `3`：歌单广场。
- `4`：歌手。
- `5`：排行榜。

- `6`：云端歌单。
- `7`：播放队列。
- `8`：音源管理。
- `9`：设置。
- `0`：可视化。

`Tab` 和 `S-Tab` 在侧边栏、内容区和队列之间切换焦点。
首页与可视化页中，主区域获得焦点时 `j` / `k` 或方向键用于切歌。

## 导航与播放

- `j` / `k` 或 `↓` / `↑`：移动选中项。
- `g` / `G`：首项 / 末项。
- `PgUp` / `PgDn`：上一页 / 下一页。
- `Enter`：播放或进入。
- `Esc`：返回或取消。

- `Space`：播放 / 暂停。
- `n` / `p`：下一首 / 上一首。
- `←` / `→`：后退 / 前进 5 秒。
- `+` / `-`：音量增加 / 减少 5%。
- `m`：静音或恢复。

- `r`：切换顺序、列表循环、单曲循环、随机。
- `y`：切换音质。

上一首在当前曲目播放超过 3 秒时会先回到本曲开头。

## 队列

- `a`：将选中歌曲加入队尾。
- `i`：将选中歌曲插到下一首。
- `A`：将当前列表全部加入队列。
- `x`：从队列移除选中歌曲。
- `X`：清空队列，需要确认。

- `o`：切换列表正序 / 倒序。
- `c`：打开云端歌单。
- `S`：将当前队列同步到云端歌单。

播放搜索或歌单中的歌曲时，整个当前列表会成为播放队列。
队列可以包含来自不同音源的歌曲。

## 搜索与目录

- `/`：打开搜索框。
- `Ctrl+T`：切换单曲、歌手、歌单搜索。
- `M`：加载更多搜索结果。
- `R`：刷新当前视图。

- `b`：打开排行榜。
- `f`：切换歌手地区筛选。

酷狗与酷狗概念版支持歌手和歌单搜索。
网易云与汽水音乐只支持单曲搜索。

## 歌词、频谱与睡眠

| 按键 | 功能 |
|---|---|
| `l` | 显示或隐藏歌词面板 |
| `[` / `]` | 歌词延后 / 提前 100 ms |
| `0` | 打开整屏可视化 |
| `z` | 切换睡眠定时 |

睡眠定时按“关闭、15、30、60、120 分钟”循环。
倒计时只在播放中推进，到点前 15 秒逐渐降低音量，然后暂停。

可视化页默认使用全局 `e` / `v` 之外的页面按键：

| 按键 | 功能 |
|---|---|
| `E` | 打开 cava 参数面板 |
| `V` | 切换柱状图 / 瀑布图 |

自定义键位占用某个键后，页面局部键优先让路。
可通过 `open_cava_settings` 与 `toggle_visualizer_mode` 重绑。

## 鼠标

- 单击列表：选中。
- 双击列表：播放或进入。
- 滚轮：移动列表选中项。
- 按住进度条：连续跳转。
- 按住音量条：连续调整音量。
- 按住歌词行：跳到对应歌词位置。

- 右键或按 `;`：打开歌曲菜单。

拖动进度条时界面立即跟随。
实际音频 seek 每 200 ms 最多执行一次，松开时提交最终位置。

## 登录与云端歌单

按 `L` 选择音源并扫码登录。
酷狗与网易云使用接口生成的二维码。

汽水音乐通过本机 Chrome、Chromium 或 Edge 的 CDP 签名页登录。
也可以使用 `--sodam-cookie` 或配置文件中的 `[sources.sodam]`。

登录凭据按音源分别保存。
切换音源不会丢失其他音源的登录态。

- `s`：收藏到云端歌单。
- `d`：从云端歌单移除。
- `D`：删除云端歌单。

- `N`：新建云端歌单。
- `V`：领取酷狗概念版当日 VIP。

云端歌单写操作只接受与歌单同音源的歌曲。

## 音源

| 音源 | 运行方式 |
|---|---|
| 酷狗 | 默认使用内嵌 native 后端 |
| 酷狗概念版 | 默认使用内嵌 native 后端 |
| 网易云 | 本机 NeteaseCloudMusicApi，默认端口 3002 |
| 汽水音乐 | 直连公网，无本地服务 |

按 `8` 打开音源页：

| 按键 | 功能 |
|---|---|
| `Enter` | 启用或禁用选中音源 |
| `E` | 设为默认音源 |
| `J` / `K` | 下调 / 上调优先级 |

`api_backend` 只控制酷狗后端。
`native` 不会影响网易云与汽水音乐。

## 酷狗 Node 回退

```bash
kugou-tui --api node
```

Node 模式会探测本机服务。
服务不存在时，程序可下载固定版本的 KuGouMusicApi、安装依赖并启动。

```bash
kugou-tui --api-start
kugou-tui --api-stop
```

`--api-start` 启动的服务会留在后台。
普通启动拉起的服务会随 kugou-tui 退出。

源码树或发行包还可使用：

```bash
./scripts/kugou-api start
./scripts/kugou-api stop
./scripts/kugou-api restart
./scripts/kugou-api status
./scripts/kugou-api logs
```

设置 `api_auto_start = false` 或传入 `--no-api-start` 可关闭自动拉起。

## 网易云服务

从源码树安装并启动：

```bash
./scripts/kugou-api-install netease
```

服务默认监听 `http://127.0.0.1:3002`。
kugou-tui 不会自动下载或托管 NeteaseCloudMusicApi。

## 汽水音乐签名

汽水完整曲目和无损依赖应用签名。
默认使用 libresoda 内置的公共签名服务。

```bash
kugou-tui \
  --sodam-signer-url URL \
  --sodam-signer-token TOKEN
```

指向自建签名服务时使用以上参数。
传入 `--sodam-signer-url none` 会关闭签名，只保留试听和免费曲目。

## 配置文件

- Linux：`~/.config/kugou-tui/config.toml`。
- macOS：`~/Library/Application Support/`
  `kugou-tui/config.toml`。
- Windows：`%APPDATA%\kugou-tui\config.toml`。

配置优先级为命令行、环境变量、配置文件、代码默认值。
`--print-config` 可检查最终生效值。

常用配置：

| 配置 | 默认 | 说明 |
|---|---|---|
| `api_backend` | `native` | `native` 或 `node`，只影响酷狗 |
| `volume` | `0.7` | `0.0` 到 `1.0` |
| `quality` | `128` | `128`、`320`、`flac`、`high` 等 |
| `playback_mode` | `sequential` | 顺序、列表循环、单曲循环、随机 |

| 配置 | 默认 | 说明 |
|---|---|---|
| `cache_limit_mib` | `512` | 音频缓存上限，`0` 表示不限 |
| `tick_ms` | `200` | 普通界面刷新间隔 |
| `lite_mode` | `false` | 关闭封面与频谱，降低刷新率 |

| 配置 | 默认 | 说明 |
|---|---|---|
| `tray` | Unix 为 `true` | 系统托盘 |
| `ws` | `true` | 本机 WebSocket 服务 |
| `ws_port` | `6520` | WebSocket 监听端口 |
| `lyric_offset_ms` | `0` | 正数表示歌词提前 |

音源凭据与优先级位于 `[sources.<name>]`。
汽水签名参数位于 `[sources.sodam_app]`。

自定义键位写在 `[keymap]`：

```toml
[keymap]
quit = "Q"
play_pause = "space"
open_search = "/"
```

动作名与按键名区分大小写。
未配置的动作继续使用默认键位。

## 桌面集成

WebSocket 默认监听 `127.0.0.1:6520`。
它推送播放状态、歌词和当前时间，并接受 `toggle`、`next`、`prev`。

```bash
kugou-tui --no-ws
kugou-tui --ws-port 6530
```

Unix 上还提供 MPRIS 与 StatusNotifierItem。
MPRIS 可配合 `playerctl`、KDE、Quickshell 等桌面组件使用。

托盘需要桌面提供 `org.kde.StatusNotifierWatcher`。
没有 watcher 时托盘线程会定期重试，播放不受影响。

## 无 Node 构建

```bash
cargo build --release --no-default-features
```

该构建移除所有 Node 引导代码。
酷狗 native 后端仍可使用，`--api node` 会报告功能已在构建时禁用。

## 排障

### 没有声音

在设置页选择正确的 `audio_device`。
Linux 下优先使用 PipeWire 或 PulseAudio 路由的设备。

### 搜索失败或提示登录

按 `L` 登录当前音源。
酷狗与酷狗概念版的登录态不通用。

### 封面显示为色块

程序只自动识别 Kitty 与 iTerm2 图形协议。
其他终端使用半块字符；可开启 `lite_mode` 关闭封面。

tmux 与 screen 会强制使用半块字符。
否则被继承的 Kitty 环境变量会导致图形转义序列显示为乱码。

### 中文或图标显示异常

安装中文字体和 Nerd Font。
字体探测失败时可用 `KUGOU_TUI_NERD_FONT=1` 强制显示图标。

### 查看日志

日志位于缓存目录的 `kugou-tui.log`。
Linux 默认路径为 `~/.cache/kugou-tui/kugou-tui.log`。

```bash
env KUGOU_TUI_DEBUG=1 kugou-tui
env KUGOU_TUI_MEM_TRACE=1 kugou-tui
```

日志会脱敏 `token`、`userid`、`dfid` 和 `mid`。
内存追踪每 5 秒输出一行 `[mem]`。

### 检查实际配置

```bash
kugou-tui --print-config
```

输出包含当前音源、后端、配置文件、日志文件和运行参数。
