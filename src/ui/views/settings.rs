//! 设置页。
//!
//! 一行一项：左边名字、中间当前值、右边一句说明。选中行高亮，按 ← → 或 Enter
//! 改值，↑ ↓ 移动，鼠标点击选中（再点一次改值）。
//!
//! # 为什么值放在中间而不是最右
//!
//! 名字左对齐、值紧跟其后，眼睛扫下来是一条固定的阅读线；说明放最右是次要信息，
//! 看不清也不影响操作。反过来（值贴右边界）会让「哪一项对应哪个值」要靠对齐去猜。

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::state::{AppState, HitTarget};
use crate::ui::theme::Theme;
use crate::ui::widgets::{panel, truncate_to_width};

/// 名字列的宽度。按最长的「播放模式」「歌词偏移」留够。
const LABEL_WIDTH: u16 = 12;
/// 值列的宽度。够放下「标准 128kbps」这类最长的值。
const VALUE_WIDTH: u16 = 18;

/// 画设置页。`values` 由调用方把每个条目的当前值翻成文字再传进来——
/// 这一层不碰 `Config`，纯渲染。
pub fn render_settings(
    frame: &mut Frame,
    area: Rect,
    state: &mut AppState,
    values: &[String],
    focused: bool,
    theme: &Theme,
) {
    let block = panel("设置", focused, theme);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height < 2 || inner.width < 24 {
        return;
    }

    // 底部一行操作提示，其余留给条目
    let [list_area, hint_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);

    let rows = list_area.height as usize;
    let total = values.len();
    // 选中项始终可见：滚动窗口跟着光标走，和列表页一个手感
    let offset = state.settings_cursor.min(total.saturating_sub(rows));

    for row in 0..rows {
        let index = offset + row;
        let Some(setting) = crate::app::settings::Setting::ALL.get(index) else {
            break;
        };
        let rect = Rect::new(list_area.x, list_area.y + row as u16, list_area.width, 1);
        let selected = index == state.settings_cursor;
        let hovered = state.hit_map.is_over(rect);
        // 选中态优先：悬停只是「鼠标在这行上」，不该盖掉光标所在的位置。
        // 两者同时存在时选中仍然是最强的视觉信号。
        let style = if selected {
            theme.selection().add_modifier(Modifier::BOLD)
        } else if hovered {
            theme.body().add_modifier(Modifier::BOLD)
        } else {
            theme.body()
        };

        let value = values.get(index).map(String::as_str).unwrap_or("");
        let text = format!(
            "{:<label$}  {:<value$}  {}",
            setting.label(),
            value,
            setting.description(),
            label = LABEL_WIDTH as usize,
            value = VALUE_WIDTH as usize,
        );
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                truncate_to_width(&text, list_area.width as usize),
                style,
            ))),
            rect,
        );
    }

    // 点击选中、再点一次改值，所以整块列表区都要能命中
    if list_area.height > 0 {
        state
            .hit_map
            .insert(list_area, HitTarget::Settings, offset, total);
    }

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            truncate_to_width(
                "↑↓ 选择 · ←→ / Enter 修改 · ? 帮助 · q 退出",
                hint_area.width as usize,
            ),
            theme.dim(),
        )))
        .alignment(Alignment::Center),
        hint_area,
    );
}

/// 给「即将改变」的值加个标记，让改动的反馈更明确。
///
/// 目前只用在状态栏提示里：设置页本身用高亮行表达选中。
pub fn change_notice(label: &str, value: &str) -> String {
    format!("{label} → {value}")
}

/// 开关型条目的显示文本。
pub fn toggle_text(on: bool) -> String {
    if on {
        "开".to_string()
    } else {
        "关".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toggle_text_reads_as_on_off() {
        assert_eq!(toggle_text(true), "开");
        assert_eq!(toggle_text(false), "关");
    }

    #[test]
    fn change_notice_shows_before_and_after() {
        assert_eq!(change_notice("主题", "日落"), "主题 → 日落");
    }

    /// 选中行要能滚动到视野里：光标超出一屏时窗口必须跟着走。
    #[test]
    fn offset_keeps_the_cursor_visible() {
        // 10 项、只能显示 4 行、光标在第 9 项 → 窗口从第 6 项开始
        let total = 10usize;
        let rows = 4usize;
        let cursor = 9usize;
        let offset = cursor.min(total.saturating_sub(rows));
        assert_eq!(offset, 6);
        assert!(
            cursor >= offset && cursor < offset + rows,
            "光标必须在窗口内"
        );

        // 光标靠前时窗口不偏移
        assert_eq!(2usize.min(total - rows), 2);
    }
}
