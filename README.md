# kugou-tui

**在终端里听酷狗：逐字歌词、真频谱，单二进制常驻约 19 MiB。**

![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)
![Rust](https://img.shields.io/badge/rust-1.90%2B-orange.svg)
![Platform](https://img.shields.io/badge/platform-linux%20%7C%20windows-lightgrey.svg)
![crates.io](https://img.shields.io/crates/v/kugou-tui.svg)

![kugou-tui：歌单广场、正在播放、逐字歌词与播放队列](assets/screenshot-0.3.3.jpg)

*主界面：左侧导航，中间逐字歌词，右侧播放队列与封面。画面全部由程序渲染，截图取自真实使用场景。*

## 它是什么

终端里的酷狗播放器。没有窗口、没有广告、没有推荐流：敲 `2` 搜歌，`Enter` 播放，
`L` 扫码把云端歌单接进来。只做「找到歌 → 放出来 → 把歌词和频谱画好看」这一件事。

1. **逐字歌词** —— 解析酷狗 KRC 的每字时间戳，按每个字自己的进度插值，边界字是渐变
   过渡；不是「整行一起亮」那种 LRC。
2. **真频谱** —— 对音频线程采集的真实采样做 FFT，对数分频到 40Hz–16kHz。
3. **酷狗曲库 + 云端歌单** —— 搜歌即播，不用先有本地文件；歌单广场、歌手、排行榜、
   个人云端歌单齐全；登录二维码直接画在终端里。

## 安装

接口实现**内嵌在二进制里**（纯 Rust），默认不需要 Node.js。两种后端可以随时切换：

| | `--api native`（内嵌，推荐） | `--api node`（回退） |
|---|---|---|
| 需要 Node.js | **不需要** | 需要 ≥ 12 |
| 常驻内存 | 约 19 MiB（单进程） | 本程序约 19 MiB **+ 独立的 node 服务进程 42–67 MiB** |
| 覆盖范围 | 酷狗标准版 / 概念版全部功能 | 同上，另含网易云与汽水音源的传输层 |

> 当前版本的**默认值仍是 `--api node`**（保持与历史版本一致）。想用内嵌后端，
> 加 `--api native`，或写进配置文件：`api_backend = "native"`。默认值会在验收完成后
> 切换到 `native`；`--api node` 始终保留作为回退。

**cargo**（推荐）：

```bash
cargo install kugou-tui
kugou-tui --api native
```

内嵌后端启动即可用，没有「首次运行准备服务」这一步。若要用 Node 回退（或跑网易云、
汽水音源），首次运行会自动把接口服务准备好并拉起（下载 + 装依赖，约半分钟），之后每次
启动直接复用。想提前把这一步做掉：`kugou-tui --api-start`；想自己管服务：`--no-api-start`。

**想编一个永远不含 Node 引导代码的二进制**：

```bash
cargo build --release --no-default-features
```

`node-bootstrap` feature 关掉后，引导代码（探测端口、下载 KuGouMusicApi、`npm install`、
spawn `node app.js`）整段不参与编译，二进制里不存在。此时 `--api node` 会明确报错说
「构建时关掉了这个 feature」，而不是含糊地连不上。体积差约 0.06 MiB（见
[docs/DESIGN.md](docs/DESIGN.md)）。

**预编译包**：[Releases](https://github.com/sijin-xb/kugou-tui/releases) 里三个平台都有，
带着脚本与全部文档。

```bash
# Linux
tar xzf kugou-tui-<版本>-x86_64-unknown-linux-gnu.tar.gz
cd kugou-tui-<版本>-x86_64-unknown-linux-gnu
./scripts/kugou-tui --api native      # 内嵌后端，不需要 Node
./scripts/kugou-api-install kugou     # 仅当你打算用 --api node 回退时才需要
```

```powershell
# Windows
Expand-Archive kugou-tui-<版本>-x86_64-pc-windows-msvc.zip -DestinationPath .
cd kugou-tui-<版本>-x86_64-pc-windows-msvc
.\scripts\kugou-tui.ps1 --api native
.\scripts\kugou-api-install.ps1       # 仅当要用 --api node 回退
```

**源码**：

| 平台 | 前置 |
|---|---|
| Linux | Rust 1.90+、`alsa-lib` 开发头文件 + `pkg-config` |
| Windows | Rust 1.90+、MSVC 工具链（不需要 CMake / NASM / OpenSSL） |
| macOS | Rust 1.90+、Xcode 命令行工具（未在真机长期使用） |

```bash
git clone https://github.com/sijin-xb/kugou-tui.git
cd kugou-tui && cargo build --release
```

启动器脚本、AUR 包、网易云音源、各平台的详细前置：**[docs/INSTALL.md](docs/INSTALL.md)**。
Node.js 只是 `--api node` 回退路径的前提，内嵌后端不碰它。

## 平台差异

| | Linux | Windows | macOS |
|---|---|---|---|
| 播放 / 搜索 / 歌词 / 封面 / 缓存 | ✅ | ✅ | ✅（未实测） |
| 一键脚本 | `scripts/*`（bash） | `scripts/*.ps1` | `scripts/*`（bash） |
| 音频后端 | ALSA / PipeWire / PulseAudio | WASAPI | CoreAudio |
| MPRIS + 系统托盘 | ✅ | ❌ 无 D-Bus | ❌ 无 D-Bus |
| 最小化窗口（托盘菜单里那一项） | ✅ 仅 niri | ❌ | ❌ |
| 配置文件 | `~/.config/kugou-tui/` | `%APPDATA%\kugou-tui\` | `~/Library/Application Support/kugou-tui/` |
| 缓存与日志 | `~/.cache/kugou-tui/` | `%LOCALAPPDATA%\kugou-tui\` | `~/Library/Caches/kugou-tui/` |
| 终端建议 | 任意现代终端 | **Windows Terminal**（别用旧 conhost） | iTerm2 / Terminal.app |

> 三条 ❌ 是**降级而不是故障**：没有 D-Bus 时相关代码不参与编译，界面上也不会出现点了
> 没反应的入口。`kugou-tui --print-config` 会如实打印「系统托盘 : 不可用」。

## 功能总览

| 分类 | 能力 |
|---|---|
| 浏览 | 歌单广场、歌手列表（可按地区筛选）、排行榜、个人云端歌单 |
| 检索 | 单曲搜索，`M` 加载更多 |
| 播放 | 播放/暂停、上下首、±5 秒跳转、音量、静音；顺序 / 列表循环 / 单曲循环 / 随机 |
| 歌词 | 逐字高亮（KRC）、译文与音译、居中滚动、换行淡入淡出、点歌词行跳转、±100 ms 偏移 |
| 可视化 | 真频谱：FFT + 对数分频 |
| 播放队列 | 追加（`a`）、插播下一首（`i`）、整列表加入（`A`）、移除（`x`）、清空（`X`） |
| 云端歌单 | 收藏单曲（`s`）、整个队列同步（`S`）、增删歌单（`N` / `D`） |
| 登录 | 应用内扫码（`L`），二维码直接画在终端里 |
| 桌面集成 | MPRIS（`playerctl` 可控）+ 系统托盘（菜单 / 滚轮音量 / 图标随状态变暗）；**仅 Linux 桌面** |
| 输入与外观 | 键盘 + 鼠标；6 套主题（真彩 / 16 色各一版）；音频落盘缓存 + LRU 回收 |

完整快捷键见 [KEYBINDINGS.md](KEYBINDINGS.md)，其余能力见
[docs/USER_GUIDE.md](docs/USER_GUIDE.md)。

## 和 cmus / mpd 比

|  | **kugou-tui** | cmus | mpd + ncmpcpp |
|---|---|---|---|
| **曲库** | **酷狗在线曲库**：搜歌即播，不需要本地文件 | 只放本地文件 | 只放本地文件 |
| **逐字歌词** | **支持**：KRC 每字时间戳，按字推进的渐变高亮 | 整行 LRC | 整行 LRC |
| **频谱** | **内置**，零配置 | 无 | 有，但要额外配 mpd 的 fifo 输出 |
| **云端歌单** | **支持**：终端内扫码登录 | 不支持 | 不支持 |
| **常驻内存** | 约 19 MiB（`--api native`，单进程，本机实测） | 社区常见 10–25 MiB | 社区常见 15–30 MiB |
| **形态** | 单进程、单二进制（约 11.7 MiB） | 单进程 | 客户端 / 服务端分离 |
| **需要自建服务** | **不需要**（`--api native` 时接口在进程内；`--api node` 回退才用本机服务） | 不需要 | 需要（mpd） |

> 内存那一行是本机实测（release 构建、112×34 终端、`--no-tray`，读 `/proc/<pid>/status`
> 的 `VmRSS`），方法与分项见 [docs/DESIGN.md](docs/DESIGN.md)；cmus / mpd 两行是社区
> 常见量级，未在本机实测。
>
> 与迁移前相比：老版本必须常驻一个 Node 服务进程，那一个进程自己就占 **42–67 MiB**
> （实测 `node app.js --port=3000` 42.5 MiB、`--port=3001` 66.9 MiB，因音源而异）；
> `--api native` 把整个进程去掉了，程序自身只比原来多约 0.4 MiB。
>
> 一句话：**cmus / mpd 是「放你有的」，kugou-tui 是「放你想听的」。**

## 它依赖什么

**`--api native`（内嵌后端）：本程序自己就是接口实现。** 搜索、取链、歌词解密（KRC）、
设备指纹、扫码登录、云歌单读写全部是纯 Rust，跑在同一个进程里——不 spawn 任何子进程，
机器上不需要 Node.js。请求的 URL、请求头、参数顺序、签名算法逐项对照上游
[KuGouMusicApi](https://github.com/MakcRe/KuGouMusicApi) 移植并做了已知答案测试（KAT）。

**`--api node`（回退）：** 搜索、取链、歌词、云歌单走本机的第三方服务
[KuGouMusicApi](https://github.com/MakcRe/KuGouMusicApi)（Node.js 写的独立仓库）。
启动时探一次端口，没有就把服务准备好并拉起，退出时只停自己拉起的那个——所以除了
Node.js，用户不需要手工部署任何东西。想手动来一遍、或部署网易云那份服务，见
[docs/INSTALL.md](docs/INSTALL.md#部署第三方-api-服务)。

Node 回退路径的引导代码可以用 `--no-default-features` 整段关掉（见「[安装](#安装)」）。

## 内嵌 WebSocket 服务

`--api native` 与 `--api node` 都会在启动时开一个 WebSocket 服务，供第三方客户端读取
播放状态与歌词、并遥控播放（协议与 [MoeKoeMusic](https://github.com/MoeKoeMusic/MoeKoeMusic)
兼容，文档见 [music.moekoe.cn/zh-CN/websocket-api.html](https://music.moekoe.cn/zh-CN/websocket-api.html)）：

| 项 | 值 |
|---|---|
| 默认地址 | `ws://127.0.0.1:6520/` |
| 监听范围 | **只绑 `127.0.0.1`**，不接受非回环连接 |
| 关闭 | `--no-ws`，或配置文件 `ws = false` |
| 改端口 | `--ws-port <PORT>`，或配置文件 `ws_port = 6520` |

服务会推送 `welcome` / `lyrics` / `playerState`，并接受 `control` 命令
（`toggle` / `next` / `prev`）。握手时校验 `Origin`：带浏览器 `Origin` 的跨站握手会被
拒绝（403），无 `Origin` 的本地客户端放行。入站消息与单帧上限 64 KiB。

## 文档

| 文档 | 内容 |
|---|---|
| [docs/INSTALL.md](docs/INSTALL.md) | 环境要求、各平台安装路径、API 服务部署、启动器脚本与环境变量 |
| [KEYBINDINGS.md](KEYBINDINGS.md) | 全部快捷键、鼠标操作、歌曲右键菜单 |
| [docs/USER_GUIDE.md](docs/USER_GUIDE.md) | 界面布局、音源配置、播放队列、登录与云端歌单、桌面集成 |
| [docs/CONFIGURATION.md](docs/CONFIGURATION.md) | 命令行参数、配置文件每一项、会话持久化 |
| [docs/FAQ.md](docs/FAQ.md) | 常见问题与排查 |
| [docs/DESIGN.md](docs/DESIGN.md) | 线程模型、边下边播原理、低资源占用、接口适配（含内存与体积的实测口径） |
| [docs/MAINTENANCE.md](docs/MAINTENANCE.md) | 维护与排障手册：模块地图、风险点、已知坑、改完怎么验 |
| [docs/LICENSES.md](docs/LICENSES.md) | 第三方依赖许可分析 |
| [docs/RELEASE.md](docs/RELEASE.md) | 发版流程、产物清单、版本与标签规则 |
| [CHANGELOG.md](CHANGELOG.md) | 更新日志 |

## 许可与免责

[MIT](LICENSE) © 2026 kugou-tui contributors。

**仅供学习与技术研究的自用工具**，不提供、不托管、不分发任何音乐内容。请尊重音乐版权、
支持正版；通过非官方接口访问可能违反酷狗的服务条款，风险由使用者自行承担。
完整条款见 [docs/DISCLAIMER.md](docs/DISCLAIMER.md)。

## 致谢

- [KuGouMusicApi](https://github.com/MakcRe/KuGouMusicApi) —— 本项目的接口来源。
- [MoeKoeMusic](https://github.com/MoeKoeMusic/MoeKoeMusic) —— 歌词解析、播放模式、
  云端歌单同步的设计参考；「每日领取概念版 VIP」的两步流程也来自它。
- [ratatui](https://github.com/ratatui/ratatui) / [rodio](https://github.com/RustAudio/rodio) /
  [tokio](https://github.com/tokio-rs/tokio) / [ratatui-image](https://github.com/benjajaja/ratatui-image)
  —— 界面、音频、异步运行时与封面渲染。
