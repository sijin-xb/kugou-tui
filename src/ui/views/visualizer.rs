//! 音频可视化页面。
//!
//! 数据来自音频线程：它在把采样透传给播放器的同时记下原始波形，这里对最近
//! [`WINDOW_SIZE`](crate::audio::spectrum::WINDOW_SIZE) 个采样做 FFT，再按**对数**
//! 分成 [`BAND_COUNT`](crate::audio::spectrum::BAND_COUNT) 个频段（见
//! [`crate::audio::spectrum`]）。所以每根柱子是真的对应一段频率：贝斯亮左边，
//! 人声亮中间，镲片亮右边。
//!
//! 拿随机数画动画当然更省事，但那样做出来的是装饰品，和音乐没关系。
//!
//! 顺带一提，早先这里画的是**时域**音量历史（最近半秒的响度），铺成柱子看着
//! 像频谱其实不是——低频高频挤在一起，动起来是一整片此起彼伏的墙。
//!
//! # 观感是怎么来的
//!
//! * **缓动**：原始电平每 ~15ms 跳一次，直接画会抖。这里用「快起慢落」（起振 20ms、
//!   回落 140ms）做指数平滑，柱子才既跟手又不抖。缓动按**真实时间**计算，所以帧率
//!   从 5fps 提到 30fps 时快慢观感不变（见 `AppState::advance_visualizer`）。
//! * **峰值刻度**：柱顶那条线落得比柱子慢，是频谱仪的标志性观感。
//! * 本页没有列表，整块主区都归它（见 `ui::render_main`）。

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::app::state::AppState;
use crate::audio::engine::PlaybackState;
use crate::keymap::key_hint_for;
use crate::ui::theme::Theme;
use crate::ui::widgets::panel;

/// 缓冲动画的一帧序列。用点字符做旋转，比整块文字闪烁克制。
const SPINNER: [char; 8] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧'];

/// 绘制可视化页面。
pub fn render_visualizer(
    frame: &mut Frame,
    area: Rect,
    state: &AppState,
    focused: bool,
    theme: &Theme,
) {
    let block = panel("可视化", focused, theme);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // 太小时只留标题，避免画出半个格子导致的错位
    if inner.height < 4 || inner.width < 8 {
        return;
    }

    // 节奏：频谱占满剩余空间，底部依次是「曲目信息」和「说明」，中间留一行呼吸
    let [bars_area, _gap, info_area] = Layout::vertical([
        Constraint::Min(2),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);

    // 左右各留 2 列内边距，柱子不会贴着边框，观感更稳
    let bars_area = shrink_horizontal(bars_area, 2);

    match state.playback {
        PlaybackState::Loading => render_buffering(frame, bars_area, state, theme),
        PlaybackState::Playing if !state.smooth_spectrum.is_empty() => {
            render_bars(
                frame,
                bars_area,
                &state.smooth_spectrum,
                &state.peak_spectrum,
                theme,
            );
        }
        _ => render_idle(frame, bars_area, state, theme),
    }

    render_track_info(frame, info_area, state, theme);
}

/// 两侧各收缩 `amount` 列（总量不够时不收缩）。
fn shrink_horizontal(area: Rect, amount: u16) -> Rect {
    let shrink = amount.saturating_mul(2);
    if area.width <= shrink {
        return area;
    }
    Rect {
        x: area.x + amount,
        width: area.width - shrink,
        ..area
    }
}

/// 每根柱子占几列：由**可用宽度和频段数**算出来，而不是写死。
///
/// 两条意图，缺一不可：
///
/// * **铺满宽度**。早先固定 2 列（柱体 1 + 空隙 1），柱数被频段数（64）截断后，
///   宽屏下柱子只占左边一小段、右边一大片空着。改成按「可用宽 ÷ 频段数」算，
///   宽屏下柱子自然变宽铺满。
/// * **柱子之间要留空隙**。把频段铺满每一列的话相邻柱子会**粘成一整片**，
///   看不出是一根根柱子。
///
/// 这两条在「可用宽 ÷ 频段数 == 1」时会打架：那个区间里柱子只有 1 列宽，空隙
/// 是 0（粘成一片），而且宽度从 64 到 127 列都有余量没铺满。所以这个区间改走
/// 2 列步长——柱数减半，但既有空隙又铺满。聚合时取的是每组的**最大值**，
/// 鼓点那种尖峰不会被平均掉，减半的代价只是频段分辨率。
fn bar_stride(area_width: usize, bands: usize) -> usize {
    if bands == 0 {
        return 1;
    }
    let fit = area_width / bands;
    if fit >= 2 {
        return fit;
    }
    // fit <= 1：频段数不少于可用列数，柱子只能是 1 列宽。
    // 此时若还有余量（宽度 > 频段数），走 2 列步长换取空隙 + 铺满。
    if area_width > bands { 2 } else { 1 }
}

/// 画柱状频谱：底部对齐，越高越亮，柱顶带一条缓慢下落的峰值刻度。
///
/// 每行只用一个 `Span`：整行共用一个颜色即可（按行做渐变），不必为每个字符建 Span。
/// 一个 80×20 的网格若逐字符建 Span 就是 1600 个，白白拖慢大尺寸终端下的帧。
fn render_bars(frame: &mut Frame, area: Rect, levels: &[f32], peaks: &[f32], theme: &Theme) {
    let height = area.height as usize;
    if height == 0 || levels.is_empty() {
        return;
    }

    // 柱子数由列数决定，但不超过频段数——柱子比频段还多只会是同一根重复画
    let stride = bar_stride(area.width as usize, levels.len());
    let bars = (area.width as usize / stride).min(levels.len());
    if bars == 0 {
        return;
    }
    // 柱体宽度：stride 够就留 1 列空隙，窄到放不下才让柱子贴着
    let body = stride.saturating_sub(1).max(1);

    // 频段数通常多于柱子数，把一段频段压成一根柱子。
    // 取**最大值**而不是平均值：平均会把鼓点那一下的尖峰抹平，柱子就只剩一团钝钝的起伏。
    let aggregate = |values: &[f32]| -> Vec<f32> {
        (0..bars)
            .map(|bar| {
                let start = bar * values.len() / bars;
                let end = ((bar + 1) * values.len() / bars).max(start + 1);
                values[start..end.min(values.len())]
                    .iter()
                    .fold(0.0f32, |max, &value| max.max(value))
                    .clamp(0.0, 1.0)
            })
            .collect()
    };
    let columns = aggregate(levels);
    let caps = aggregate(peaks);

    // 柱子整块居中。
    //
    // `bars = min(宽度 / stride, 频段数)`：当「宽度 / stride」比频段数大时，柱子
    // 只占 `bars * stride` 列，剩下的列全是空的。靠左铺的话右边会空出一大条
    // ——112 列的终端下实测空 15 列，看着像图没画完。居中的话两侧留白对称，
    // 一眼能看出是「画完了、就这么宽」。
    //
    // 不改成「把柱子加宽铺满」是因为那会得到宽度不一的柱子：79 列塞 64 根柱子，
    // 多出来的 15 列只能分给其中一部分，柱体粗细不匀比留白更难看。
    let used = (bars - 1) * stride + body;
    let pad = (area.width as usize).saturating_sub(used) / 2;

    let mut lines = Vec::with_capacity(height);
    for row in 0..height {
        // 从底部数起的行号，用来判断这一格要不要点亮
        let from_bottom = height - row;
        let style = bar_style(row, height, theme);

        let mut text = String::with_capacity(used + pad);
        for _ in 0..pad {
            text.push(' ');
        }
        for (bar, &level) in columns.iter().enumerate() {
            let filled = (level * height as f32).round() as usize;
            let cap = (caps[bar] * height as f32).round() as usize;
            let cell = if filled > 0 && from_bottom <= filled {
                '█'
            } else if cap > 0 && from_bottom == cap {
                '▔'
            } else {
                ' '
            };
            // 柱体横向铺 `body` 列：宽屏下柱子变宽而不是右边留一片空白
            for _ in 0..body {
                text.push(cell);
            }
            // 柱间空隙。最后一根后面不留，否则右边会多出一列空白
            if bar + 1 < bars {
                for _ in body..stride {
                    text.push(' ');
                }
            }
        }
        lines.push(Line::from(Span::styled(text, style)));
    }

    frame.render_widget(ratatui::widgets::Paragraph::new(lines), area);
}

/// 柱子的颜色：底部暗、顶部亮，形成渐变。
///
/// 用主题的语义色而不是硬编码色值，16 色模式下也能正确降级。
fn bar_style(row: usize, height: usize, theme: &Theme) -> Style {
    let ratio = if height <= 1 {
        1.0
    } else {
        row as f32 / (height - 1) as f32
    };

    let base = if ratio > 0.8 {
        theme.accent
    } else if ratio > 0.5 {
        theme.accent_dim
    } else {
        theme.text_dim
    };

    Style::default().fg(base).add_modifier(Modifier::BOLD)
}

/// 未播放：给一句能直接照做的引导，而不是干瘪的「无数据」。
///
/// **停止和暂停必须分开说**。早先这里只判「有没有当前曲目」，于是 `Stopped` 也
/// 显示「已暂停 —— 按 Space 继续」。可停止是「播完了 / 还没开始」，按 Space 是
/// **从头播**；暂停是「停在半路」，按 Space 是**接着播**。混成一句的话，用户
/// 按下 Space 的下一秒就知道界面在骗人。
fn render_idle(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    // 空格键可能在 `[keymap]` 里被改过，所以提示里的键名实时取。
    let play_pause = key_hint_for("Space");
    let text = match state.playback {
        PlaybackState::Paused => &format!("已暂停 —— 按 {play_pause} 接着播"),
        PlaybackState::Stopped if state.current.is_some() => {
            &format!("已停止 —— 按 {play_pause} 从头播")
        }
        PlaybackState::Stopped => "未在播放 —— 到「歌单」或「排行榜」里按 Enter 播一首",
        // 走到这里说明是 Playing 但还没采到频谱（刚起播的那一瞬）。
        // 说「已暂停」或「已停止」都是谎话，那就照实说在采数据
        PlaybackState::Loading | PlaybackState::Playing => "正在播放 —— 频谱数据还没到",
    };
    frame.render_widget(
        ratatui::widgets::Paragraph::new(Line::from(Span::styled(text, theme.dim())))
            .alignment(Alignment::Center),
        area,
    );
}

/// 缓冲中：转点 + 百分比。下载进度已有节流，这里只做显示。
fn render_buffering(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    // 每 3 拍换一帧，约 600ms 转一圈（按默认 tick），不刺眼
    let glyph = SPINNER[(state.ticks / 3) as usize % SPINNER.len()];
    let mut spans = vec![Span::styled(format!("{glyph} 缓冲中"), theme.now_playing())];
    if let Some((received, total)) = state.download_progress
        && let Some(total) = total
        && total > 0
    {
        spans.push(Span::styled(
            format!("  {}%", (received * 100 / total).min(100)),
            theme.dim(),
        ));
    }
    frame.render_widget(
        ratatui::widgets::Paragraph::new(Line::from(spans)).alignment(Alignment::Center),
        area,
    );
}

/// 曲目信息：歌名居中，后面跟当前进度，两者用不同层级区分。
fn render_track_info(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let title = state
        .current
        .as_ref()
        .map(|song| format!("{} - {}", song.singer_text(), song.name))
        .unwrap_or_else(|| "—".to_string());
    let position = format!(
        "  {:02}:{:02}",
        state.position_ms / 60_000,
        (state.position_ms / 1000) % 60
    );

    frame.render_widget(
        ratatui::widgets::Paragraph::new(Line::from(vec![
            Span::styled(title, theme.title()),
            Span::styled(position, theme.dim()),
        ]))
        .alignment(Alignment::Center),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::spectrum::BAND_COUNT;
    use crate::ui::theme::ThemeName;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    /// 把柱子画进一块 Buffer 里，方便逐格断言。
    fn bars_of(width: u16, height: u16, levels: &[f32]) -> Buffer {
        let area = Rect::new(0, 0, width, height);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("测试后端可用");
        let theme = Theme::for_config(ThemeName::Default, false);
        let drawn = terminal
            .draw(|frame| render_bars(frame, area, levels, levels, &theme))
            .expect("绘制成功");
        drawn.buffer.clone()
    }

    /// 用户抱怨的「糊在一起」：柱子之间没有空隙时，相邻柱子一旦都亮着就连成一片。
    /// 这里直接断言每一列是柱子还是空隙。
    #[test]
    fn bars_are_separated_by_a_gap() {
        // 8 个频段、16 列 → 8 根柱子：柱体在偶数列，奇数列是空隙
        let buffer = bars_of(16, 4, &[1.0; 8]);
        let bottom = 3;
        for x in 0..16 {
            let expected = if x % 2 == 0 { "█" } else { " " };
            assert_eq!(
                buffer[(x, bottom)].symbol(),
                expected,
                "第 {x} 列应当是{expected}"
            );
        }
    }

    /// 柱子高度按比例，且底部对齐：能量低的柱子只亮下面几行。
    #[test]
    fn bar_height_follows_the_level_and_sits_on_the_floor() {
        // 满格与半格交替：满的那根四行全亮，半的那根只亮两行
        let levels = [1.0, 0.5, 1.0, 0.5];
        let buffer = bars_of(8, 4, &levels);

        for row in 0..4 {
            // 柱子 0（第 0 列）满格、柱子 1（第 2 列）半格
            assert_eq!(
                buffer[(0, row)].symbol(),
                "█",
                "满格柱子在第 {row} 行也该亮"
            );
            let expected = if row >= 2 { "█" } else { " " };
            assert_eq!(buffer[(2, row)].symbol(), expected, "半格柱子第 {row} 行");
        }
    }

    /// 柱子数不超过频段数：比频段还多的柱子只能是同一根重复画，没有意义。
    ///
    /// 数的是**柱子块数**（靠柱间空隙分隔的连续段），不是填充的列数——柱宽
    /// 会随可用宽度变，按列数断言会在宽屏下假失败。
    #[test]
    fn bar_count_is_capped_by_band_count() {
        // 100 列，只有 4 个频段 → 4 根柱子（每根会被拉宽铺满）
        let buffer = bars_of(100, 2, &[1.0; 4]);
        assert_eq!(count_blocks(&buffer, 100, 1), 4, "柱子数应当等于频段数");
    }

    /// 柱子整块居中，两侧留白对称。
    ///
    /// `bars = min(宽度 / stride, 频段数)`：宽度除不尽时剩下的列全是空的。靠左铺
    /// 的话右边会空出一大条（112 列的终端实测空 15 列），看着像图没画完。
    #[test]
    fn bars_are_centered_when_they_do_not_fill_the_width() {
        let width = 10u16;
        let buffer = bars_of(width, 2, &[1.0; 4]);
        let bottom = 1;

        let lit: Vec<usize> = (0..width as usize)
            .filter(|x| buffer[(*x as u16, bottom)].symbol() == "█")
            .collect();
        assert!(!lit.is_empty(), "应当有柱子被点亮");

        let left = lit[0];
        let right = width as usize - 1 - lit[lit.len() - 1];
        assert!(left > 0, "宽度除不尽时应两侧留白，而不是贴着左边缘");
        assert!(
            left.abs_diff(right) <= 1,
            "两侧留白应当对称：左 {left} 列、右 {right} 列"
        );
    }

    /// **停止和暂停必须分开说**。
    ///
    /// 早先这里只判「有没有当前曲目」，`Stopped` 也显示「已暂停 —— 按 Space 继续」。
    /// 可两者按 Space 的后果不同：停止是**从头播**，暂停是**接着播**。
    /// 混成一句的话，用户按下去的下一秒就知道界面在骗人。
    #[test]
    fn stopped_and_paused_read_differently() {
        fn idle_text(playback: PlaybackState, has_song: bool) -> String {
            let mut state = AppState::new(crate::config::Config::default());
            state.playback = playback;
            state.current = has_song.then(crate::api::model::Song::default);
            let mut terminal = Terminal::new(TestBackend::new(60, 8)).expect("建测试终端");
            let theme = Theme::for_config(ThemeName::Default, false);
            terminal
                .draw(|frame| {
                    render_visualizer(frame, Rect::new(0, 0, 60, 8), &state, false, &theme)
                })
                .expect("渲染可视化");
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>()
                .chars()
                .filter(|character| !character.is_whitespace())
                .collect()
        }

        let stopped = idle_text(PlaybackState::Stopped, true);
        assert!(stopped.contains("已停止"), "停止应说停止：{stopped}");
        assert!(!stopped.contains("已暂停"), "停止不能说成暂停：{stopped}");

        let paused = idle_text(PlaybackState::Paused, true);
        assert!(paused.contains("已暂停"), "暂停应说暂停：{paused}");
        assert!(!paused.contains("已停止"), "暂停不能说成停止：{paused}");

        let nothing = idle_text(PlaybackState::Stopped, false);
        assert!(
            nothing.contains("未在播放"),
            "没有曲目时应引导去播一首：{nothing}"
        );
    }

    /// 「可用宽 ÷ 频段数 == 1」且还有余量时，改走 2 列步长。
    ///
    /// 那个区间里 1 列步长会得到「1 列宽 + 0 空隙」的柱子：既粘成一整片（看不出
    /// 是一根根柱子），又因为宽度从 64 到 127 列都有余量而铺不满。
    #[test]
    fn stride_prefers_gaps_when_one_column_would_not_fill() {
        assert_eq!(bar_stride(100, 64), 2, "有余量时应改走 2 列步长");
        assert_eq!(bar_stride(64, 64), 1, "正好等于频段数时 1 列就铺满");
        assert_eq!(bar_stride(40, 64), 1, "比频段数还少只能贴着放");
        assert_eq!(bar_stride(192, 64), 3, "宽屏下柱子直接变宽");
        assert_eq!(bar_stride(80, 0), 1, "没有频段时不能除零");
    }

    /// 64 个频段在 100 列的终端上要铺满，而不是只占左边 64 列。
    #[test]
    fn sixty_four_bands_fill_a_hundred_columns() {
        let width = 100usize;
        let buffer = bars_of(width as u16, 2, &[1.0; BAND_COUNT]);
        let bottom = 1;

        let lit: Vec<usize> = (0..width)
            .filter(|x| buffer[(*x as u16, bottom)].symbol() == "█")
            .collect();
        assert!(!lit.is_empty(), "应当有柱子被点亮");
        let span = lit[lit.len() - 1] - lit[0] + 1;
        assert!(
            span >= width * 95 / 100,
            "柱子应铺满宽度，实际只占 {span} / {width} 列"
        );
    }

    /// 宽屏下柱子要横向铺满，而不是只占左边一段。
    ///
    /// 这是之前那个 bug：stride 写死 2，柱数被频段数（64）截断后，多出来的
    /// 列全空着，全屏看右边一大片空白。
    #[test]
    fn bars_stretch_to_fill_wide_terminal() {
        let width: usize = 100;
        let buffer = bars_of(width as u16, 2, &[1.0; 4]);
        let filled = (0..width as u16)
            .filter(|&x| buffer[(x, 1)].symbol() == "█")
            .count();
        // 4 根柱子把 100 列几乎占满，只剩柱间空隙
        assert!(
            filled >= width - 8,
            "宽屏下柱子该铺满：100 列里只填了 {filled} 列"
        );
    }

    /// 窄屏放不下空隙时，柱子贴着画（不塌陷、不错位）。
    #[test]
    fn bars_degrade_gracefully_when_narrow() {
        // 4 列画 4 个频段 → stride 1，没有空隙，但仍是 4 根
        let buffer = bars_of(4, 2, &[1.0; 4]);
        let filled = (0..4).filter(|&x| buffer[(x, 1)].symbol() == "█").count();
        assert_eq!(filled, 4, "窄屏也该画满 4 列");
    }

    /// 数一行里被点亮的**连续段**数量。
    fn count_blocks(buffer: &ratatui::buffer::Buffer, width: u16, row: u16) -> usize {
        let mut blocks = 0;
        let mut previous = false;
        for x in 0..width {
            let filled = buffer[(x, row)].symbol() == "█";
            if filled && !previous {
                blocks += 1;
            }
            previous = filled;
        }
        blocks
    }
}
