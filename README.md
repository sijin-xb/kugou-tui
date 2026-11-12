# kugou-tui

终端里的酷狗音乐播放器。默认使用内嵌 Rust 接口，无需 Node.js 即可播放。

![歌单、播放、逐字歌词和队列](assets/screenshot-0.6.2.png)

## 安装

```bash
cargo install kugou-tui
```

从当前源码安装：

```bash
cargo install --git https://github.com/sijin-xb/kugou-tui
```

Arch Linux 推荐使用项目仓库，步骤见 [安装说明](docs/arch-repository.md)。

源码构建需要 Rust 1.90 或更新版本。
Linux 还需要 `alsa-lib` 与 `pkg-config`。

## 启动

```bash
kugou-tui
```

按 `2` 搜索，`Enter` 播放。
按 `?` 查看完整帮助，按 `q` 保存并退出。

## 文档

- [使用指南](docs/usage.md)
- [架构与实现](docs/architecture.md)
- [Arch Linux 软件仓库](docs/arch-repository.md)
- [更新日志](CHANGELOG.md)
- [问题反馈](https://github.com/sijin-xb/kugou-tui/issues)

## 开发

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

发版使用 `./scripts/release`。

## 许可

项目以 AGPL-3.0-or-later 授权，正文见 [LICENSE](LICENSE)。
项目不分发音乐内容。
