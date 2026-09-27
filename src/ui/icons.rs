//! 界面图标：Nerd Font 优先，没装就退回 ASCII。
//!
//! 终端里「有没有 Nerd Font」只能问系统，不能问终端——同一个 kitty 在装了
//! Nerd Font 的机器上是图标，在没装的机器上一屏豆腐块。所以用 fontconfig
//! 的 `fc-list` 查一次，结果缓存住（`OnceLock`），别每次渲染都 spawn 进程。
//!
//! # Windows
//!
//! 那边没有 fontconfig，`fc-list` 这个命令根本不存在。字体清单在注册表里
//! （`HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Fonts`），为一个图标
//! 去读注册表不划算，所以**默认按没有 Nerd Font 处理**（退回 ASCII，不会出错），
//! 装了 Nerd Font 的用户用 `KUGOU_TUI_NERD_FONT=1` 显式打开。
//! 这个开关在两个平台上都生效，也方便 Linux 上装了非 fontconfig 字体的人手动覆盖。

use std::sync::OnceLock;

/// 系统是否装了 Nerd Font。只查一次。
///
/// 判定顺序：`KUGOU_TUI_NERD_FONT` 显式开关 → fontconfig 探测 → 没有。
fn has_nerd_font() -> bool {
    static CACHED: OnceLock<bool> = OnceLock::new();
    *CACHED.get_or_init(|| {
        // 显式开关优先：有 fontconfig 但没装 Nerd Font 的人也能靠它强开，
        // 反过来（装了但 fc-list 查不到）也能靠它兜住。
        if let Some(value) = std::env::var_os("KUGOU_TUI_NERD_FONT") {
            return !value.is_empty() && value != "0";
        }
        detect_via_fontconfig()
    })
}

/// 用 fontconfig 的 `fc-list` 找 Nerd Font。
///
/// 非 Linux（Windows/macOS）上没有这个命令，`Command` 会返回 `Err`，
/// 于是当「没装」处理——退回 ASCII 比画一屏豆腐块安全。
fn detect_via_fontconfig() -> bool {
    let Ok(output) = std::process::Command::new("fc-list")
        .arg(":family")
        .output()
    else {
        return false;
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|line| line.to_lowercase().contains("nerd"))
}

/// 取一个图标：装了 Nerd Font 就用 `nerd`，否则用 `ascii`。
///
/// 所有图标都走这里，保证同一个界面里不会混着两种风格。
fn icon(nerd: &'static str, ascii: &'static str) -> &'static str {
    if has_nerd_font() { nerd } else { ascii }
}

// ==================================================================
// 侧边栏 / 标签页图标
// ==================================================================

pub fn search() -> &'static str {
    icon("\u{f002}", ">")
}

pub fn playlists() -> &'static str {
    icon("\u{f001}", "*")
}

pub fn artist() -> &'static str {
    icon("\u{f2bd}", "@")
}

pub fn rank() -> &'static str {
    icon("\u{f080}", "#")
}

pub fn cloud() -> &'static str {
    icon("\u{f0c2}", "~")
}

pub fn home() -> &'static str {
    icon("\u{f015}", "H")
}

pub fn queue() -> &'static str {
    icon("\u{f03a}", "=")
}

pub fn visualizer() -> &'static str {
    // 面积图（area-chart）比波形（U+F6FE）更像频谱柱状，码点也更常见
    icon("\u{f1fe}", "^")
}

pub fn sources() -> &'static str {
    icon("\u{f0ec}", "<>")
}

pub fn settings() -> &'static str {
    icon("\u{f013}", "!")
}

// ==================================================================
// 播放条图标
// ==================================================================

/// 正在播放 / 暂停 / 缓冲 / 停止。
pub fn now_playing(playing: bool) -> &'static str {
    if playing {
        icon("\u{f04b}", ">>")
    } else {
        icon("\u{f04c}", "||")
    }
}

pub fn loading() -> &'static str {
    icon("\u{f250}", "..")
}

pub fn stopped() -> &'static str {
    icon("\u{f04d}", "--")
}

/// 「下一首」提示。
pub fn next_up() -> &'static str {
    icon("\u{f064}", ">")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 两个分支都必须返回非空——ASCII 回退要是空字符串，界面就塌一块。
    #[test]
    fn icons_are_never_empty() {
        assert!(!search().is_empty());
        assert!(!playlists().is_empty());
        assert!(!artist().is_empty());
        assert!(!rank().is_empty());
        assert!(!cloud().is_empty());
        assert!(!home().is_empty());
        assert!(!queue().is_empty());
        assert!(!visualizer().is_empty());
        assert!(!sources().is_empty());
        assert!(!settings().is_empty());
        assert!(!now_playing(true).is_empty());
        assert!(!now_playing(false).is_empty());
        assert!(!loading().is_empty());
        assert!(!stopped().is_empty());
        assert!(!next_up().is_empty());
    }
}
