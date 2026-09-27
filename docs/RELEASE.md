# 发版与回收

本文回答四件事：**版本号与标签规则**、**发行版产物的内容**、**发版流程的顺序与依赖**、
以及**两类回收逻辑的触发时机与清理范围**。

对应实现：`scripts/release`（发版流程）、`scripts/clean`（构建产物回收）、
`src/audio/streaming.rs` + `src/audio/cache.rs`（运行时回收）。
本文只描述**已经验证过的行为**；没验证的会明说。

---

## 一、版本号与标签规则

| 项 | 规则 | 由谁强制 |
|---|---|---|
| 版本号 | 写在 `Cargo.toml` 的 `version`，必须是纯数字的 `X.Y.Z`（无 `-alpha` 之类后缀） | `scripts/release` 前置检查 |
| 标签 | 固定为 `vX.Y.Z`，**附注 tag**（`git tag -a`），注释里带版本说明 | `scripts/release` |
| 标签唯一 | 本地与远端都查，已存在即中止 | `scripts/release` |
| 变更日志 | `CHANGELOG.md` 必须有 `## [X.Y.Z]` 一节 | `scripts/release` |
| AUR `pkgver` | 必须等于同一个版本号 | `scripts/release` 自动改写 |
| 二进制自报版本 | 构建出的二进制 `--version` 必须与 `Cargo.toml` 一致 | `scripts/release` |

**标签不可重来。** 版本号一旦用掉就换新的，不要删 tag 重打——AUR 与 crates.io 的
版本同样不可撤销。

---

## 二、发行版产物应包含哪些内容

产物是一个 tarball：`kugou-tui-<版本>-x86_64-unknown-linux-gnu.tar.gz`（约 3.16 MB），
由 `scripts/make-release-tarball` 生成。**必须自足**，因为它是**非 Arch 用户唯一
能拿到的路径**——AUR 包里直接带了接口服务，这个 tarball 没有，所以它必须带上
把服务配起来所需的全部东西：

| 内容 | 为什么必须有 |
|---|---|
| `kugou-tui`（二进制，约 7.0 MiB，已 strip） | 主程序 |
| `scripts/kugou-api` | 启停接口服务 |
| `scripts/kugou-api-install` | **拉取并配置服务**——少了它，用户配不起服务，流程直接断 |
| `scripts/kugou-tui` | 启动器（按需拉起服务） |
| `README.md`、`KEYBINDINGS.md`、`CHANGELOG.md`、`LICENSE` | 基本说明 |
| `docs/*.md` | 完整文档 |

`scripts/release` 会逐项检查这个清单，缺任何一项就中止。

> 0.3.7 及之前的发行包**只有二进制与 README**，漏了三个脚本——非 Arch 用户解压后
> 跑不了 `kugou-tui-install-api`。手工拼目录很容易漏，所以这一步已经固化成脚本。

**不含**：接口服务本身（那是独立项目，由用户按 `docs/INSTALL.md` 拉取）。
用 `cargo build --release` 构建，**不能用 makepkg** —— `makepkg` 会带上
`makepkg.conf` 的 `-march=native`，那样的二进制换台机器就跑不了。

### 三个平台的包由 CI 产出

`scripts/release` 跑在开发机上，能产出 `x86_64-unknown-linux-gnu` 的 tarball；
另外两个平台**编不出来不是漏了，是物理限制**——Windows 的 `.exe` 要 MSVC 工具链，
macOS 的二进制要 Apple SDK。

所以三个平台统一交给 `.github/workflows/release.yml`：推 `v*` tag 时在
`ubuntu-latest` / `windows-latest` / `macos-latest` 上各编一份、打好包、
挂到同一个 Release 上。这样**开发机不在手边也能发版**，产物不依赖任何一台具体的机器。

| 平台 | 产物 | 打包脚本 |
|---|---|---|
| Linux x86_64 | `kugou-tui-<版本>-x86_64-unknown-linux-gnu.tar.gz` | `make-release-tarball`（CI 与本地都跑） |
| Windows x86_64 | `kugou-tui-<版本>-x86_64-pc-windows-msvc.zip` | `build-windows.ps1`（CI 跑） |
| macOS arm64 | `kugou-tui-<版本>-aarch64-apple-darwin.tar.gz` | `make-release-tarball`（CI 跑） |

> **Linux 有两个来源。** 本地 `scripts/release` 仍会编一份并上传（它顺带核对资产内容），
> CI 也会编一份，两边同名，靠 `upload-release-asset` 的 `--clobber` 覆盖，谁后到谁生效。
> 因此**两边的 sha256 不保证相同**——rustc 小版本、目标机器上的 C 工具链都会影响
> `aws-lc-sys` 的编译结果。本地发版时记下的哈希，事后从 Release 下载回来对不上是正常的，
> 不是文件被换了。想让哈希只有一个来源，就把 `scripts/release` 里的 Linux 打包与上传
> 去掉，完全交给 CI——那属于发版流程的改动，单独做。

> **Intel Mac 不在覆盖范围**：`macos-latest` 是 arm64 机器，产出的二进制在 Intel
> 机器上跑不了。要补的话得在同一个 runner 上 `--target x86_64-apple-darwin` 交叉编一份
> ——那是另一个包名（`x86_64-apple-darwin`），加一个 job 即可，目前没做。

时序上有个坑：`scripts/release` 的顺序是「推 tag → 建 Release」，而 tag 一推 CI 就
起来了——**上传前必须等 Release 出现**。这件事连同幂等（`--clobber`）都收在
`scripts/upload-release-asset` 里，三个平台共用同一个脚本（Windows runner 上的
`shell: bash` 就是 Git Bash）。三个 job 都会先核对二进制的 `--version` 与 tag 一致，
checkout 错 ref 时能拦住。

**给已经发过的版本补资产**：`workflow_dispatch` 手动指定 tag 即可，例如 0.4.2 发布时
这个工作流还不存在：

```bash
gh workflow run release.yml -f tag=v0.4.2
```

zip 里同时带着三个 **bash** 脚本（Git Bash / WSL 下仍然用得着），macOS 的 tarball
同理——包里带的是「另一套平台下也用得上的东西」，而不是「本平台的原生脚本」，
后者由源码仓库提供。

---

## 三、推送发布流程的目标仓库

| 目标 | 地址 | 推什么 | 凭据 |
|---|---|---|---|
| GitHub（源码与 Release） | `github.com/sijin-xb/kugou-tui` | `main` 分支、`vX.Y.Z` tag、Release 资产 | `gh` 已登录 |
| AUR | `ssh://aur@aur.archlinux.org/kugou-tui.git`（本地在 `~/aur/kugou-tui`） | `master` 分支的 `PKGBUILD` 与 `.SRCINFO` | 独立密钥 `~/.ssh/aur`，公钥贴在 AUR 账户 |

AUR **只收 `master` 分支**；`.SRCINFO` 必须一起推，否则页面不更新版本号。
本地仓库的 git 身份是仓库级的 `sijin-xb <sijin-xb@users.noreply.github.com>`
——**作者邮箱会明文出现在 AUR 的提交日志里**，所以用 GitHub 的 noreply 而不是真实邮箱。

---

## 四、执行顺序与依赖关系

`./scripts/release` 一条命令走完。分两阶段，**阶段二全部是不可逆的外部动作**，
所以脚本在阶段一结束后停下来列出将要做的每一条，等确认。

### 阶段一：本地（可反复跑，无外部影响）

| # | 步骤 | 依赖 |
|---|---|---|
| 1 | 前置检查（版本号格式、分支、工作区干净、与 origin 同步、标签未被占用、CHANGELOG 有对应小节） | — |
| 2 | 回收上一轮的中间产物（调 `scripts/clean`） | 1 |
| 3 | `cargo clippy --all-targets` + `cargo test` | 2 |
| 4 | `cargo build --release`，并核对二进制自报版本 | 3 |
| 5 | 打发行包，并逐项核对内容清单 | 4 |
| 6 | 列出阶段二的动作，等待确认 | 5 |

### 阶段二：外部（不可逆）

| # | 步骤 | 依赖 | 备注 |
|---|---|---|---|
| 7 | `git push origin main` | 6 | 网络动作，带退避重试 |
| 8 | `git tag -a vX.Y.Z` + `git push origin vX.Y.Z` | 7 | **不可撤销** |
| 9 | 取 tag 源码包的 sha256，改写 AUR 的 `pkgver` 与 `sha256sums`，重新生成 `.SRCINFO`，本地提交（**不推**） | 8 | **顺序上唯一无法重排的依赖**，见下 |
| 10 | 从该提交克隆出 AUR 仓库并跑一次 `makepkg` | 9 | 验证「用户 clone 到的能构建」 |
| 11 | `gh release create` 并挂发行包 | 8 | 幂等：已存在则改为 `gh release upload --clobber` |
| 12 | 把 Release 资产**下载回来**核对内容与 sha256 | 11 | 不能只看本地文件 |
| 13 | 推 AUR（`master`） | 9 | 密钥未注册时只提示、不报错 |

### `--no-aur`：跳过 AUR 那三步

AUR 账号注册不了的时候（实测撞过），用 `./scripts/release --no-aur` 可以把整条流程
走完而完全不碰 AUR：第 **9、10、13** 步被跳过，`PKGBUILD` 与 `.SRCINFO` 一个字节都不改。

阶段二开始前打印的动作清单**会跟着变**——AUR 的三项不出现、编号重排，并明说「已用
`--no-aur`」。这份清单的用途就是让人在按下回车之前确认「将要发生什么」，列了却不做
等于在骗人。

⚠️ 它**不是演练**：推送 main、打 tag、建 Release 全都照做，**真的会发版**，只是不发 AUR。
只想检查不发版请用 `--dry-run`。

### 为什么第 9 步必须在第 8 步之后

AUR 的 `sha256sums` 要取 **GitHub 上 tag 源码包**的校验和，而那个 tarball 得等 tag
推上去才由 GitHub 生成。所以 AUR 的更新**不可能**提前到阶段一做完——这是整条流程里
唯一无法重排的依赖，脚本里对这段单独写了注释。

（另一条路是让 AUR 用 `git+...#tag=` 源，那样无需校验和、可以提前更新；但版本化发布
用 tarball + 校验和更符合 AUR 惯例，能挡住上游被篡改的情况，所以没有采用。）

### 失败与恢复

- 阶段一任何一步失败：什么都没发出去，改完重跑即可。
- 阶段二中途失败：脚本会明确指出停在哪一步。**不要删 tag 重来**；已经推出去的
  部分保留，缺的部分手工补齐（第 9–13 步都是幂等的，可以单独重跑）。
- 第 12 步失败是**必须停下来查**的——那说明本地打出来的东西和用户下载到的不一致。

---

## 五、回收逻辑

### 5.1 构建与打包产物（已实现：`scripts/clean`）

**触发时机**

1. 发版**开始前**——`scripts/release` 自动调用，保证从干净状态构建
2. 发版**成功后**——同样可以再跑一次
3. **手动**——磁盘紧张时随时

**清理范围（默认）**

| 清 | 位置 | 实测占用 |
|---|---|---|
| ✅ | `target/package/` | 4.7 MiB |
| ✅ | `dist/` 里**非当前版本**的 tarball | 3.2 MiB / 版 |
| ✅ | AUR 仓库的 `src/`、`pkg/` | — |
| ✅ | AUR 仓库里 makepkg 拉下来的上游源码副本 | — |
| ✅ | AUR 仓库里上一次构建出的 `.pkg.tar.zst` | 5.6 MiB |

一轮下来约 **10 MiB**；多留几版就是几百 MiB。

**明确不动**（要清得显式加参数）

| 参数 | 会额外清什么 | 为什么不默认清 |
|---|---|---|
| `--all` | 整个 `target/` | 毁掉增量编译，下次全量重编约 3 分钟。**但它是这里最大的头**：实测 `target/` 有 **26 GiB**（其中 `debug/` 25 GiB——clippy 与测试的产物，`release/` 1.2 GiB）。磁盘紧张时这一条比其它所有加起来都管用 |
| `--cache` | `~/.cache/kugou-tui`（含已下载的歌） | 那是**用户数据**，不是构建产物 |

`--dry-run` 只报告不删。每一处删除前都确认「它确实是我们以为的东西」——AUR 目录里
没有 `PKGBUILD` 就直接跳过，宁可什么都不删。

### 5.2 运行时内存（**方案，尚未实现**）

这一项**没有动代码**。下面是实测到的事实、可选方案与取舍，等确认后再改。

#### 事实（读代码 + 实测得出）

- `StreamingBuffer` 的 `data: Vec<u8>` 是**只追加**的，且**按绝对文件偏移索引**
  （`read` 里取 `data[pos]`）。
- `pos` 是**每个克隆各自持有**的：写入端（下载线程）那份 `pos` 永远是 0，
  **看不到读端读到哪**。所以「回收」不能凭空加，得先让写入端能看到读位置。
- 因此内存占用随曲目体积线性上涨，实测**1:1**（往缓冲里塞 64 MiB，进程 RSS 涨 64 MiB）。
- 同一份数据**同时也在落盘**到缓存（`download.rs` 的流式路径会写缓存文件）。
- 下载端**已经支持 Range 请求**（`ChunkRange` / `Accept-Ranges` / 分块并发），
  也有 `fetch_to`（整首下完再播）这条路径。

#### 触发时机（建议）

在 `push()` 里判断：**已缓冲字节数超过上限（建议 64 MiB）时触发一次**，一次丢弃到
「读位置往前留一个窗口」的位置，而不是每次 push 都动——`Vec::drain` 是 memmove，
频繁调用代价高。这个「超限才动、一次丢到位」的模式与磁盘缓存已有的
`EVICT_TARGET_RATIO`（丢到 80%）保持一致。

不选「定时回收」：没有时钟驱动，且回收的意义只与「相对读位置的落后量」有关。

#### 清理范围

只丢**读位置之前**的字节，且保留一个回看窗口（建议 32 MiB）。
**绝不丢读位置之后的**——那是解码器马上要读的数据。

按这个参数，5 分钟的 320 kbps MP3（约 12 MB）**永远触发不了回收**，
只有大体积的 FLAC / Hi-Res 会被压到 64 MiB 上下（现在是无上限）。

#### 三个方案与取舍

| 方案 | 做法 | 代价 |
|---|---|---|
| **A. 只丢，窗口外 seek 报错** | 改动最小：加 `base` 与共享的 `reader_pos`，丢完就完 | **有 UX 回归**：长曲目往回拖到窗口外会失败。±5 秒跳转（FLAC 24/96 约 2.5 MB）远在窗口内，不受影响 |
| **B. 丢 + 窗口外按 Range 重取** | 下载端已有 Range 能力，新增一条「从偏移续流」的路径 | 中等；要处理重取期间的读阻塞与超时 |
| **C. 丢 + 回落到磁盘缓存文件** | 数据本来就落盘，已落盘的部分从文件读，内存只留还没落盘的尾巴 | 内存最优，但要把 `Read` 的来源从「单一内存缓冲」改成「内存 + 文件」两段，改动最大 |

**建议 B**：它把「回收」变成纯粹的内存优化、不引入行为回归，而 Range 能力已经在了。
A 只适合当中间步骤，C 的收益相对 B 有限（省下那 64 MiB 窗口）而复杂度高一档。

无论选哪个，都要补的测试：`seek` 到已丢弃区间**之前**的位置、`seek` 到正中间、
丢弃与读并发时的边界（`read` 正在读的区间不能被丢）。

#### 落地前提

需要 `StreamingBuffer` 暴露一个「读端位置」给写入端（现在没有），
以及决定上限与回看窗口两个常量。这两项确认后，实现大约 80 行 + 一组单元测试。
