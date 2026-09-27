//! 窗口控制（目前只支持 niri）。
//!
//! # 为什么需要它
//!
//! 终端程序没法自己最小化窗口——Wayland 的 xdg-shell 里没有这个请求。想「把 TUI
//! 收起来继续放歌」就得走 compositor 的 IPC：niri 提供 `minimize-window` /
//! `toggle-window-minimized`，而且都能带 `--id` 精确指定窗口。
//!
//! # 怎么找到「自己的窗口」
//!
//! niri 给的窗口 `pid` 是**终端模拟器**的（kitty、alacritty…），不是 kugou-tui 的，
//! 匹配不上；tty 也关联不起来（窗口信息里没有这个字段）。所以改用**窗口标题**：
//! 启动时通过 OSC 序列把终端标题设成 [`WINDOW_TITLE`]，之后按标题找回自己的窗口。
//! 这也是终端程序标识自身的常规做法，顺带让用户在窗口列表里认得出来。
//!
//! # 自适配
//!
//! 没有 `NIRI_SOCKET` 时 [`available`] 返回 false，调用方据此**不提供入口**
//! （托盘菜单里干脆不出现那一项，免得点了没反应）。找不到窗口、命令失败都只记
//! 一行日志，不打扰界面。
//!
//! # 非 Linux
//!
//! 整套机制依赖 niri 的 IPC socket，Windows / macOS 上 `NIRI_SOCKET` 必然不存在，
//! [`available`] 恒为 false，于是调用方永远不会走到 [`toggle_minimized`]。
//! 也就是说**这里不需要平台分支**：降级路径本来就是「不提供入口」。
//! 真要支持 Windows，得改用 `GetConsoleWindow` + `ShowWindow(SW_MINIMIZE)`，
//! 那是另一套语义（最小化的是控制台窗口本身，不是终端模拟器的窗口）。
//! 唯一的例外是 [`set_terminal_title`]——OSC 序列是终端标准，Windows Terminal
//! 也认，所以那边照常设标题。

use std::process::Command;

/// 终端标题，同时充当「找回自己窗口」的标记。
const WINDOW_TITLE: &str = "kugou-tui";

/// 当前会话能不能控制窗口（也就是：跑在不在 niri 下）。
pub fn available() -> bool {
    std::env::var_os("NIRI_SOCKET").is_some()
}

/// 把终端标题设成 [`WINDOW_TITLE`]。
///
/// **必须在进入 alternate screen 之后调用**：部分终端在切换屏幕时会把标题重置回去。
pub fn set_terminal_title() {
    use ratatui::crossterm::execute;
    use ratatui::crossterm::terminal::SetTitle;
    let _ = execute!(std::io::stdout(), SetTitle(WINDOW_TITLE));
}

/// 切换「自己的窗口」的最小化状态，返回是否成功。
pub fn toggle_minimized() -> bool {
    let Some(id) = own_window_id() else {
        crate::logger::tlog!(
            crate::logger::LEVEL_WARN,
            "niri 窗口列表里没有标题含 {WINDOW_TITLE} 的窗口，无法最小化"
        );
        return false;
    };
    run(&[
        "msg",
        "action",
        "toggle-window-minimized",
        "--id",
        &id.to_string(),
    ])
}

/// 在 niri 的窗口列表里按标题找回自己的窗口。
fn own_window_id() -> Option<u64> {
    let output = Command::new("niri")
        .args(["msg", "--json", "windows"])
        .output()
        .ok()?;
    if !output.status.success() {
        crate::logger::tlog!(
            crate::logger::LEVEL_WARN,
            "niri msg --json windows 失败：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        return None;
    }
    let windows: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    pick_window_id(&windows)
}

/// 从 niri 的窗口列表里挑出标题匹配的那一个。
///
/// 抽成纯函数是为了能直接测：这里判错了，表现是「点了最小化，结果收起来的是
/// 别人的窗口」——比不生效更难查。
fn pick_window_id(windows: &serde_json::Value) -> Option<u64> {
    windows
        .as_array()?
        .iter()
        .find(|window| {
            window
                .get("title")
                .and_then(|title| title.as_str())
                // **精确相等，不是 `contains`**。实测真会撞车：用户浏览器里开着标题为
                // `Comparing sijin-xb:main... · sijin-xb/kugou-tui - Google Chrome` 的
                // 标签页，用 `contains` 时它和我们的窗口一起命中，谁排在前面就把谁最小化
                // ——表现是「点了最小化，浏览器没了」。kugou-tui 是用 OSC 0 **覆盖**
                // 标题的，正常情况下恰好等于这个值。
                .is_some_and(|title| title == WINDOW_TITLE)
        })
        .and_then(|window| window.get("id"))
        .and_then(|id| id.as_u64())
}

/// 跑一条 niri 子命令，失败时把 stderr 记进日志。
fn run(args: &[&str]) -> bool {
    match Command::new("niri").args(args).output() {
        Ok(output) if output.status.success() => true,
        Ok(output) => {
            crate::logger::tlog!(
                crate::logger::LEVEL_WARN,
                "niri {args:?} 失败：{}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
            false
        }
        Err(error) => {
            crate::logger::tlog!(crate::logger::LEVEL_WARN, "执行 niri 失败：{error}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 照抄 `niri msg --json windows` 的真实形状（字段名与类型都没改）。
    fn sample() -> serde_json::Value {
        serde_json::json!([
            {
                "id": 7,
                "title": "QQ",
                "app_id": "QQ",
                "pid": 1174,
                "is_minimized": false
            },
            {
                "id": 27,
                "title": "~: dms run - dms",
                "app_id": "kitty",
                "pid": 41547,
                "is_minimized": false
            },
            {
                "id": 31,
                "title": "kugou-tui",
                "app_id": "kitty",
                "pid": 41547,
                "is_minimized": true
            }
        ])
    }

    #[test]
    fn picks_the_window_whose_title_matches() {
        assert_eq!(pick_window_id(&sample()), Some(31));
    }

    /// **只认精确相等**：标题里含 `kugou-tui` 但不是它本人的窗口必须跳过。
    ///
    /// 这条是实测踩出来的——用户浏览器里开着标题为
    /// `Comparing sijin-xb:main... · sijin-xb/kugou-tui - Google Chrome` 的标签页，
    /// 用 `contains` 时它和我们的窗口一起命中，排在前面的那个就被最小化了。
    #[test]
    fn ignores_windows_that_merely_contain_the_name() {
        let windows = serde_json::json!([
            { "id": 20, "title": "Comparing sijin-xb:main... · sijin-xb/kugou-tui - Google Chrome" },
            { "id": 5, "title": "~/kugou-tui: kugou-tui --api-base http://127.0.0.1:3001" },
            { "id": 34, "title": "kugou-tui" }
        ]);
        assert_eq!(
            pick_window_id(&windows),
            Some(34),
            "只有精确相等的那个才是我们的窗口"
        );
    }

    /// 只有撞名的窗口、没有我们自己时返回 `None`——宁可什么都不做，
    /// 也不能去动别人的窗口。
    #[test]
    fn returns_none_when_only_lookalikes_exist() {
        let windows = serde_json::json!([
            { "id": 20, "title": "sijin-xb/kugou-tui - Google Chrome" }
        ]);
        assert_eq!(pick_window_id(&windows), None);
    }

    #[test]
    fn returns_none_when_no_window_matches() {
        let windows = serde_json::json!([
            { "id": 1, "title": "QQ" },
            { "id": 2, "title": "~: nvim" }
        ]);
        assert_eq!(pick_window_id(&windows), None);
    }

    /// 字段缺失或形状不对时返回 `None`，不能 panic——niri 的版本差异
    /// （比如某个字段被改名）不该把程序带崩。
    #[test]
    fn tolerates_missing_or_malformed_fields() {
        assert_eq!(pick_window_id(&serde_json::json!({})), None);
        assert_eq!(pick_window_id(&serde_json::json!([])), None);
        assert_eq!(
            pick_window_id(&serde_json::json!([{ "title": "kugou-tui" }])),
            None,
            "没有 id 字段时应当放弃"
        );
        assert_eq!(
            pick_window_id(&serde_json::json!([{ "id": 3 }])),
            None,
            "没有 title 字段时应当放弃"
        );
        assert_eq!(
            pick_window_id(&serde_json::json!([{ "id": "3", "title": "kugou-tui" }])),
            None,
            "id 是字符串（类型不符）时应当放弃而不是猜"
        );
    }
}
