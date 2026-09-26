# kugou-tui

**在终端里听酷狗：逐字歌词、真频谱，常驻 15 MiB。**

![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)
![Rust](https://img.shields.io/badge/rust-1.86%2B-orange.svg)
![Platform](https://img.shields.io/badge/platform-linux-lightgrey.svg)

### 为什么做这个

我大部分时间在终端里。想听首歌，得切窗口、开客户端、看广告、被推荐一堆不想听的东西——
而我当时只想放一首刚想到的歌。cmus 和 mpd 很好，但它们放的是**我硬盘上的文件**；
我的歌在云端，在酷狗的曲库里。

所以写了这个：一个终端里的酷狗播放器。没有窗口，没有广告，没有推荐流。
敲 `2` 搜歌，`Enter` 播放，`L` 扫码登录把云端歌单接进来。
它只做「找到歌 → 放出来 → 把歌词和频谱画好看」这一件事。

![kugou-tui：歌单广场、正在播放、逐字歌词与播放队列](assets/screenshot-0.3.3.jpg)

*主界面：左侧导航，中间逐字歌词，右侧播放队列与封面。画面全部由程序渲染（终端文本 + 半块字符画），截图取自真实使用场景。*

### 三个核心卖点

1. **逐字歌词**。解析酷狗 KRC 的**每字时间戳**，按每个字自己的进度在底色与强调色之间
   插值——边界字是渐变过渡，仿 Apple Music 的推进效果；非当前行按距离线性变暗。
   不是「整行一起亮」那种 LRC。
2. **真频谱**。对音频线程采集的真实采样做 FFT，按对数分频到 40Hz–16kHz，
   贝斯亮左、镲片亮右。有真实音频输入，不是装饰性动画。
3. **酷狗曲库 + 云端歌单**。搜歌即播，不用先有本地文件；歌单广场、歌手、排行榜、
   个人云端歌单都在；登录二维码直接画在终端里（`L`），不需要手机以外的任何工具。

### 安装

从源码构建（AUR 包**计划中，尚未上架**；不想装 Rust 工具链的话，用
[Release 里的预编译包](https://github.com/sijin-xb/kugou-tui/releases)，它带着脚本与文档，解压即用）：

```bash
git clone https://github.com/sijin-xb/kugou-tui.git
cd kugou-tui && cargo build --release
```

装完第一次运行：

```bash
./scripts/kugou-api-install kugou   # 拉取并配置酷狗接口服务（约 1 分钟，只需一次）
./scripts/kugou-tui                 # 开播
```

> **本程序不含任何接口实现**，数据全部来自本机的第三方服务
> [KuGouMusicApi](https://github.com/MakcRe/KuGouMusicApi)。上面那条
> `kugou-api-install` 就是把「clone → 装依赖 → 配端口」做完，之后启动器会在每次开播前
> 按需把服务拉起来。想手动来一遍、要装启动器到 `~/.local/bin`、或要部署网易云音源，
> 见 **[docs/INSTALL.md](docs/INSTALL.md)**。

### 功能总览

| 分类 | 能力 |
|---|---|
| 浏览 | 歌单广场、歌手列表（可按地区筛选）、排行榜、个人云端歌单 |
| 检索 | 单曲搜索，结果按服务端相关性排序，`M` 加载更多 |
| 播放 | 播放/暂停、上下首、±5 秒跳转、音量、静音；顺序 / 列表循环 / 单曲循环 / 随机 |
| 歌词 | 逐字高亮（KRC）、译文与音译、卡拉 OK 式居中滚动、±100 ms 偏移微调 |
| 可视化 | 真频谱：FFT + 对数分频，贝斯亮左、镲片亮右 |
| 播放队列 | 追加（`a`）、插播下一首（`i`）、整列表加入（`A`）、移除（`x`）、清空（`X`） |
| 云端歌单 | 收藏单曲（`s`）、整个队列同步（`S`）、增删歌单（`N` / `D`） |
| 登录 | 应用内扫码（`L`），二维码直接画在终端里 |
| 桌面集成 | MPRIS（`playerctl` 可控）+ 系统托盘（右键菜单，Quickshell / waybar / KDE） |
| 输入与外观 | 键盘 + 鼠标；6 套主题（真彩 / 16 色各一版）；音频落盘缓存 + LRU 回收 |

完整能力（含网络重试策略、概念版 VIP 自动领取等）见
[docs/USER_GUIDE.md](docs/USER_GUIDE.md#功能一览)。

### 和 cmus / mpd + ncmpcpp 比

|  | **kugou-tui** | cmus | mpd + ncmpcpp |
|---|---|---|---|
| **曲库** | **酷狗在线曲库**：搜歌即播，不需要本地文件 | 只放本地文件 | 只放本地文件（无内置在线源） |
| **逐字歌词** | **支持**：KRC 每字时间戳，按字推进的渐变高亮 | 整行 LRC | 整行 LRC |
| **频谱** | **内置**，零配置 | 无 | 有，但要额外配 mpd 的 fifo 音频输出 |
| **云端歌单** | **支持**：酷狗账号的云端歌单，终端内扫码登录 | 不支持 | 不支持 |
| **常驻内存** | 14.2–16.9 MiB | 社区常见量级 10–25 MiB | 合计常见量级 15–30 MiB |
| **形态** | 单进程、单二进制（约 7.0 MiB） | 单进程 | 客户端 / 服务端分离（mpd 常驻 + 前端） |
| **需要自建服务** | 需要（本机跑第三方 API 服务，启动器会按需拉起） | 不需要 | 需要（mpd） |

> kugou-tui 的内存是**本机实测**（112×34 终端，读 `/proc/<pid>/status` 的 `VmRSS`，
> 四个场景 14.2–16.9 MiB，方法与明细见 [docs/DESIGN.md](docs/DESIGN.md)）；
> cmus 与 mpd 两行是**社区常见量级，未在本机实测**，仅供数量级参考。
>
> 边下边播的字节缓冲**不保留整首**：下载先落盘，内存里只留一个 4 MiB 的尾部窗口，
> 窗口之外的字节从落盘文件读回来。所以放几十 MB 的 Hi-Res 也不会让内存按曲目体积涨
> （实测 64 MiB 的曲子占 12.5 MiB；切歌时还会主动取消上一首的下载）。
>
> 一句话：**cmus / mpd 是「放你有的」，kugou-tui 是「放你想听的」。**

### 文档

| 文档 | 内容 |
|---|---|
| [docs/INSTALL.md](docs/INSTALL.md) | 环境要求、三种安装路径、API 服务部署、启动脚本与环境变量 |
| [KEYBINDINGS.md](KEYBINDINGS.md) | 全部快捷键、鼠标操作、歌曲右键菜单 |
| [docs/USER_GUIDE.md](docs/USER_GUIDE.md) | 功能一览、界面布局、音源配置、播放队列、登录与云端歌单、桌面集成 |
| [docs/CONFIGURATION.md](docs/CONFIGURATION.md) | 命令行参数、配置文件每一项、会话持久化 |
| [docs/FAQ.md](docs/FAQ.md) | 常见问题与排查 |
| [docs/DESIGN.md](docs/DESIGN.md) | 线程模型、边下边播原理、低资源占用、接口适配 |
| [docs/MAINTENANCE.md](docs/MAINTENANCE.md) | 维护与排障手册：模块地图、风险点、已知坑、改完怎么验 |
| [docs/LICENSES.md](docs/LICENSES.md) | 第三方依赖许可分析 |
| [docs/RELEASE.md](docs/RELEASE.md) | 发版流程与依赖顺序、发行版产物清单、版本与标签规则、回收逻辑 |
| [CHANGELOG.md](CHANGELOG.md) | 更新日志 |

### 许可与免责

[MIT](LICENSE) © 2026 kugou-tui contributors。

**仅供学习与技术研究的自用工具**，不提供、不托管、不分发任何音乐内容；不含任何接口实现，
全部数据来自第三方项目 [KuGouMusicApi](https://github.com/MakcRe/KuGouMusicApi)。
请尊重音乐版权、支持正版。通过非官方接口访问可能违反酷狗的服务条款，风险由使用者自行承担。
完整条款见 [docs/DISCLAIMER.md](docs/DISCLAIMER.md)。

### 致谢

- [KuGouMusicApi](https://github.com/MakcRe/KuGouMusicApi) —— 本项目的接口来源。
  所有接口路径都对照其 `docs/README.md` 核对过。
- [MoeKoeMusic](https://github.com/MoeKoeMusic/MoeKoeMusic) —— 歌词解析、播放模式、
  云端歌单同步的设计参考；「每日领取概念版 VIP」的两步流程（领一天 → 升级为畅听 VIP）
  也来自它的 `getVip()`。
- [KugouMusic.NET](https://github.com/Linsxyx/KugouMusic.NET) —— 接口封装思路参考。
- [ratatui](https://github.com/ratatui/ratatui) / [rodio](https://github.com/RustAudio/rodio) /
  [tokio](https://github.com/tokio-rs/tokio) —— 本项目的三块基石。
- [ratatui-image](https://github.com/benjajaja/ratatui-image) —— 封面渲染。它把图片
  写进 ratatui 的 Buffer 而不是自己写 stdout，并负责探测 kitty / iTerm2 / sixel
  协议（都不支持时退到彩色半块）。
