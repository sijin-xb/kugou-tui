# 许可

## 本项目

本项目采用 **MIT**（© 2026 kugou-tui contributors）。

许可原文在仓库根目录的 `LICENSE`；用包管理器装的话，它在
`/usr/share/licenses/kugou-tui/LICENSE`。

> 这里刻意不用相对链接指向 `LICENSE`：这份文档会被装到
> `/usr/share/doc/kugou-tui/docs/`，而许可按 Arch 惯例放在 `/usr/share/licenses/`
> 下——相对链接在安装后必然失效。写路径反而两种场合都读得懂。

## 第三方依赖许可

`Cargo.lock` 共 **478 个包**（含本仓库自己；不区分目标平台）。去掉本仓库后
**477 个第三方依赖**，分布如下：

| 许可 | 数量 | 说明 |
|---|---|---|
| `MIT` / `Apache-2.0` 及其各种组合写法（含 BSD / ISC / Zlib / Unlicense / WTFPL / 0BSD / BSL-1.0 / CC0 / CDLA / LLVM-exception，以及 Unicode 相关） | **464** | 宽松，任选其一即可 |
| **`MPL-2.0`** | **13** | **弱著佐权**：symphonia 系列（FLAC / MP3 / Vorbis / AAC 等解码器）与 `option-ext`。文件级 copyleft，静态链接分发需保留其源码可得性 |
| 清单里没有 `license` 字段 | **0** | — |

统计方式：`cargo metadata`（从 registry 索引与清单读 `license`，**不依赖本机是否
解包过某个 crate**）。复核只需在仓库根目录跑一次：

```bash
python3 scripts/license-stats.py
```

> 早先这个脚本直接读 `$CARGO_HOME/registry/src/<name>-<version>/Cargo.toml`，于是
> 「读不到」的数量随本机缓存漂移——同一份 `Cargo.lock`，一次跑出 26 个读不到、
> 一次跑出 102 个，数字根本没法复核（本文件旧版那张表就是被这个坑带偏的）。
> 换成 `cargo metadata` 后，结果只取决于 `Cargo.lock`。

需要注意的三点：

1. **MPL-2.0**：若以二进制形式分发本项目，需保证 symphonia 相关 MPL 代码的源码可得
   （保留 `Cargo.lock` 与目标平台的依赖获取方式即满足）。另外 `termina` 是
   `MIT OR MPL-2.0`，属于**可选**双许可，整体按 MIT 用即可，不构成著佐权义务。
2. **`ring` / `aws-lc-rs` / `aws-lc-sys`**：加密库，随 **非 Windows** 的
   `reqwest` + `rustls` 引入（Windows 上 TLS 走系统 SChannel，见 `Cargo.toml`）。
   许可是 `Apache-2.0 AND ISC`、`ISC AND (Apache-2.0 OR ISC)` 这类**必须同时满足**的组合；
   `aws-lc-sys` 还叠了 BSD-3-Clause 与 MIT。这些库在某些司法辖区可能涉及出口管制，
   商业分发前请自行确认。
3. **`native-tls` / `openssl` / `openssl-sys` / `foreign-types` / `vcpkg`**：
   这张表**不区分目标平台**，所以它们也在清单里——那是 `native-tls` 为
   **非 Windows、非 macOS** 目标声明的 OpenSSL 后端。本项目只在
   `cfg(windows)` 下使用 `native-tls`（那里它走 `schannel`，不碰 OpenSSL），
   Linux 构建完全不编译这几个包，实际产物里没有 OpenSSL 代码。
4. **出现 GPL / LGPL 字样的 3 个包**均为**可选**双许可，整体按宽松许可使用即可，
   不构成著佐权义务：`self_cell`（`Apache-2.0 OR GPL-2.0-only`，随 ratatui-image
   引入）、`r-efi` 两个版本（`MIT OR Apache-2.0 OR LGPL-2.1-or-later`）。

> 本页只做**分类统计**，不判断许可兼容性。想更严格的话用
> [`cargo-deny`](https://github.com/EmbarkStudios/cargo-deny) 或
> [`cargo-about`](https://github.com/EmbarkStudios/cargo-about)。
