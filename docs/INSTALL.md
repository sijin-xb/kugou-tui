# 安装

本文是 [README](../README.md) 的展开版：环境要求、安装路径、第三方 API 服务怎么部署、
启动器脚本与环境变量、装完先做什么。

---

## 环境要求

| 项目 | 要求 |
|---|---|
| Rust 工具链 | **1.90+**（edition 2024）。下限由**依赖**顶上去（`quantette` 要 1.90），不是本项目代码决定的，见 `Cargo.toml` |
| Node.js | 用于运行 KuGouMusicApi（上游 `engines` 要求 **12+**） |
| 音频输出 | 任意 rodio 支持的后端（Linux 上为 ALSA/PulseAudio，Windows 上为 WASAPI，macOS 上为 CoreAudio） |
| 终端 | 支持 UTF-8；**真彩（24 位）** 才能看到完整的主题配色与逐字渐变，老终端可加 `--basic-color` 退回 16 色 |
| 操作系统 | **Linux**（主力，在 CachyOS 上实测）、**Windows 10/11 x86_64**（见「[在 Windows 上构建与运行](#在-windows-上构建与运行)」）、**macOS**（CI 覆盖但未真机长期使用，见「[在 macOS 上构建与运行](#在-macos-上构建与运行)」） |
| 构建依赖（Linux） | `alsa-lib` 的开发头文件与 `pkg-config`——`cpal` 通过 `alsa-sys` 链接它。Debian/Ubuntu 上是 `libasound2-dev pkg-config`，Arch 上是 `alsa-lib`。桌面发行版一般已经装好，**干净的容器 / 服务器上不是**，缺了会在 `alsa-sys` 编译时报错 |
| 构建依赖（Windows） | 只需 **MSVC 工具链**（VS 生成工具 / Build Tools 里的「使用 C++ 的桌面开发」）。**不需要 CMake、NASM 或 OpenSSL** —— 见下文 |
| 构建依赖（macOS） | 只需 Xcode 命令行工具（`xcode-select --install`） |
| D-Bus（可选） | 有 session bus 时自动启用 MPRIS 与系统托盘；没有（纯 tty / macOS）则跳过，**不影响播放**。**Windows 上没有这套东西，相关代码不参与编译** |
| 系统托盘（可选） | 需要状态栏提供 `org.kde.StatusNotifierWatcher`（Quickshell / waybar / KDE 都有）。没有就静默跳过；不需要时可用 `--no-tray` 关闭 |

---

## 安装路径

### 路径一：从源码构建（当前可用）

```bash
git clone https://github.com/sijin-xb/kugou-tui.git
cd kugou-tui
cargo build --release
./target/release/kugou-tui --help
```

release 产物约 **7.0 MiB**（7,317,024 字节；`opt-level="z"` + fat LTO + strip）。

Windows 上把 `./target/release/kugou-tui` 换成 `.\target\release\kugou-tui.exe`，
详细前置与脚本见「[在 Windows 上构建与运行](#在-windows-上构建与运行)」。

### 路径二：AUR（计划中，尚未上架）

上架后目标是这样，现在**还跑不通**：

```bash
paru -S kugou-tui          # 或 yay -S kugou-tui
```

包会把**整套东西**装上，装完直接 `kugou-tui` 就能听：

- 主程序、`kugou-api`、`kugou-tui-install-api`、`kugou-tui-launch` 四个可执行文件；
- **第三方接口服务连同它的生产依赖**，放在 `/usr/share/kugou-tui/api/kugou/`
  ——所以不需要再跑一次 `kugou-tui-install-api` 等 npm install。

> 之所以能把服务打进包里，是因为它**不往自己目录写任何文件**（源码里没有
> `writeFile` / `mkdirSync`），从只读目录跑完全正常——这条是实测过的。
> 服务只读、依赖随包，`/usr` 不会被 npm 污染，也不需要常驻进程。
>
> 网易云那份服务不在包里（它只在用网易云音源时才需要），仍然走
> `kugou-tui-install-api netease` 拉取。

> `cargo install kugou-tui` 这条路**暂时不走**——crate 尚未发布到 crates.io。
> 而且它只能装上主程序，没有那套脚本与服务；想省掉编译的话请用下面的「路径三」。

### 路径三：预编译二进制（GitHub Release）

不想装 Rust 工具链的话，直接下 Release 里的包——**三个平台都有**，内容一致
（二进制 + 三个脚本 + 全部文档）：

| 平台 | 文件 |
|---|---|
| Linux x86_64 | `kugou-tui-<版本>-x86_64-unknown-linux-gnu.tar.gz` |
| Windows x86_64 | `kugou-tui-<版本>-x86_64-pc-windows-msvc.zip` |
| macOS arm64 | `kugou-tui-<版本>-aarch64-apple-darwin.tar.gz` |

> Intel Mac 没有预编译包（CI 的 runner 是 arm64），从源码构建即可。

**Linux（x86_64）**：

```bash
tar xzf kugou-tui-0.4.3-x86_64-unknown-linux-gnu.tar.gz
cd kugou-tui-0.4.3-x86_64-unknown-linux-gnu

# 二进制与三个脚本都链进 PATH。
# `scripts/kugou-tui` 与二进制同名，所以链过去要改名（同 AUR 包的做法）。
mkdir -p ~/.local/bin
ln -s "$PWD/kugou-tui"                    ~/.local/bin/kugou-tui
ln -s "$PWD/scripts/kugou-api"            ~/.local/bin/kugou-api
ln -s "$PWD/scripts/kugou-api-install"    ~/.local/bin/kugou-tui-install-api
ln -s "$PWD/scripts/kugou-tui"            ~/.local/bin/kugou-tui-launch

kugou-tui-install-api kugou   # 一次性：拉取并配置接口服务
kugou-tui                     # 开播（也可用 kugou-tui-launch，它会按需拉起服务）
```

**Windows（x86_64）**：

```powershell
Expand-Archive kugou-tui-0.4.3-x86_64-pc-windows-msvc.zip -DestinationPath .
cd kugou-tui-0.4.3-x86_64-pc-windows-msvc

.\scripts\kugou-api-install.ps1   # 一次性
.\scripts\kugou-tui.ps1           # 开播
```

**macOS（arm64）** —— 脚本是 bash，用法与 Linux 相同：

```bash
tar xzf kugou-tui-0.4.3-aarch64-apple-darwin.tar.gz
cd kugou-tui-0.4.3-aarch64-apple-darwin
./scripts/kugou-api-install kugou
./scripts/kugou-tui
```

> 包除了二进制还带着脚本与全部文档，所以这套流程是自足的。
> `kugou-api-install` 需要 `node` / `npm` / `git` 与网络（它要 clone 服务并装依赖）。
>
> 它**不含**接口服务本身——那份服务要么这样拉一次，要么用 AUR 包（包里直接带）。
>
> Windows 的 zip 里同时带着 bash 版脚本，方便在 Git Bash / WSL 下用；反过来
> Linux / macOS 的 tarball 里没有 PowerShell 脚本——那三个只在 Windows 上有意义。

---

## 在 Windows 上构建与运行

Windows 走的是「自己编一份」，但**日常使用已经不用手动起服务了**：
`scripts/` 下有一套 PowerShell 脚本，对应 Unix 侧的 bash 脚本。

| Windows | Unix 侧对应 | 干什么 |
|---|---|---|
| `scripts/kugou-api-install.ps1` | `kugou-api-install` | 拉取接口服务 + 装依赖（只需一次） |
| `scripts/kugou-tui.ps1` | `kugou-tui` | 启动器：确保服务在跑，然后进播放器 |
| `scripts/kugou-api.ps1` | `kugou-api` | 服务的启停管理：`start` / `stop` / `restart` / `status` / `logs` |
| `scripts/build-windows.ps1` | `make-release-tarball` | 构建 + 打包 zip |

> 安装脚本**只做安装**（bash 侧是 `kugou-api-install` 顺手把服务拉起来）：起服务交给
> 启动器，它每次开播前都会探活、没起就自己拉。这样「日常怎么起服务」只有一处实现。
>
> 想手动管服务——停掉后台的 node、看日志、重启——用 `kugou-api.ps1`：
>
> ```powershell
> .\scripts\kugou-api.ps1 status
> .\scripts\kugou-api.ps1 logs lite
> .\scripts\kugou-api.ps1 restart
> ```
>
> 它和启动器共用同一套 PID 文件（缓存目录里的 `api-<实例>.pid`，内容是 `<PID> <端口>`），
> 所以启动器拉起来的实例，它认得出、停得掉。此前 Windows 上想停掉后台的 node 只能去
> 任务管理器按名字猜着杀，猜错会把别的 Node 项目一起带走。
>
> 这几个脚本都**只依赖 PowerShell 5.1**（Windows 自带的那版），不需要额外装 pwsh 7。

### 1. 装工具链

| 要装的东西 | 怎么装 |
|---|---|
| Rust | [rustup.rs](https://rustup.rs) → 默认选 `x86_64-pc-windows-msvc` |
| MSVC 工具链 | Visual Studio 生成工具，勾「使用 C++ 的桌面开发」；或装了 VS 就有 |
| Node.js | [nodejs.org](https://nodejs.org)，用于跑接口服务 |
| Git | [git-scm.com](https://git-scm.com)，安装脚本要用它 clone |

> **不需要 CMake、NASM 或 OpenSSL。** 这一点是刻意保住的：Windows 上 TLS 走
> 系统自带的 SChannel，而不是 rustls（后者的 `aws-lc-sys` 是 C 代码，要额外装
> CMake + NASM 才编得过）。见 `Cargo.toml` 里 `cfg(windows)` 那两段注释。

### 2. 构建

```powershell
git clone https://github.com/sijin-xb/kugou-tui.git
cd kugou-tui
cargo build --release
.\target\release\kugou-tui.exe --help
```

打包成 zip（对应 Unix 侧的 `make-release-tarball`）：

```powershell
.\scripts\build-windows.ps1
# 产物：dist\kugou-tui-<版本>-x86_64-pc-windows-msvc.zip
```

### 3. 拉取接口服务（只需一次）

和 Linux 一样，数据全部来自第三方的 [KuGouMusicApi](https://github.com/MakcRe/KuGouMusicApi)，
本项目**不含任何接口实现**：

```powershell
.\scripts\kugou-api-install.ps1
# 等价于：clone 到 %USERPROFILE%\KuGouMusicApi → 钉到验证过的提交 → npm install --omit=dev
```

想装到别处就用 `-Dir`，或设 `KUGOU_API_DIR`（启动器也认这个变量）：

```powershell
.\scripts\kugou-api-install.ps1 -Dir D:\apps\KuGouMusicApi
.\scripts\kugou-api-install.ps1 netease        # 网易云音源（用不到可以不装）
```

### 4. 开播

```powershell
.\scripts\kugou-tui.ps1
.\scripts\kugou-tui.ps1 -s 海阔天空      # 参数原样透传给播放器
```

服务没起时启动器会自己拉起来并等它就绪（约 20 秒超时），已经起着就只做一次本地
探测。想确认它到底在做什么，用 `--dry-run`：

```powershell
.\scripts\kugou-tui.ps1 --dry-run
# 配置        : C:\Users\you\AppData\Roaming\kugou-tui\config.toml
# 当前音源    : kugou_concept
# 探测地址    : http://127.0.0.1:3001（端口 3001，平台 lite）
# 服务目录    : C:\Users\you\KuGouMusicApi
# 服务在跑吗  : 没起
```

也可以完全手动，两个终端各跑一条：

```powershell
cd $env:USERPROFILE\KuGouMusicApi; $env:PORT=3000; node app.js
.\target\release\kugou-tui.exe
```

### 5. 终端要求

**用 Windows Terminal，不要用旧的「命令提示符」窗口。** 程序需要
`ENABLE_VIRTUAL_TERMINAL_PROCESSING`（交替屏幕、真彩、鼠标上报）与 UTF-8 输出，
Windows Terminal 默认满足；老的 conhost 窗口在中文与颜色上会有明显残缺。

封面在 Windows Terminal 里会退回**半块字符画**：它不支持 Kitty / iTerm2 的图形
协议，程序探测不到就自动降级，属预期行为（想省掉这份开销可以开 `lite_mode`）。

装了 Nerd Font 的话加一个环境变量，图标才不会退成 ASCII：

```powershell
$env:KUGOU_TUI_NERD_FONT = "1"
```

### 6. 与 Linux 的功能差异

| 能力 | Linux | Windows |
|---|---|---|
| 播放、搜索、歌词、封面、缓存 | ✅ | ✅ 相同 |
| MPRIS（桌面媒体控件、`playerctl`） | ✅ | ❌ D-Bus 接口，Windows 没有 |
| 系统托盘（StatusNotifierItem） | ✅ | ❌ 同上 |
| 最小化窗口 | ✅ 仅 niri 下（触发入口在**托盘菜单**里，不是键盘） | ❌ 走 compositor IPC，Windows 无对应实现 |
| 配置文件位置 | `~/.config/kugou-tui/config.toml` | `%APPDATA%\kugou-tui\config.toml` |
| 缓存与日志位置 | `~/.cache/kugou-tui/` | `%LOCALAPPDATA%\kugou-tui\` |
| 启动器 / 服务安装器 | `scripts/` 下的 bash 版 | `scripts/` 下的 `.ps1` 版（功能对应） |

三条 ❌ 都是**降级而不是故障**：没有 D-Bus 时相关代码不参与编译，界面上也不会
出现点了没反应的入口。用 `--print-config` 可以直接确认：

```powershell
.\target\release\kugou-tui.exe --print-config
# 系统托盘 : 不可用（windows 无 D-Bus）
```

### 7. 改代码时怎么确认没弄坏 Windows

`.github/workflows/ci.yml` 里有一栏 `windows-latest`，每次推送都会真的在 Windows 上
跑 `clippy + test + build + 打包`。本地想先查一遍（不需要 Windows 机器）：

```bash
rustup target add x86_64-pc-windows-msvc
cargo check  --target x86_64-pc-windows-msvc --all-targets
cargo clippy --target x86_64-pc-windows-msvc --all-targets -- -D warnings
```

最后一步链接需要 Windows 或 MSVC 工具链，Linux 上跑到
`error: linker link.exe not found` 就说明 Rust 侧全部通过了。每一处平台分支
为什么存在，见 [docs/MAINTENANCE.md](MAINTENANCE.md) 的「1.6 平台分支在哪」。

---

## 在 macOS 上构建与运行

**支持等级：代码走的是与 Linux 同一套 `cfg(unix)` 分支，CI 在 `macos-latest` 上
跑 `clippy + test + build`，但没有在真机上长期使用过。** 下面是已知的差异与做法。

### 构建

```bash
# 前置只有 Rust 1.90+ 与 Xcode 命令行工具（xcode-select --install）
git clone https://github.com/sijin-xb/kugou-tui.git
cd kugou-tui && cargo build --release
```

`aws-lc-sys`（随 rustls 引入）是 C 代码，靠 Xcode 的 clang 编译；macOS 不需要
ALSA 那套依赖，也不需要额外装 CMake（构建脚本在没有 cmake 时走 `cc` 直编）。

### 运行

脚本是 bash，和 Linux 一样用：

```bash
./scripts/kugou-api-install kugou
./scripts/kugou-tui
```

> **这些脚本刻意避开了 GNU 专有的东西**，因为 macOS 自带的是 BSD 工具链加 bash 3.2：
> 不用 `readlink -f`、`dirname --`、`seq`、`setsid`、`ss`、`declare -A`。每一处的
> 替代写法和原因都写在脚本文件头的「可移植性」一节里，改脚本前先读那段。
>
> 目录也对齐了 `dirs` 在 macOS 上的取值（`~/Library/Application Support` 与
> `~/Library/Caches`，**不是** `~/.config` / `~/.cache`）——不这样对齐的话，启动器会去
> 找一个永远不存在的 `config.toml`，于是永远按默认端口和默认音源探活。
>
> 如果二进制是从浏览器下载的 tar 包里解出来的，它会带 `com.apple.quarantine` 属性，
> 而包里没有签名——直接跑会被 Gatekeeper 杀掉，终端上只显示 `Killed: 9`，看起来像
> 程序自己的 bug。启动器会检测到并打印该怎么做；手动来一遍是：
>
> ```bash
> xattr -d com.apple.quarantine ./kugou-tui
> ```

### 与 Linux 的差异

| 项目 | macOS 上的表现 |
|---|---|
| 音频后端 | CoreAudio（cpal 自动选），不需要配设备；设置页里的设备列表来自 CoreAudio |
| MPRIS / 系统托盘 | **不可用**。两者都是 D-Bus 接口，macOS 默认没有 session bus；启动时会往日志写一条 WARN 然后跳过，不影响播放 |
| 最小化窗口 | 不可用。它走的是 niri 的 compositor IPC，`NIRI_SOCKET` 不存在时入口直接不出现 |
| 配置文件 | `~/Library/Application Support/kugou-tui/config.toml`（**不是** `~/.config`） |
| 缓存与日志 | `~/Library/Caches/kugou-tui/` |
| 默认下载目录 | `~/Music`（`dirs` 的 `audio_dir` 就是它） |
| 封面 | iTerm2 能识别并走图形协议；Terminal.app 退回半块字符画（预期行为） |
| Nerd Font 探测 | 没有 fontconfig，`fc-list` 探测一律落空 → 退 ASCII。装了 Nerd Font 就设 `KUGOU_TUI_NERD_FONT=1` |

想确认实际读的是哪个配置文件，用 `--print-config`。

---

## 部署第三方 API 服务

**装过 AUR 包的话这一整节都不用做**：包已经把酷狗那份服务连同生产依赖放在
`/usr/share/kugou-tui/api/kugou/`，启动器会优先用它。下面这套流程是给
「从源码跑」和「要装网易云那份服务」的人准备的。

**本项目不含任何接口实现**，数据全部来自第三方的
[KuGouMusicApi](https://github.com/MakcRe/KuGouMusicApi)——它是**独立仓库**，
不在本仓库里（没有 submodule，也没有 vendor 目录），所以得先把它拉下来跑起来。

### 一键（推荐）

仓库里有个脚本把「clone → 装依赖 → 启动」合成一条命令：

```bash
./scripts/kugou-api-install kugou
```

它会 clone 到 `~/KuGouMusicApi`、`npm install`，然后调 `scripts/kugou-api start`
把标准版（:3000）和概念版（:3001）两个实例都拉起来。`./scripts/kugou-api-install`
不带参数会列出各音源的仓库、**钉住的提交**与当前运行状态。

### 手动

```bash
git clone https://github.com/MakcRe/KuGouMusicApi.git
cd KuGouMusicApi
# 锁定到经过验证的提交：上游是活跃仓库，接口字段会变，
# 直接跟 master 可能某天就解析不出歌名或歌词
git checkout a5a98013cce79fe0ae2ad65fc84b68176ebcfc1e
npm install
npm start          # 注意是 npm start，不是 npm run dev
```

> 这个提交号在 `scripts/kugou-api-install` 的 `PINNED[kugou]` 里也有一份，
> 脚本会按它做**浅取**（`git fetch --depth 1 origin <sha>`，只拉那一个提交）。
> 换验证过的提交时两处一起改。

> 概念版（lite）实例要带 `platform=lite` 启动：
> `platform=lite PORT=3001 npm start`
> 不加这个环境变量，概念版搜索会拿不到正确结果。
> 用 `scripts/kugou-api start` 就不用管这些，它两个实例都会带对参数起。

服务默认监听 `http://127.0.0.1:3000`。验证一下：

```bash
curl -s "http://127.0.0.1:3000/register/dev"
```

> `npm run dev` 走的是 nodemon（一个 devDependency），只装过生产依赖的环境里会直接失败。

> **只用「酷狗」音源的话，起这一个实例就够了。**
> 想用「酷狗概念版」音源，见[常见问题](FAQ.md)里的「概念版音源怎么配」。
>
> 服务目录默认是 `~/KuGouMusicApi`，用 `KUGOU_API_DIR` 可以改（启动脚本和
> `kugou-api` 都认这个变量）。

**网易云音源**走的是另一套服务（NeteaseCloudMusicApi），需要单独部署，
部署方式与配置见[使用指南](USER_GUIDE.md#网易云音源)。

---

## 安装启动器脚本（可选）

仓库提供两个脚本：

| 脚本 | 作用 |
|---|---|
| `scripts/kugou-tui` | 启动播放器；API 服务没起就自动拉起并等待就绪 |
| `scripts/kugou-api` | 一次拉起**两个** API 实例（标准版 + 概念版） |

```bash
ln -s "$PWD/scripts/kugou-tui" "$PWD/scripts/kugou-api" ~/.local/bin/
```

只用一个音源时，装 `kugou-tui` 就够；两个音源都要用，再装 `kugou-api`：

```bash
kugou-api start     # 启动两个实例（已在跑的会跳过），打印 PID / 日志路径 / 访问地址
kugou-api status    # 查看状态（运行中会显示 PID；端口被别人占着也会如实说明）
kugou-api logs      # 跟随日志（默认标准版，`kugou-api logs lite` 跟概念版）
kugou-api restart   # 先停再起（改了端口/配置后用它）
kugou-api stop      # 停止（按 PID 精确停止）
kugou-api help      # 完整说明
```

启动器（`kugou-tui`）在服务没起时会自己拉起，并把 PID 写进
`<缓存目录>/kugou-tui/api-<实例>.pid`（内容 `<PID> <端口>`）——所以它拉起来的实例，
上面这几条命令也管得到。想先看清楚「它到底在探哪个地址、用哪个配置、找哪个目录」，
用 `kugou-tui --dry-run`：只打印决策，不启动任何东西。

`start` 之前会做一轮预检，缺什么补什么：`node`、KuGouMusicApi 目录、
`node_modules`（缺了自动 `npm install --omit=dev`）、客户端二进制（缺了自动
`cargo build --release`）。任一步失败都会带着明确原因终止，不会留一个半死不活的进程。
不想让它碰编译就设 `KUGOU_API_SKIP_BUILD=1`（`stop` / `status` 本来就不触发编译）。

参数按**环境变量 > 配置文件 > 默认值**取值。配置文件是可选的
`<配置根>/kugou-tui/api.env`——Linux 上是 `~/.config/kugou-tui/api.env`，
macOS 上是 `~/Library/Application Support/kugou-tui/api.env`，两者都可以用
`KUGOU_TUI_CONFIG_DIR` 整个覆盖。每行一个 `KEY=VALUE`：

```bash
# 临时换端口起一次，不动任何文件
KUGOU_STANDARD_PORT=3100 KUGOU_LITE_PORT=3101 kugou-api restart
```

`kugou-api` 认这些（配置文件里写同名键）：

| 变量 | 默认值 | 说明 |
|---|---|---|
| `KUGOU_API_DIR` | `/usr/share/kugou-tui/api/kugou`（软件包提供时）或 `$HOME/KuGouMusicApi` | 服务所在目录 |
| `KUGOU_API_SYSTEM_ROOT` | `/usr/share/kugou-tui/api` | 软件包放服务的位置。改它是给非 `/usr` 前缀的打包用的，普通用户不用管 |
| `KUGOU_API_LOG_DIR` | `$XDG_CACHE_HOME/kugou-tui` | 日志与 PID 文件目录 |
| `KUGOU_API_HOST` | `127.0.0.1` | 监听地址 |
| `KUGOU_STANDARD_PORT` | `3000` | 标准版端口 |
| `KUGOU_LITE_PORT` | `3001` | 概念版端口 |
| `KUGOU_API_BIN` | `<仓库>/target/release/kugou-tui`，不存在则用 `PATH` 上的 `kugou-tui` | 客户端二进制路径 |
| `KUGOU_API_SKIP_BUILD` | 空 | 设为 `1` 跳过编译预检 |
| `KUGOU_API_CONFIG` | `$XDG_CONFIG_HOME/kugou-tui/api.env` | 配置文件路径 |

`scripts/kugou-tui`（播放器启动器）认的是另一组：

| 变量 | 默认值 | 说明 |
|---|---|---|
| `KUGOU_API_BASE` | **不设置** | 设了才给程序传 `--api-base`，并拿它探活。不设时让程序**读配置里选中的音源**——否则每次都被强行拉回标准版 `:3000`，用概念版的人得按 `v` 切两次才回得去 |
| `KUGOU_API_DIR` | `$HOME/KuGouMusicApi` | 服务所在目录，用于自动拉起 |
| `KUGOU_API_LOG` | `$XDG_CACHE_HOME/kugou-tui/api.log` | 服务日志路径 |
| `KUGOU_TUI_BIN` | 自动探测 | 手动指定二进制路径。默认依次找：`脚本所在目录/../target/release/kugou-tui`（软链到 `~/.local/bin` 时走这条）→ `PATH` 上的 `kugou-tui`（包管理器装到 `/usr/bin` 时走这条） |
| `API_PORT` | `3000` | 自动拉起服务时用的端口 |

### 不用常驻服务的启动方式（fish）

`scripts/kugou-tui` 用 `setsid` 把服务留在后台，**不会在退出时回收**。不想让 node
常驻的话，可以把 API 服务当成「听歌时的临时依赖」：启动前 `git pull` 一次拿最新代码，
缺依赖就装，没在跑就起，播放器退出时收掉自己起的那个。好处是不占常驻内存、不会因为
服务端更新而失效、卸载就是删一个文件。

`~/.config/fish/functions/kg.fish`：

```fish
function kg --description '启动 kugou-tui：按需拉取并拉起接口服务，退出时收摊'
    set -l cfg $HOME/.config/kugou-tui/config.toml
    test -n "$XDG_CONFIG_HOME"; and set cfg $XDG_CONFIG_HOME/kugou-tui/config.toml

    # 当前音源决定用哪个服务、哪个目录、哪个端口
    set -l kind (sed -n 's/^[[:space:]]*active[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' $cfg 2>/dev/null | head -n1)
    test -n "$kind"; or set kind kugou

    set -l dir $HOME/KuGouMusicApi
    set -l port 3000
    set -l extra
    switch $kind
        case kugou_concept
            set port 3001
            set extra --platform=lite
        case netease
            set dir $HOME/NeteaseCloudMusicApi
            set port 3002
    end

    set -l cache $HOME/.cache
    test -n "$XDG_CACHE_HOME"; and set cache $XDG_CACHE_HOME
    set -l log $cache/kugou-tui/api-$port.log
    mkdir -p (dirname $log)

    # 1. 拉取：只更新代码，不动 node_modules
    if test -d $dir/.git
        printf '\e[36m拉取 %s …\e[0m\n' $dir >&2
        git -C $dir pull --ff-only --quiet
    else
        printf '\e[31m%s 不存在。先跑一次：kugou-api-install %s\e[0m\n' $dir $kind >&2
        return 1
    end

    # 2. 依赖：只在缺的时候装
    test -d $dir/node_modules; or npm --prefix $dir install --omit=dev

    # 3. 拉起。已经在跑就跳过；只有本次启动的才会在退出时被收掉
    set -l api_pid 0
    if not curl -sf --max-time 2 -o /dev/null http://127.0.0.1:$port/
        # 端口两种写法都给：酷狗认 --port=，网易云只认 PORT 环境变量。认哪个用哪个。
        env PORT=$port nohup node $dir/app.js --port=$port $extra >$log 2>&1 </dev/null &
        set api_pid $last_pid
        for _ in (seq 40)
            sleep 0.5
            curl -sf --max-time 2 -o /dev/null http://127.0.0.1:$port/; and break
        end
    end

    # 4. 进播放器（前台阻塞，退出后继续往下走）
    command kugou-tui $argv

    # 5. 收摊：只收自己起的那个，别人的进程不动
    if test $api_pid -ne 0
        kill $api_pid 2>/dev/null
    end
end
```

三个刻意的选择：

1. **不用 `setsid`**。它会 fork，`$last_pid` 拿到的是 `setsid` 而不是 `node`，
   `kill` 就杀不到真正的服务。这里要的正是「进程归我管」，所以直接 `nohup node &`。
2. **不写 `VAR=value cmd`**。fish 不支持这种写法，用 `env PORT=...`。
3. **端口两种参数都给**。KuGouMusicApi 认 `--port=`，NeteaseCloudMusicApi
   **只认 `PORT` 环境变量**——传 `--port=3002` 会被忽略，起在默认 3000，而客户端探
   3002 永远失败；它的日志里却还写着 `Server started successfully`，极具误导性。

---

## 装完先做什么

**不登录也能听歌**。搜索和云端歌单才需要账号。

1. 确认 KuGouMusicApi 已在跑（见上一步）。
2. 启动：`./target/release/kugou-tui`（或 `kugou-tui`，若已装到 `~/.local/bin`）。
3. 按 **`3`** 进入歌单广场 → `Enter` 打开一个歌单 → 移动光标 → `Enter` 播放。

数字键落点是 `1` 首页、`2` 搜索、`3` 歌单、`4` 歌手、`5` 排行榜、`6` 云端、
`7` 队列、`8` 音源、`9` 设置、`0` 可视化（完整表见 [KEYBINDINGS.md](../KEYBINDINGS.md)）。

想搜歌（`2`）得先登录，按 **`L`** 扫码即可。

> 首次启动时程序会自动获取设备指纹 `dfid` 并写入配置。取播放直链需要它，
> 这一步是自动的，不需要你做任何事。

---

## 相关

- 快捷键全表：[KEYBINDINGS.md](../KEYBINDINGS.md)
- 界面布局与音源配置：[使用指南](USER_GUIDE.md)
- 配置文件每一项：[配置说明](CONFIGURATION.md)
- 装完出问题：[常见问题](FAQ.md)
