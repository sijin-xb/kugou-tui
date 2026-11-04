# AGENTS.md

给在本仓库里改代码的 AI agent 看的约定。人类协作者读着同样有效。

## 项目是什么

终端里的酷狗音乐播放器。单进程二进制，接口内嵌（`--api native`），可选回退到本机 Node 服务。Rust 2024 edition，MSRV 1.90（由依赖决定，见 `Cargo.toml` 顶部注释）。

## 提交前必跑

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

三者必须全绿。CI 在 Linux / Windows / macOS 三栏各跑一遍 clippy + test（见 `.github/workflows/ci.yml`）。

发版走 `./scripts/release`，它分两阶段：先在本机跑完 `make-release-tarball` 停下等确认，确认后才推 tag。

## 目录结构

| 路径 | 职责 |
|---|---|
| `src/api/` | 接口客户端。`native/` 是内嵌的纯 Rust 实现，`node.rs` 是回退 |
| `src/source/` | 音源抽象：酷狗、网易云、汽水各一套；`sodam/` 是汽水 |
| `src/audio/` | 播放引擎、缓存、下载、重采样、`cava.rs`（频谱算法） |
| `src/app/` | 状态与业务逻辑。`state.rs` 是全局状态，`update.rs` 是事件总线 |
| `src/ui/` | 渲染。`theme.rs` 配色、`widgets.rs` 原语、`hit.rs` 命中区、`views/` 各页面 |
| `crates/libresoda/` | 汽水音乐数据层，独立 crate |

## 改 UI 前先读这两处

**`src/ui/widgets.rs` 的 `panel()`。** 圆角闭合边框 + 左右各 1 列内边距（`Padding::horizontal(1)`）：内容区比面板**窄 4 列、矮 2 行**，算布局预算按这个来。标题画在边框线上，不额外吃行；但太窄的面板（比如 2 行高的）塞不下内容，先想清楚再挂。

**`src/ui/hit.rs` 的 `HitMap`。** 鼠标命中区由渲染层每帧回填，事件层查表。不要在视图里自己算「鼠标落在第几行」——用 `hits.data_index_under(inner, offset, len)`。新增可点元素时：在 `HitTarget` 加变体 → 渲染时 `hit_map.insert_*` → `app/update.rs` 的 `click_at` 与三处「不处理」分支都要补齐。

## 不变量（违反会静默出错）

1. **`Cava::execute` 返回的是内部缓冲的借用**，下一帧会被覆盖。调用方必须立刻拷贝。
2. **cava 的 `input` 缓冲是倒序的**（`input[0]` 最新）。改成正序会让频谱泄漏大三个数量级，柱子糊成一片宽丘。
3. **`plan_bands` 的 `first_bar` 是跨迭代携带的状态**，不是循环内局部变量。写成局部变量会让低频段边界回退。
4. **`min_bandwidth` 是整数除法**（`rate / bass_size`），与上游一致。改成浮点会让频段整体右移。
5. **图片协议探测必须先排除 tmux/screen**。`KITTY_WINDOW_ID` 会被 tmux 继承，但 tmux 不透传图形协议——不排除的话封面会变成一行乱码。
6. **时间相关的缓动按真实经过时间算，不按帧数**。帧率从 5fps 提到 30fps 时观感必须不变。

## 注释与文档

注释只写「不写就会有人踩坑」的边角：反直觉的副作用、必须按特定顺序调用的约束、看起来能简化但不能简化的原因。**不写**模块用途、架构分层、复述代码、设计论证。

文档语言：`README.md`、`CHANGELOG.md` 用中文。代码注释能用英文就用英文。

`CHANGELOG.md` 用陈述式，只写「改了什么」，不写症状描述与修复过程。按 Keep a Changelog 分类。

## 平台差异

- **Windows**：没有 fontconfig（Nerd Font 默认按没有处理）、没有 D-Bus（MPRIS 与托盘整块 `cfg(unix)` 掉）、TLS 走 SChannel 而不是 rustls。
- **Unix**：依赖 `alsa-lib` 开发头文件与 `pkg-config`。
- 跨平台改动要在 `cargo check --target` 之外真的跑测试——路径展开、配置目录、stderr 重定向这些只有运行时才露出来。

## 依赖

加依赖前先问。现有依赖每一处取舍都写在 `Cargo.toml` 的注释里，改之前读那一段。

**不要**为了「更现代」把 `ratatui` 换成 `cursive` / `tui-realm`。`src/ui/` 约 8.7 千行、`src/app/` 约 1.3 万行，全部围绕 ratatui 的 `Frame` / `Layout` / `ListState` 写；cursive 的线性布局做不出「侧边栏自适应 + 窄屏歌词上下对半」，也没有 `ratatui-image` 的等价物。

## 已验证的实测数据

改动涉及性能时，用这些做基线（本机 CachyOS，120×34 终端，播放 44.1kHz MP3）：

| 指标 | 值 | 测法 |
|---|---|---|
| 播放中 RSS | ~16 MiB | `grep VmRSS /proc/<pid>/status` |
| 可视化页 30fps RSS | ~16 MiB，稳定不涨 | 同上，切到 `0` 页等 8 秒 |
| 二进制体积 | ~11.8 MiB（`ls` 显示 12M） | `ls -lh target/release/kugou-tui` |
| cava 频谱与 libcava 1.0.0 对拍 | 相对误差 1e-5 | 见 `src/audio/cava.rs` 测试注释 |

内存追踪：`KUGOU_TUI_MEM_TRACE=1`，日志每 5 秒一行 `[mem]`。

## 调试手段

- `KUGOU_TUI_DEBUG=1` → 详细日志（按键映射、出站请求）
- `KUGOU_TUI_MEM_TRACE=1` → 内存时间线
- `KUGOU_TUI_NERD_FONT=1` → 强制启用图标字体（覆盖自动探测）
- 日志在 `~/.cache/kugou-tui/kugou-tui.log`（权限 0600），凭据已自动脱敏
- `cargo test dump_a_frame -- --ignored --nocapture` → 把整帧渲染成纯文本打出来，改布局时看观感用
