# 安装

本文是 [README](../README.md) 的展开版：环境要求、安装路径、第三方 API 服务怎么部署、
启动器脚本与环境变量、装完先做什么。

---

## 环境要求

| 项目 | 要求 |
|---|---|
| Rust 工具链 | **1.86+**（edition 2024）。下限由 `ratatui-image` 11.x 决定，见 `Cargo.toml` |
| Node.js | 用于运行 KuGouMusicApi（上游 `engines` 要求 **12+**） |
| 音频输出 | 任意 rodio 支持的后端（Linux 上为 ALSA/PulseAudio） |
| 终端 | 支持 UTF-8；**真彩（24 位）** 才能看到完整的主题配色与逐字渐变，老终端可加 `--basic-color` 退回 16 色 |
| 操作系统 | **Linux**（在 CachyOS 上实测）。macOS 未验证但理论上可行；**Windows 不行**——`zbus` 在 Windows 上要求 `async-io` 特性，而这里按 `default-features = false` 只开了 `tokio` |
| D-Bus（可选） | 有 session bus 时自动启用 MPRIS 与系统托盘；没有（纯 tty）则跳过，**不影响播放** |
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

不想装 Rust 工具链的话，直接下 Release 里的 tarball（x86_64 Linux）：

```bash
tar xzf kugou-tui-0.4.0-x86_64-unknown-linux-gnu.tar.gz
cd kugou-tui-0.4.0-x86_64-unknown-linux-gnu

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

> tarball 里除了二进制还带着三个脚本与全部文档，所以这套流程是自足的。
> `kugou-tui-install-api` 需要 `node` / `npm` / `git` 与网络（它要 clone 服务并装依赖）。
>
> 它**不含**接口服务本身——那份服务要么这样拉一次，要么用 AUR 包（包里直接带）。

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
kugou-api restart   # 先停再起（改了端口/配置后用它）
kugou-api stop      # 停止（按 PID 精确停止）
kugou-api help      # 完整说明
```

`start` 之前会做一轮预检，缺什么补什么：`node`、KuGouMusicApi 目录、
`node_modules`（缺了自动 `npm install --omit=dev`）、客户端二进制（缺了自动
`cargo build --release`）。任一步失败都会带着明确原因终止，不会留一个半死不活的进程。
不想让它碰编译就设 `KUGOU_API_SKIP_BUILD=1`（`stop` / `status` 本来就不触发编译）。

参数按**环境变量 > 配置文件 > 默认值**取值。配置文件是可选的
`~/.config/kugou-tui/api.env`，每行一个 `KEY=VALUE`：

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
