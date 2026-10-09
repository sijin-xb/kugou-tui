# 安装

本文是 [README](../README.md) 的展开版：环境要求、各平台的安装路径、第三方 API 服务怎么部署、
启动器脚本与环境变量、装完先做什么。

**先看这一句**：接口实现**内嵌在二进制里**，默认路径（`--api native`）**不需要 Node.js**。
只有走 `--api node` 回退（或网易云音源）时，才需要本机有一个第三方 API 服务
（见「[部署第三方 API 服务](#部署第三方-api-服务)」）。

---

## 环境要求

| 项目 | 要求 |
|---|---|
| Node.js | **仅 `--api node` 回退需要**，≥ 12（含 npm），用于运行 KuGouMusicApi。`--api native` 与 `--no-default-features` 构建都不需要 |
| Rust 工具链 | 1.90+（edition 2024）。下限由**依赖**顶上去（`quantette` 要 1.90），不是本项目代码决定的 |
| 音频输出 | rodio 支持的后端：Linux 是 ALSA / PipeWire / PulseAudio，Windows 是 WASAPI，macOS 是 CoreAudio |
| 终端 | 支持 UTF-8；**真彩（24 位）** 才有完整的主题配色与逐字渐变，老终端可加 `--basic-color` 退回 16 色 |
| 操作系统 | **Linux**（主力，在 CachyOS 上实测）、**Windows 10/11 x86_64**、**macOS**（CI 覆盖但未真机长期使用） |
| 构建依赖（Linux） | `alsa-lib` 开发头文件 + `pkg-config`（`cpal` 通过 `alsa-sys` 链接它）。Debian/Ubuntu 是 `libasound2-dev pkg-config`，Arch 是 `alsa-lib`。桌面发行版一般已有，**干净的容器 / 服务器上没有** |
| 构建依赖（Windows） | 只需 MSVC 工具链。**不需要 CMake、NASM 或 OpenSSL** |
| 构建依赖（macOS） | 只需 Xcode 命令行工具（`xcode-select --install`） |
| D-Bus（可选） | 有 session bus 时自动启用 MPRIS 与系统托盘；没有则跳过，不影响播放。Windows 上相关代码不参与编译 |
| 系统托盘（可选） | 需要状态栏提供 `org.kde.StatusNotifierWatcher`（Quickshell / waybar / KDE 都有）。面板后启动也不要紧——托盘线程每 5 秒重试注册 |

---

## 安装路径

### 路径一：`cargo install`（crates.io）

```bash
cargo install kugou-tui
kugou-tui --api native      # 内嵌后端，不需要 Node
```

内嵌后端**开箱即用**，没有「首次准备服务」这一步。想用 Node 回退（或跑网易云音源）时
才需要下面这套：首次启动会自己把接口服务准备好并拉起（下载钉住的上游提交 →
`npm install --omit=dev` → 起服务，实测约 25 秒），之后每次启动探到端口就复用。

| 项 | 说明 |
|---|---|
| 首次启动（仅 `--api node`） | 需要网络。想提前做完：`kugou-tui --api node --api-start`（它拉起的服务留在后台，`--api-stop` 停） |
| 服务装在哪 | 优先 `/usr/share/kugou-tui/api/kugou`（发行包）与已存在的 `~/KuGouMusicApi`，都没有才装到 `~/.local/share/kugou-tui/api/kugou` |
| 想自己管服务 | 配置 `api_auto_start = false`，或 `--no-api-start` |
| 退出行为 | 自己拉起的实例随退出停止，不留常驻 node；已在跑的（你或别的实例起的）不动 |
| 想彻底去掉引导代码 | `cargo install kugou-tui --no-default-features`（关掉 `node-bootstrap` feature，二进制里不含下载器 / npm / spawn node 的代码） |

> 这条路拿不到仓库里的 `scripts/*`（它们不是 crate 的一部分）。要那套一键脚本就用下面的
> 预编译包。

### 路径二：从源码构建

```bash
git clone https://github.com/sijin-xb/kugou-tui.git
cd kugou-tui
cargo build --release
./target/release/kugou-tui --help
```

release 产物约 **11.7 MiB**（`opt-level="z"` + fat LTO + strip）。Windows 上把二进制路径换成
`.\target\release\kugou-tui.exe`，前置与脚本见「[在 Windows 上构建与运行](#在-windows-上构建与运行)」。

### 路径三：预编译二进制（GitHub Release）

[Releases](https://github.com/sijin-xb/kugou-tui/releases) 里三个平台都有，内容一致
（二进制 + 脚本 + 全部文档）：

| 平台 | 文件 |
|---|---|
| Linux x86_64 | `kugou-tui-<版本>-x86_64-unknown-linux-gnu.tar.gz` |
| Windows x86_64 | `kugou-tui-<版本>-x86_64-pc-windows-msvc.zip` |
| macOS arm64 | `kugou-tui-<版本>-aarch64-apple-darwin.tar.gz` |

> Intel Mac 没有预编译包（CI runner 是 arm64），从源码构建即可。

**Linux**：

```bash
tar xzf kugou-tui-<版本>-x86_64-unknown-linux-gnu.tar.gz
cd kugou-tui-<版本>-x86_64-unknown-linux-gnu

# 二进制与脚本都链进 PATH。`scripts/kugou-tui` 与二进制同名，链过去要改名
mkdir -p ~/.local/bin
ln -s "$PWD/kugou-tui"                 ~/.local/bin/kugou-tui
ln -s "$PWD/scripts/kugou-api"         ~/.local/bin/kugou-api
ln -s "$PWD/scripts/kugou-api-install" ~/.local/bin/kugou-tui-install-api
ln -s "$PWD/scripts/kugou-tui"         ~/.local/bin/kugou-tui-launch

kugou-tui-install-api kugou   # 一次性：拉取并配置接口服务
kugou-tui                     # 开播
```

**Windows**：

```powershell
Expand-Archive kugou-tui-<版本>-x86_64-pc-windows-msvc.zip -DestinationPath .
cd kugou-tui-<版本>-x86_64-pc-windows-msvc
.\scripts\kugou-api-install.ps1   # 一次性
.\scripts\kugou-tui.ps1           # 开播
```

**macOS**（脚本是 bash，用法同 Linux）：

```bash
tar xzf kugou-tui-<版本>-aarch64-apple-darwin.tar.gz
cd kugou-tui-<版本>-aarch64-apple-darwin
./scripts/kugou-api-install kugou
./scripts/kugou-tui
```

> 包是自足的（含脚本与文档），但**不含接口服务本身**：`kugou-api-install` 需要
> `node` / `npm` / `git` 与网络（它要 clone 服务并装依赖）。
>
> Windows 的 zip 里同时带着 bash 版脚本（方便 Git Bash / WSL）；反过来 Linux / macOS 的
> tarball 里没有 PowerShell 脚本。

### 路径四：AUR（计划中，尚未上架）

```bash
paru -S kugou-tui          # 或 yay -S kugou-tui
```

目标形态是「装完直接 `kugou-tui` 就能听」：包把**接口服务连同生产依赖**放进
`/usr/share/kugou-tui/api/kugou/`，程序会优先用它，因此不需要再跑一次 npm install。

> 之所以能把服务打进包里，是因为它**不往自己目录写任何文件**（源码里没有 `writeFile` /
> `mkdirSync`），从只读目录跑完全正常——这条是实测过的。
>
> 网易云那份服务不在包里（只在用网易云音源时才需要），仍走 `kugou-tui-install-api netease`。

---

## 在 Windows 上构建与运行

日常使用**不需要手动起服务**：`scripts/` 下的 PowerShell 脚本对应 Unix 侧的 bash 脚本。

| Windows | Unix 侧对应 | 干什么 |
|---|---|---|
| `scripts/kugou-api-install.ps1` | `kugou-api-install` | 拉取接口服务 + 装依赖（只需一次） |
| `scripts/kugou-tui.ps1` | `kugou-tui` | 启动器：需要服务时确保它在跑，然后进播放器 |
| `scripts/kugou-api.ps1` | `kugou-api` | 服务启停管理：`start` / `stop` / `restart` / `status` / `logs` |
| `scripts/build-windows.ps1` | `make-release-tarball` | 构建 + 打包 zip |

这几个脚本**只依赖 PowerShell 5.1**（Windows 自带），不需要额外装 pwsh 7。它们与启动器
共用同一套 PID 文件（缓存目录里的 `api-<实例>.pid`，内容 `<PID> <端口>`），所以启动器拉起的
实例，`kugou-api.ps1` 认得出、停得掉——不必去任务管理器按名字猜着杀 node。

### 1. 装工具链

| 要装的东西 | 怎么装 |
|---|---|
| Rust | [rustup.rs](https://rustup.rs) → 默认选 `x86_64-pc-windows-msvc` |
| MSVC 工具链 | Visual Studio 生成工具，勾「使用 C++ 的桌面开发」；或装了 VS 就有 |
| Node.js | [nodejs.org](https://nodejs.org) |
| Git | [git-scm.com](https://git-scm.com)，安装脚本要用它 clone |

> **不需要 CMake / NASM / OpenSSL**：Windows 上 TLS 走系统自带的 SChannel 而不是 rustls
> （后者的 `aws-lc-sys` 是 C 代码，要 CMake + NASM）。见 `Cargo.toml` 里 `cfg(windows)` 的注释。

### 2. 构建

```powershell
git clone https://github.com/sijin-xb/kugou-tui.git
cd kugou-tui
cargo build --release
.\target\release\kugou-tui.exe --help

.\scripts\build-windows.ps1        # 打包：dist\kugou-tui-<版本>-x86_64-pc-windows-msvc.zip
```

### 3. 拉取接口服务（只需一次）

```powershell
.\scripts\kugou-api-install.ps1
# 等价于：clone 到 %USERPROFILE%\KuGouMusicApi → 钉到验证过的提交 → npm install --omit=dev

.\scripts\kugou-api-install.ps1 -Dir D:\apps\KuGouMusicApi   # 换目录
.\scripts\kugou-api-install.ps1 netease                      # 网易云音源
```

### 4. 开播

```powershell
.\scripts\kugou-tui.ps1
.\scripts\kugou-tui.ps1 -s 海阔天空      # 参数原样透传给播放器

.\scripts\kugou-tui.ps1 --dry-run        # 只打印决策，不启动任何东西
# 配置        : C:\Users\you\AppData\Roaming\kugou-tui\config.toml
# 当前音源    : kugou_concept
# 接口后端    : native
# 接口服务    : 不需要（内嵌后端（--api native）在进程内实现酷狗接口，不需要 Node.js）
```

默认的 `--api native` 不需要本机服务，启动器直接进播放器。只有 `--api node` 才会走
「检查 → 拉起 → 等待就绪」（约 20 秒超时），那时 dry-run 打的是端口与目录：

```powershell
.\scripts\kugou-tui.ps1 --dry-run --api node
# 接口后端    : node
# 探测地址    : http://127.0.0.1:3001（端口 3001，平台 lite）
# 服务目录    : C:\Users\you\KuGouMusicApi
# 服务在跑吗  : 没起
```

也可以完全手动，两个终端各跑一条（仅在 `--api node` 路径上需要）：

```powershell
cd $env:USERPROFILE\KuGouMusicApi; $env:PORT=3000; node app.js
.\target\release\kugou-tui.exe
```

### 5. 终端要求

**用 Windows Terminal，不要用旧的「命令提示符」窗口。** 程序需要
`ENABLE_VIRTUAL_TERMINAL_PROCESSING`（交替屏幕、真彩、鼠标上报）与 UTF-8 输出，
Windows Terminal 默认满足；老 conhost 在中文与颜色上会有明显残缺。

封面在 Windows Terminal 里会退回**半块字符画**（它不支持 Kitty / iTerm2 图形协议，程序探测
不到就自动降级，属预期行为；想省掉这份开销可以开 `lite_mode`）。装了 Nerd Font 的话加一个
环境变量，图标才不会退成 ASCII：

```powershell
$env:KUGOU_TUI_NERD_FONT = "1"
```

### 6. 与 Linux 的功能差异

| 能力 | Linux | Windows |
|---|---|---|
| 播放、搜索、歌词、封面、缓存 | ✅ | ✅ 相同 |
| MPRIS（桌面媒体控件、`playerctl`） | ✅ | ❌ D-Bus 接口，Windows 没有 |
| 系统托盘（StatusNotifierItem） | ✅ | ❌ 同上 |
| 最小化窗口 | ✅ 仅 niri 下（入口在**托盘菜单**里） | ❌ 走 compositor IPC，无对应实现 |
| 配置文件位置 | `~/.config/kugou-tui/config.toml` | `%APPDATA%\kugou-tui\config.toml` |
| 缓存与日志位置 | `~/.cache/kugou-tui/` | `%LOCALAPPDATA%\kugou-tui\` |
| 启动器 / 服务安装器 | `scripts/` 下的 bash 版 | `scripts/` 下的 `.ps1` 版（功能对应） |

三条 ❌ 都是**降级而不是故障**：相关代码不参与编译，界面上也不会出现点了没反应的入口。
用 `--print-config` 可以确认（会打印「系统托盘 : 不可用（windows 无 D-Bus）」）。

### 7. 改代码时怎么确认没弄坏 Windows

CI 里有一栏 `windows-latest`，每次推送都会真的在 Windows 上跑 clippy + test + build + 打包。
本地想先查一遍（不需要 Windows 机器）：

```bash
rustup target add x86_64-pc-windows-msvc
cargo check  --target x86_64-pc-windows-msvc --all-targets
cargo clippy --target x86_64-pc-windows-msvc --all-targets -- -D warnings
```

最后一步链接需要 MSVC 工具链，Linux 上跑到 `error: linker link.exe not found` 就说明 Rust 侧
全部通过了。每处平台分支为什么存在，见 [MAINTENANCE.md](MAINTENANCE.md) 的「平台分支在哪」。

---

## 在 macOS 上构建与运行

**支持等级：代码走与 Linux 同一套 `cfg(unix)` 分支，CI 在 `macos-latest` 上跑
clippy + test + build，但没有在真机长期使用过。**

```bash
git clone https://github.com/sijin-xb/kugou-tui.git
cd kugou-tui && cargo build --release

./scripts/kugou-api-install kugou
./scripts/kugou-tui
```

`aws-lc-sys`（随 rustls 引入）是 C 代码，靠 Xcode 的 clang 编译；macOS 不需要 ALSA 那套依赖，
也不需要额外装 CMake。

> **脚本刻意避开了 GNU 专有的东西**（macOS 自带 BSD 工具链 + bash 3.2）：不用
> `readlink -f`、`dirname --`、`seq`、`setsid`、`ss`、`declare -A`。替代写法与原因写在脚本
> 文件头的「可移植性」一节，改脚本前先读那段。
>
> 目录对齐了 `dirs` 在 macOS 上的取值（`~/Library/Application Support` 与 `~/Library/Caches`，
> **不是** `~/.config` / `~/.cache`）——不对齐的话启动器会去找一个永远不存在的 `config.toml`。
>
> 从浏览器下载的 tar 解出来的二进制带 `com.apple.quarantine` 属性，而包里没有签名，直接跑会被
> Gatekeeper 杀掉（终端只显示 `Killed: 9`，看起来像程序自己的 bug）。启动器会检测并提示；
> 手动处理是 `xattr -d com.apple.quarantine ./kugou-tui`。

### 与 Linux 的差异

| 项目 | macOS 上的表现 |
|---|---|
| 音频后端 | CoreAudio（cpal 自动选），不需要配设备 |
| MPRIS / 系统托盘 | **不可用**（D-Bus 接口，macOS 默认无 session bus）；启动时写一条 WARN 然后跳过 |
| 最小化窗口 | 不可用（走 niri 的 compositor IPC，`NIRI_SOCKET` 不存在时入口直接不出现） |
| 配置文件 | `~/Library/Application Support/kugou-tui/config.toml`（**不是** `~/.config`） |
| 缓存与日志 | `~/Library/Caches/kugou-tui/` |
| 默认下载目录 | `~/Music` |
| 封面 | iTerm2 走图形协议；Terminal.app 退回半块字符画（预期行为） |
| Nerd Font 探测 | 没有 fontconfig，`fc-list` 探测一律落空 → 退 ASCII。装了 Nerd Font 就设 `KUGOU_TUI_NERD_FONT=1` |

---

## 部署第三方 API 服务

> **这一节只在你要用 `--api node` 回退（或网易云音源）时才需要。**
> 默认的内嵌后端（`--api native`）把接口实现在进程内，不碰这一节。

**`cargo install` 装的、或用过预编译包 / AUR 的，这一节基本不用看**——程序启动时会自己
探活、缺了就装好并拉起。下面这套是给「要手动控制」和「要装网易云那份服务」的人准备的。

酷狗接口的**协议**来自独立仓库 [KuGouMusicApi](https://github.com/MakcRe/KuGouMusicApi)
（本仓库没有 submodule，也没有 vendor 目录）；内嵌后端是照它逐项移植的纯 Rust 实现，
`--api node` 回退则是直接跑那个 Node 服务。

### 一键

```bash
./scripts/kugou-api-install kugou
```

它会 clone 到 `~/KuGouMusicApi`、`npm install`，然后调 `scripts/kugou-api start` 把标准版
（:3000）与概念版（:3001）两个实例都拉起来。不带参数运行会列出各音源的仓库、**钉住的提交**
与当前运行状态。

### 手动

```bash
git clone https://github.com/MakcRe/KuGouMusicApi.git
cd KuGouMusicApi
git checkout a5a98013cce79fe0ae2ad65fc84b68176ebcfc1e   # 钉到验证过的提交
npm install
npm start          # 注意是 npm start，不是 npm run dev
```

> **为什么要钉提交**：上游是活跃仓库，接口字段会变，跟 master 可能某天就解析不出歌名或歌词。
> 这个提交号在 `scripts/kugou-api-install` 里也有一份，脚本按它做浅取
> （`git fetch --depth 1 origin <sha>`）。换验证过的提交时两处一起改。
>
> **概念版（lite）**要带 `platform=lite` 启动：`platform=lite PORT=3001 npm start`。
> 不加这个环境变量，概念版搜索拿不到正确结果。
>
> `npm run dev` 走的是 nodemon（一个 devDependency），只装过生产依赖的环境里会直接失败。

服务默认监听 `http://127.0.0.1:3000`，验证：`curl -s "http://127.0.0.1:3000/register/dev"`。
**只用酷狗标准版的话，起这一个实例就够了。**

### 网易云音源

走另一套服务（NeteaseCloudMusicApi），需要单独部署，方式与配置见
[USER_GUIDE.md](USER_GUIDE.md#网易云音源)。

---

## 启动器脚本（可选）

| 脚本 | 作用 |
|---|---|
| `scripts/kugou-tui` | 启动播放器；**需要**本机 API 服务时，没起就自动拉起并等待就绪 |
| `scripts/kugou-api` | 一次拉起**两个** API 实例（标准版 + 概念版） |

> `scripts/kugou-tui` 认后端：默认的 `--api native` 把酷狗接口实现在进程内，本机不需要
> KuGouMusicApi、也不需要 Node.js，启动器直接进播放器，**不探端口、不拉进程**。只有
> `--api node`（或配置里 `api_backend = "node"`）才会走下面的「检查 → 拉起 → 等待就绪」。
> 网易云与汽水音源的接口不在内嵌实现里，照旧需要本机服务（汽水直连公网，是唯一例外）。

```bash
ln -s "$PWD/scripts/kugou-tui" "$PWD/scripts/kugou-api" ~/.local/bin/

kugou-api start     # 启动两个实例（已在跑的会跳过），打印 PID / 日志路径 / 访问地址
kugou-api status    # 查看状态（端口被别人占着也会如实说明）
kugou-api logs      # 跟随日志（`kugou-api logs lite` 跟概念版）
kugou-api restart   # 先停再起（改了端口/配置后用它）
kugou-api stop      # 停止（按 PID 精确停止）
kugou-api help      # 完整说明
```

`start` 之前会做一轮预检，缺什么补什么：`node`、KuGouMusicApi 目录、`node_modules`（缺了自动
`npm install --omit=dev`）、客户端二进制（缺了自动 `cargo build --release`）。任一步失败都会
带着明确原因终止，不留半死不活的进程。不想让它碰编译就设 `KUGOU_API_SKIP_BUILD=1`
（`stop` / `status` 本来就不触发编译）。

参数按**环境变量 > 配置文件 > 默认值**取值。配置文件是可选的
`<配置根>/kugou-tui/api.env`（Linux 是 `~/.config/kugou-tui/api.env`，macOS 是
`~/Library/Application Support/kugou-tui/api.env`，两者都可用 `KUGOU_TUI_CONFIG_DIR` 覆盖），
每行一个 `KEY=VALUE`。例：`KUGOU_STANDARD_PORT=3100 kugou-api restart`。

`kugou-api` 认这些：

| 变量 | 默认值 | 说明 |
|---|---|---|
| `KUGOU_API_DIR` | `/usr/share/kugou-tui/api/kugou`（软件包提供时）或 `$HOME/KuGouMusicApi` | 服务所在目录 |
| `KUGOU_API_SYSTEM_ROOT` | `/usr/share/kugou-tui/api` | 软件包放服务的位置（给非 `/usr` 前缀的打包用） |
| `KUGOU_API_LOG_DIR` | `$XDG_CACHE_HOME/kugou-tui` | 日志与 PID 文件目录 |
| `KUGOU_API_HOST` | `127.0.0.1` | 监听地址 |
| `KUGOU_STANDARD_PORT` | `3000` | 标准版端口 |
| `KUGOU_LITE_PORT` | `3001` | 概念版端口 |
| `KUGOU_API_BIN` | `<仓库>/target/release/kugou-tui`，不存在则用 `PATH` 上的 | 客户端二进制路径 |
| `KUGOU_API_SKIP_BUILD` | 空 | 设为 `1` 跳过编译预检 |
| `KUGOU_API_CONFIG` | `$XDG_CONFIG_HOME/kugou-tui/api.env` | 配置文件路径 |

`scripts/kugou-tui`（启动器）认另一组：

| 变量 | 默认值 | 说明 |
|---|---|---|
| `KUGOU_API_BACKEND` | 配置文件里的 `api_backend`，再退到 `native` | 与程序同名变量一致。`native` 时启动器不碰本机服务；`--api <node\|native>` 参数优先于它 |
| `KUGOU_API_BASE` | **不设置** | 设了才给程序传 `--api-base` 并拿它探活；不设时让程序读配置里选中的音源 |
| `KUGOU_API_DIR` | `$HOME/KuGouMusicApi` | 服务所在目录，用于自动拉起（仅 `--api node` 时用得上） |
| `KUGOU_API_LOG` | `$XDG_CACHE_HOME/kugou-tui/api.log` | 服务日志路径 |
| `KUGOU_TUI_BIN` | 自动探测 | 手动指定二进制路径。默认先找 `脚本目录/../target/release/kugou-tui`，再找 `PATH` 上的 |
| `API_PORT` | `3000` | 自动拉起服务时用的端口 |

`kugou-tui --dry-run` 会打印它算出的后端与是否需要服务，排查「为什么它没起服务 / 为什么它起了服务」时用：

```
接口后端    : native
接口服务    : 不需要（内嵌后端（--api native）在进程内实现酷狗接口，不需要 Node.js）
```

### 不想让 node 常驻？

只有在 `--api node` 回退路径上才会有 node 进程；默认的 `--api native` 根本不拉它，
这一节可以跳过。

`scripts/kugou-tui` 用 `setsid` 把服务留在后台、退出时**不回收**（为了下次开播秒起）。
不想留常驻进程的话，两种做法：

- 直接用程序内建的引导：`kugou-tui` 每次启动探一次端口，没有就拉起，**退出时只停自己拉起的
  那个**（脚本起的不动）。想常驻就用 `--api-start` / `--api-stop`。
- 想在启动前顺手 `git pull` 更新服务代码（程序不做这件事），写个 shell 函数把
  「pull → 缺依赖就装 → 没跑就起 → 进播放器 → 退出时收掉自己起的」串起来即可。三段的关键
  写法：fish 里用 `env PORT=$port nohup node app.js --port=$port &`（不要 `setsid`，否则
  `$last_pid` 拿到的是 setsid 而不是 node，`kill` 杀不到真正的服务）。

---

## 装完先做什么

**不登录也能听歌**，搜索和云端歌单才需要账号。

1. 启动：`kugou-tui`（默认内嵌后端，不需要先准备任何服务）。
2. 按 **`3`** 进歌单广场 → `Enter` 打开一个歌单 → 移动光标 → `Enter` 播放。

> 只有走 `--api node` 回退路径时才需要先确认 KuGouMusicApi 在跑
> （`curl -s http://127.0.0.1:3000/`）；启动器会自动把它拉起来。

数字键落点：`1` 首页、`2` 搜索、`3` 歌单、`4` 歌手、`5` 排行榜、`6` 云端、`7` 队列、
`8` 音源、`9` 设置、`0` 可视化（完整表见 [KEYBINDINGS.md](../KEYBINDINGS.md)）。想搜歌按 `2`，
首次需要按 `L` 扫码登录。

> 首次启动会自动获取设备指纹 `dfid` 并写入配置（取播放直链需要它），不需要你做任何事。

---

## 相关

- 快捷键全表：[KEYBINDINGS.md](../KEYBINDINGS.md)
- 界面布局与音源配置：[USER_GUIDE.md](USER_GUIDE.md)
- 配置文件每一项：[CONFIGURATION.md](CONFIGURATION.md)
- 装完出问题：[FAQ.md](FAQ.md)
- 排查「内存只涨不落」：`KUGOU_TUI_MEM_TRACE=1 kugou-tui`，日志里每 5 秒一行 RSS
