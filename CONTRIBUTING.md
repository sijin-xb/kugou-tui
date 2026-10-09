# 贡献指南

怎么把项目跑起来、代码有哪些约定、提交前该跑哪些检查。

## 先把环境跑起来

1. **Rust 1.90+**（edition 2024；下限由**依赖**顶上去——`quantette` 要 1.90）。
2. **Node.js 12+**：接口服务 [KuGouMusicApi](https://github.com/MakcRe/KuGouMusicApi) 是独立
   仓库（无 submodule、无 vendor），得单独拉一份。最省事：

   ```bash
   ./scripts/kugou-api-install kugou   # clone + npm install + 起两个实例
   ```

   手动来一遍也可以，但**要固定到验证过的提交**（上游是活跃仓库，接口字段会漂移）：

   ```bash
   git clone https://github.com/MakcRe/KuGouMusicApi.git && cd KuGouMusicApi
   git checkout a5a98013cce79fe0ae2ad65fc84b68176ebcfc1e
   npm install && npm start            # 注意是 npm start，不是 npm run dev
   ```

3. 编译运行：

   ```bash
   cargo build --release && ./target/release/kugou-tui
   ```

不登录也能浏览和播放，一般开发调试不需要扫码。

## 提交前请跑这三条

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test
```

**clippy 是 `-D warnings` 的**，任何告警都会让构建失败。CI（`.github/workflows/ci.yml`）在
Linux / Windows / macOS 三个平台各跑一遍 clippy + test（`cargo fmt --check` 与关掉默认
feature 的那套组合只在 Linux 那一栏，两者都与平台无关）。Windows 与 macOS 两栏不是摆设：
路径展开、配置目录、缓存文件命名、音频后端这些差异只有真跑起来才露出来，而开发机通常是
Linux。

**CI 只做验证，不产出任何二进制**——三栏都不跑 `cargo build --release`、不打发行包、
不传 artifact。想拿一份能跑的二进制有两条路：Releases 页（由 `release.yml` 在推 tag 时
产出），或本机 `cargo build --release`。理由写在 `ci.yml` 的文件头。

改到平台相关代码时，另外确认 Windows 目标能编：

```bash
rustup target add x86_64-pc-windows-msvc
cargo check --target x86_64-pc-windows-msvc --all-targets
```

> MSRV 是 **1.90**，写在 `Cargo.toml` 的 `rust-version` 里，由**依赖**顶上去，会随
> `cargo update` 漂。改完依赖想确认下限：`cargo +1.90.0 check --locked --all-targets`。

## 代码结构

```
src/
├── main.rs          入口：分配器策略、参数解析、日志、接口服务引导、启动 App
├── bootstrap.rs     本机接口服务（KuGouMusicApi）的自动引导
├── cli.rs           clap 命令行参数
├── config.rs        TOML 配置持久化（含默认值与合法取值常量）
├── error.rs         统一错误类型 AppError
├── logger.rs        极简文件日志（不引入 tracing）+ 内存追踪
├── event.rs         事件总线与 Loaded 枚举
├── keymap.rs        按键 → Action 的映射，以及帮助面板用的 CHEATSHEET
├── mpris.rs         MPRIS（桌面媒体控件），仅 Unix
├── tray.rs          系统托盘（StatusNotifierItem），仅 Unix
├── window.rs        niri 窗口控制（最小化 / 显示）
├── util.rs          时间格式化、随机数等小工具
├── api/
│   ├── client.rs    HTTP 客户端：鉴权参数、错误码解析、响应缓存绕开
│   ├── model.rs     JSON → 领域模型的解析（多候选键名都在这里）
│   ├── catalog.rs   浏览类接口：搜索 / 歌单 / 歌手 / 排行榜
│   ├── cloud.rs     登录与云端歌单写操作
│   └── lyric.rs     歌词获取与 KRC/LRC 解码
├── app/
│   ├── mod.rs       App 装配、主循环、输入线程
│   ├── state.rs     AppState 与各列表结构
│   ├── update.rs    事件 → 状态变更（唯一改状态的地方）
│   ├── playback.rs  播放控制（起播 / 切歌 / 进度 / 音量）
│   ├── cloud.rs     登录、云端歌单与音源切换
│   ├── desktop.rs   把状态推给 MPRIS 与托盘
│   ├── navigation.rs 标签页与焦点
│   ├── queue.rs     播放队列与播放模式
│   ├── search.rs    搜索
│   ├── session.rs   会话持久化
│   └── settings.rs  设置页
├── audio/
│   ├── engine.rs    rodio 播放引擎（独立线程 + 原子量）
│   ├── streaming.rs 边下边播的内存窗口与落盘
│   ├── download.rs  音频下载（写 .part 再原子重命名）
│   ├── cache.rs     缓存目录管理与 LRU 回收
│   ├── resample.rs  hi-res 母版的抗混叠重采样
│   ├── downmix.rs   多声道下混
│   ├── levels.rs    电平采集与频谱计算
│   └── spectrum.rs  FFT
├── source/          音源抽象（酷狗 / 概念版 / 网易云）
└── ui/
    ├── mod.rs       布局
    ├── theme.rs     配色（真彩 / 16 色两套）
    ├── icons.rs     图标（Nerd Font / ASCII 两套）
    ├── widgets.rs   列表行、面板、二维码等渲染原语
    └── views/       侧边栏、列表、播放条、歌词、帮助、设置、音源
```

## 约定

### 注释写「为什么」，不写「是什么」

代码本身已经说明了它在做什么，注释补充**读代码看不出来的背景**：某个字段为什么有两个名字、
某处为什么要加时间戳、某个魔法数字的来历。每个模块顶部的 `//!` 文档用来讲这个模块的设计取舍。

### UI 只用 ASCII 标记

界面里的符号（选中标记 `>`、播放中 `*`）一律用 ASCII。花式 Unicode 符号在缺字体的终端里会变成
豆腐块或让列宽算错。

### 文本按「显示宽度」截断

中文、全角标点占 2 列。所有截断都要走 `display_width` / `truncate_to_width`，直接按
`chars().count()` 切会让中英混排列错位。

### 新增依赖要说明理由

本项目的目标之一是「轻量」（release 二进制约 **7.0 MiB**、常驻约 **14–17 MiB**，实测见
[docs/DESIGN.md](docs/DESIGN.md#低资源占用)）。加依赖前请说明：它解决什么问题、有没有几十行
代码能替代、会引入多大的依赖树。`tracing` / `chrono` / `rand` 目前都被刻意排除在外。

### 解析上游响应要宽容

KuGouMusicApi 是逆向封装，**响应结构会漂移，文档也和实测不一致**。约定：

- 同一语义按**候选键名列表**依次尝试（`pick_string` / `pick_i64` 等）。
- 类型不假设：`AlbumID` 可能是数字、字符串或 `null`。
- 单条解析失败只丢这一条，不影响整页。
- 新增解析逻辑时，顺手把与文档不一致的地方记进
  [docs/DESIGN.md](docs/DESIGN.md#接口适配) 的「上游文档说 / 实际是」表格。
- **同一个上游可能有多套字段布局**。网易云就是例子：`/search` 给 `artists` / `album` /
  `duration`，而 `/playlist/track/all` 与 `/artists` 给 `ar` / `al` / `dt`——只认前者的话搜索页
  正常，歌单页与歌手页的歌会全部没有歌手、没有专辑、时长显示 `00:00`。**每种布局都要有单元
  测试钉住。**

### 状态只在主线程改

`app/update.rs` 是唯一修改 `AppState` 的地方；音频线程只通过原子量共享播放位置、时长、音量。
这样不需要加锁，也让状态变更可追溯。

## 怎么加一个新接口

1. `api/catalog.rs`（或 `cloud.rs`，如果是写操作）加一个 `async fn`，用
   `self.get_json_uncached(...)` 或 `get_json(...)`。
2. `api/model.rs` 加对应的解析函数，按上面的宽容约定写。
3. `event.rs` 的 `Loaded` 枚举加一个变体。
4. `app/update.rs` 加 fetch 方法 + `Loaded` 分支的处理。
5. 需要新按键的话：`keymap.rs` 加 `Action` 变体、按键映射、`CHEATSHEET` 条目，以及
   `REBINDABLE` 里的动作名（用户靠这个名字在 `[keymap]` 里重绑）；最后同步
   [docs/CONFIGURATION.md](docs/CONFIGURATION.md#自定义键位) 的动作名清单。
   **漏了动作名的话测试会当场告诉你**——`every_cheatsheet_shortcut_can_be_rebound_by_name`
   拿两份名单对着数。（CONFIGURATION.md 那份清单只能靠人同步，测试管不到。）
6. 需要展示的话：`app/state.rs` 加列表字段，`ui/` 加渲染。
7. 补单元测试——尤其是解析部分。

## 验证方式

**不要只靠文档验证。** 本项目踩过的坑几乎都来自「文档这么说，实际不这样」。改动涉及接口时：

- 用 `curl` 打真实服务，把原始响应存下来对照；
- 起应用实机点一遍，确认到「能看到具体内容」为止（不只是「列表加载成功」）。

调试按键问题时打开详细日志（会记录每个按键映射成了什么动作）：

```bash
KUGOU_TUI_DEBUG=1 ./target/release/kugou-tui
```

日志路径见 `kugou-tui --print-config`。

## 提交与 PR

- commit message 一句话说明改了什么；涉及多个改动请拆成多个 commit。
- PR 里说明：**改了什么、为什么改、怎么验证的**（接口相关的改动附一小段真实响应的关键字段
  会很有帮助）。
- 改动影响用户可见行为时，同步更新 `README.md`。

## 发版

### 1. 先改版本号与变更日志

```bash
# Cargo.toml 的 version（纯数字 X.Y.Z；Cargo.lock 会跟着变，一起提交）
# CHANGELOG.md 顶部加一节，格式照 [Keep a Changelog]
git add -A && git commit -m "chore: 版本 X.Y.Z" && git push
```

### 2. 发 crates.io

```bash
cargo publish          # 需要 crates.io 账号，且邮箱已验证
```

`scripts/release` **不管这一步**。注意 crates.io 页面上渲染的是**该版本包里那份 README**——
README 改了但没发新版，页面就不会变。

### 3. 打 tag 与建 Release

```bash
./scripts/release              # 阶段一本地构建，确认后阶段二推送
./scripts/release --dry-run    # 只检查，什么都不构建不推送
./scripts/release --no-aur     # 跳过 AUR（AUR 推送走 SSH，key 没配好时必失败）
```

它会自动做完：前置检查 → 回收上一轮中间产物 → clippy + 测试 → 构建 → 打发行包并核对内容
清单 → **停下等你确认** → push main → 打 tag → 推 tag → 更新并验证 AUR → 建 Release →
下载回来核对 → 推 AUR。

**完整规格（版本与标签规则、产物清单、各步骤依赖、失败后怎么恢复）见
[docs/RELEASE.md](docs/RELEASE.md)。**

### 两条最容易忘的

1. **工具脚本要在打 tag 之前提交。** `scripts/make-release-tarball` 曾提交在 tag 之后，结果
   checkout 那个 tag 拿不到它。`scripts/release` 的前置检查要求工作区干净，从机制上挡住了这个。
2. **上游提交号钉在两个地方**：`scripts/kugou-api-install` 的 `PINNED[kugou]`，以及 AUR 的
   `PKGBUILD` 里的 `_api_pin`。换 `_api_pin` 时还要重新生成 AUR 仓库里的
   `kugou-api-package-lock.json`（上游那个提交只有 `pnpm-lock.yaml`，而构建用的是 `npm ci`）。
   生成命令写在 PKGBUILD 的注释里。

### 磁盘

一轮发版会在 `target/package/`、`dist/`、AUR 仓库的 `src/` 与 `pkg/` 留下约 10 MiB 中间产物。
`scripts/release` 开跑前会自动回收；平时也可以手动跑：

```bash
./scripts/clean --dry-run   # 先看它想删什么
./scripts/clean             # 回收
./scripts/clean --all       # 连 target/ 一起清（下次全量重编约 3 分钟）
```

## 法律与合规

本项目只做客户端，不实现任何接口。提交代码时请确保：

- 不包含任何破解、绕过鉴权或规避版权保护措施的实现；
- 不引入会上传用户数据的代码（本项目无任何遥测，登录凭据只存在本机）；
- 涉及第三方服务的改动，在 PR 里说明用途与合规影响。
