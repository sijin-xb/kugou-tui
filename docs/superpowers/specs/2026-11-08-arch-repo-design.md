# Arch Linux 安装渠道设计

日期：2026-11-08
状态：待批准

## 意图

让 Arch 系用户用两条命令装好 kugou-tui，不需要 Rust 工具链、不需要编译：

```bash
curl -fsSL https://sijin-xb.github.io/kugou-tui-repo/install.sh | sudo bash
sudo pacman -S kugou-tui
```

成功标准：一台干净的 Arch/CachyOS 上，上面两条命令跑完，`kugou-tui` 能直接启动并播放音乐。

## 非目标

- **不做 AUR 包**。AUR 要求用户在本地编译，与本设计要解决的「不装 Rust 工具链」直接冲突。若将来需要，另开一份 `PKGBUILD` 即可，两者不互斥。
- **不预装 KuGouMusicApi**。默认后端是 `native`，接口内嵌在二进制里，预装的服务对绝大多数用户是几十 MB 死重。
- **不做 aarch64 / armv7**。上游 Release 也只出 x86_64 Linux。
- **不做 GPG 签名**。
- **不装启动脚本**。`scripts/kugou-tui` 的唯一价值在 `--api node` 回退路径，默认路径下它只是 `exec` 二进制。

## 已确认的约束

来自实机核对（2026-11-08）：

- 二进制 `ldd` 只链 `libasound.so.2`、`libgcc_s.so.1`、`libm.so.6`、`libc.so.6`。
- `Cargo.toml`：`edition = "2024"`，`rust-version = "1.90"`，`license = "AGPL-3.0-or-later"`。
- release profile 已含 `lto = "fat"`、`codegen-units = 1`、`panic = "abort"`、`strip = true`。
- `aws-lc-sys` 的 C 代码经不起外层 `-flto`，`makepkg.conf` 默认 `LTOFLAGS="-flto=auto"`，因此 `options=('!lto')` 必须保留。
- 本机 `makepkg.conf` 的 `CFLAGS="-march=native"`，**本机产物不可直接分发**；包必须在 CI 构建。
- `aur/kugou-tui/PKGBUILD` 停留在 0.3.9，不是 git 仓库，从未推送到 AUR。它是历史遗留，本设计不复用它，但其中的 `!lto` 理由与 `cargo build --release --locked` 写法沿用。

## 架构

三个产物，分属两个仓库：

| 产物 | 位置 | 职责 |
|---|---|---|
| `PKGBUILD` | 主仓库 `packaging/arch/PKGBUILD` | 定义如何构建与打包 |
| 仓库发布工作流 | 主仓库 `.github/workflows/arch-repo.yml` | 打 tag 时构建 `.pkg.tar.zst`、更新索引、推到 Pages |
| 静态仓库 + `install.sh` | 新仓库 `sijin-xb/kugou-tui-repo` | 用户 `curl` 拿到的脚本，以及 pacman 读的 db |

### 为什么 PKGBUILD 放主仓库

放主仓库可以和版本号一起改、一起 review、一起打 tag。工作流 checkout 主仓库时直接就能用，不需要跨仓库同步。新仓库只存**产物**（`.pkg.tar.zst`、`.db`、`install.sh`），是纯输出目录。

### 仓库目录布局（Pages 站点）

```
/                                   # 分支 gh-pages 根
├── install.sh                      # 用户 curl 的脚本
├── x86_64/
│   ├── kugou-tui-0.8.1-1-x86_64.pkg.tar.zst
│   ├── kugou-tui-0.8.1-1-x86_64.pkg.tar.zst.sig   # 不签则不存在
│   ├── kugou-tui.db                # repo-add 产出
│   └── kugou-tui.db.tar.gz         # repo-add 产出（符号链接或副本）
└── .nojekyll                       # 防止 GitHub Pages 的 Jekyll 吞掉文件
```

pacman.conf 里 `Server = https://sijin-xb.github.io/kugou-tui-repo/$arch`，`$arch` 展开为 `x86_64`。

**保留旧版本包**：`repo-add` 只更新索引，不删旧文件。保留历史版本让用户能用 `pacman -U` 回滚。CI 里不清空目录，只 `repo-add` 新包。

### 发布流程

复用现有的 tag 触发（`v*`）：

1. `arch-repo.yml` 在 `push: tags: ['v*']` 时触发，`runs-on: ubuntu-latest`。
2. 用 `archlinux:latest` 容器（或 `archlinux/archlinux` action）装 `base-devel`、`alsa-lib`、`pkgconf`、`rust`。
3. `makepkg --syncdeps --noconfirm --cleanbuild`（在 `PKGBUILD` 所在目录，需非 root 用户，容器内建一个 builder 用户）。
4. `repo-add kugou-tui.db.tar.gz *.pkg.tar.zst`。
5. 把 `*.pkg.tar.zst`、`kugou-tui.db*`、`install.sh` 推到 `kugou-tui-repo` 的 `gh-pages` 分支（用 deploy key 或 `GITHUB_TOKEN` + 跨仓库 push 权限）。

**跨仓库推送的认证**：`GITHUB_TOKEN` 只对当前仓库有效。需要一个 fine-grained PAT 或 deploy key，存在主仓库的 secret 里。这是本设计唯一的额外凭据。

### install.sh 的行为

必须是幂等的，重复运行不产生重复配置：

1. 检查 `ID_LIKE` 含 `arch`（`/etc/os-release`），不是就报错退出。
2. 若 `/etc/pacman.conf` 已有 `[kugou-tui]` 段，跳过追加。
3. 追加：
   ```
   [kugou-tui]
   SigLevel = Optional TrustAll
   Server = https://sijin-xb.github.io/kugou-tui-repo/$arch
   ```
4. 打印下一步：`sudo pacman -Sy && sudo pacman -S kugou-tui`。

**不自动 `pacman -Sy`**：脚本由 `curl | sudo bash` 执行，替用户刷新数据库是越权。让用户自己敲。

**`$arch` 不展开**：`pacman.conf` 的 `Server` 行里 `$arch` 由 pacman 自己展开，脚本要原样写入（用单引号 heredoc）。

## 包定义要点

```bash
pkgname=kugou-tui
pkgver=0.8.1          # 由 CI 从 tag 注入，不手改
pkgrel=1
arch=('x86_64')
license=('AGPL-3.0-or-later')
depends=('alsa-lib' 'gcc-libs' 'glibc')
makedepends=('cargo' 'cmake' 'pkgconf')
options=('!lto')
```

- `cmake` 是 `aws-lc-sys` 的构建依赖，不能省。
- `source=` 用本地 tarball 还是 `git+...#tag=v$pkgver`：**用 git tag**，与现有 release 工作流一致，且 `--locked` 能保证依赖树固定。
- `package()` 只装 `target/release/kugou-tui` → `/usr/bin/kugou-tui`，加 `LICENSE` 到 `/usr/share/licenses/kugou-tui/`。
- **不装** `scripts/*`。用户在需要 `--api node` 时用二进制自带的 `--api-start`（见 README）。

## 留给你决定的部分

无。上面所有选择都已在对话中确认。

## 留给实施者的判断

- `archlinux` 容器里 `makepkg` 不能以 root 跑，需要建普通用户并 `chown` 构建目录。
- GitHub Pages 对 `gh-pages` 分支有大小限制（仓库 1 GB 软限制）。每个 `.pkg.tar.zst` 约 5–8 MB，保留全部历史版本多年后可能触顶。实施时加一个「只保留最近 N 个版本」的清理步骤，N=20。
- `install.sh` 里的 URL 硬编码 `sijin-xb.github.io/kugou-tui-repo`，换托管时要改这一处和 `PKGBUILD` 里的 `url`。
