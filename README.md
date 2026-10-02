# kugou-tui

**在终端里听酷狗：逐字歌词、真频谱，常驻 15 MiB。**

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

**前提：Node.js ≥ 12。** 接口实现不在本程序里（见「[它依赖什么](#它依赖什么)」）。

**cargo**（推荐）：

```bash
cargo install kugou-tui
kugou-tui
```

首次运行会自动把接口服务准备好并拉起（下载 + 装依赖，约半分钟），之后每次启动直接
复用。想提前把这一步做掉：`kugou-tui --api-start`；想自己管服务：`--no-api-start`。

**预编译包**：[Releases](https://github.com/sijin-xb/kugou-tui/releases) 里三个平台都有，
带着脚本与全部文档。

```bash
# Linux
tar xzf kugou-tui-<版本>-x86_64-unknown-linux-gnu.tar.gz
cd kugou-tui-<版本>-x86_64-unknown-linux-gnu
./scripts/kugou-api-install kugou
./scripts/kugou-tui
```

```powershell
# Windows
Expand-Archive kugou-tui-<版本>-x86_64-pc-windows-msvc.zip -DestinationPath .
cd kugou-tui-<版本>-x86_64-pc-windows-msvc
.\scripts\kugou-api-install.ps1
.\scripts\kugou-tui.ps1
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
| **常驻内存** | 14.2–16.9 MiB（本机实测） | 社区常见 10–25 MiB | 社区常见 15–30 MiB |
| **形态** | 单进程、单二进制（约 7.0 MiB） | 单进程 | 客户端 / 服务端分离 |
| **需要自建服务** | 需要（本机跑第三方 API 服务，程序会自己拉起） | 不需要 | 需要（mpd） |

> 内存那一行是本机实测（112×34 终端，读 `/proc/<pid>/status` 的 `VmRSS`），方法与
> 明细见 [docs/DESIGN.md](docs/DESIGN.md)；cmus / mpd 两行是社区常见量级，未在本机实测。
>
> 一句话：**cmus / mpd 是「放你有的」，kugou-tui 是「放你想听的」。**

## 它依赖什么

**本程序不含任何接口实现**：搜索、取链、歌词、云歌单全部来自本机的第三方服务
[KuGouMusicApi](https://github.com/MakcRe/KuGouMusicApi)（Node.js 写的独立仓库）。
启动时探一次端口，没有就把服务准备好并拉起，退出时只停自己拉起的那个——所以除了
Node.js，用户不需要手工部署任何东西。想手动来一遍、或部署网易云那份服务，见
[docs/INSTALL.md](docs/INSTALL.md#部署第三方-api-服务)。

## 文档

| 文档 | 内容 |
|---|---|
| [docs/INSTALL.md](docs/INSTALL.md) | 环境要求、各平台安装路径、API 服务部署、启动器脚本与环境变量 |
| [KEYBINDINGS.md](KEYBINDINGS.md) | 全部快捷键、鼠标操作、歌曲右键菜单 |
| [docs/USER_GUIDE.md](docs/USER_GUIDE.md) | 界面布局、音源配置、播放队列、登录与云端歌单、桌面集成 |
| [docs/CONFIGURATION.md](docs/CONFIGURATION.md) | 命令行参数、配置文件每一项、会话持久化 |
| [docs/FAQ.md](docs/FAQ.md) | 常见问题与排查 |
| [docs/DESIGN.md](docs/DESIGN.md) | 线程模型、边下边播原理、低资源占用、接口适配 |
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
