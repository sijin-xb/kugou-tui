# Arch Linux 软件仓库

## 组成

Arch 安装使用两个仓库：

| 属性 | 仓库 | 内容 |
|---|---|---|
| 主仓库 | `sijin-xb/kugou-tui` | `packaging/arch/` 与构建工作流 |
| Pages 仓库 | `kugou-tui-repo` | 包、索引和 `install.sh` |

主仓库负责构建。
Pages 仓库只保存发布产物。

## 用户安装

```bash
curl -fsSL https://sijin-xb.github.io/kugou-tui-repo/install.sh -o /tmp/kugou-tui-install.sh
less /tmp/kugou-tui-install.sh
sudo bash /tmp/kugou-tui-install.sh
sudo pacman -Syu --needed kugou-tui
```

安装脚本会检查 Arch 系发行版和 `pacman`。
首次运行时，它会备份 `/etc/pacman.conf`，再追加 `[kugou-tui]` 仓库段。

脚本可重复运行。
仓库段已存在时不会重复写入。

## 发布流程

`.github/workflows/arch-repo.yml` 在推送 `v*` tag 时运行。
也可以通过 `workflow_dispatch` 为指定 tag 补包。

构建阶段使用 `archlinux:latest` 容器：

1. 从 `main` 检出主仓库。
2. 从 tag 注入 `pkgver`。
3. 创建非 root `builder` 用户。
4. 缓存 Cargo registry、git 和 target。
5. 执行 `makepkg --syncdeps --cleanbuild --noconfirm --force`。
6. 从生成的包中运行 `kugou-tui --version`，核对 tag 版本。

发布阶段检出 `kugou-tui-repo` 的 `gh-pages` 分支：

1. 将 `.pkg.tar.zst` 和 `install.sh` 放入 Pages 站点。
2. 排除 `-debug` 包。
3. 使用 `repo-add` 更新 `kugou-tui.db.tar.gz`。
4. 只保留最近 20 个主包版本。
5. 重新生成索引并提交到 `gh-pages`。

## 包定义

`packaging/arch/PKGBUILD` 定义以下内容：

- 包名 `kugou-tui`
- `x86_64` 架构
- AGPL-3.0-or-later 许可
- `alsa-lib`、`gcc-libs`、`glibc` 运行依赖
- `cargo`、`cmake`、`pkgconf` 构建依赖
- Node.js 与 npm 可选依赖

构建使用 `cargo build --release --locked`。
安装内容包含二进制、许可证、README、CHANGELOG 和 `docs/`。

`options=('!lto' '!debug')` 用于避免外层 LTO 与 Rust release profile 叠加。
关闭 debug 包是因为 release 二进制已经 strip。

## 凭据

跨仓库推送需要 `ARCH_REPO_TOKEN`。
该 token 必须对 `sijin-xb/kugou-tui-repo` 拥有 `contents: write`。

GitHub Actions 的 `GITHUB_TOKEN` 只对主仓库有效。
Token 通过环境变量传给 git，不写入 remote URL 或日志。

## 版本核对

工作流不直接相信 `PKGBUILD` 中的版本字符串。
构建完成后会从 `.pkg.tar.zst` 提取二进制并执行 `--version`。

版本不一致时发布失败。
这样可以发现 tag、源码归档和包版本之间的偏差。

## 保留策略

每个包约 5–8 MiB。
仓库只保留最近 20 个版本，避免超过 GitHub Pages 的 1 GB 软限制。

每次删除旧包后都会重新运行 `repo-add`。
索引不会继续引用已删除的包。
