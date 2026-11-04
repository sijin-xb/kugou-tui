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
//! * **峰值刻度**：柱顶那条线落得比柱子慢，是频谱仪的标志性观感。它画成一条
//!   和柱体不同色的 `▔`，一眼能认出是「近期最高」而不是柱子的一部分。
//! * **亚格分辨率 + 柱内渐变**：柱高按 1/8 格取整（`▁▂▃▄▅▆▇`），柱顶是连续的；
//!   颜色按**柱内**相对高度取，每根柱子读起来是一列，而不是一条横向色带。
//! * 柱区底下有一条暗色**地面线**，柱子有落脚点；空状态（未播放 / 缓冲）整块居中。
//! * 本页没有列表，整块主区都归它（见 `ui::render_main`）。

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

use crate::app::state::{AppState, VisualizerMode};
use crate::audio::engine::PlaybackState;
use crate::keymap::key_hint_for;
use crate::ui::theme::{Theme, mix};
use crate::ui::widgets::panel;

/// 缓冲动画的一帧序列。用点字符做旋转，比整块文字闪烁克制。
const SPINNER: [char; 8] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧'];

/// 柱顶收口用的竖直填充块：1/8 … 8/8。
///
/// 一整格只用 `█` 的话柱高被量化到整行，低矮的柱子会在「空 / 满」之间整行跳变；
/// 用这一组字符，柱顶就能落在一格之内的任意 1/8 处，起伏是连续的（cava / btop
/// 那类频谱的观感）。
const BLOCKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// 每格切成 8 个刻度。`height` 行的柱区就是 `height * 8` 个刻度。
const EIGHTHS: usize = 8;

/// 第 `eighth` 个 1/8 刻度（1..=8）对应的填充块字符。
///
/// 迷你频谱只有一行可用，振幅只能编码进字符本身：`▁` 到 `█` 一列比一列实，
/// 一行也能读出起伏。与柱状图的柱顶收口共用同一组字符，观感一致。
pub(crate) fn sub_block_glyph(eighth: usize) -> char {
    BLOCKS[eighth.clamp(1, BLOCKS.len()) - 1]
}

/// 柱体最多占几列。
///
/// 1 列：这是 cava 的典型观感——柱子细、间隙宽，铺满宽度靠**柱数**而不是
/// 柱宽。封顶到 2 时宽屏下柱子会胖成方砖，几根连在一起就成了一片色块，
/// 看不出是频谱。
const MAX_BODY: usize = 1;

/// 绘制可视化页面。
pub fn render_visualizer(
    frame: &mut Frame,
    area: Rect,
    state: &AppState,
    focused: bool,
    theme: &Theme,
) {
    let title = match state.visualizer_mode {
        VisualizerMode::Bars => "可视化 · 柱状（V 切瀑布 · E 调参数）",
        VisualizerMode::Waterfall => "可视化 · 瀑布（V 切柱状 · E 调参数）",
    };
    let block = panel(title, focused, theme);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // 太小时只留标题，避免画出半个格子导致的错位
    if inner.height < 4 || inner.width < 8 {
        return;
    }

    // 频谱占满剩余空间，底部只留一行曲目信息。
    // 不再画「地面线」：柱子本身就是从底边生长的，多一条横线反而把画面压住。
    let [bars_area, _gap, info_area] = Layout::vertical([
        Constraint::Min(2),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);

    // 左右各留 2 列内边距，柱子不贴边框，观感更稳
    let bars_area = shrink_horizontal(bars_area, 2);

    // 空状态（未播放 / 缓冲）跨过地面线整块居中：只把内容塞进柱区的话，
    // 视觉重心会明显偏高，下面空一大片。
    let content_area = Rect {
        height: info_area.y.saturating_sub(bars_area.y),
        ..bars_area
    };

    match state.playback {
        PlaybackState::Loading => render_buffering(frame, content_area, state, theme),
        PlaybackState::Playing if !state.smooth_spectrum.is_empty() => {
            match state.visualizer_mode {
                VisualizerMode::Bars => render_bars(
                    frame,
                    bars_area,
                    &state.smooth_spectrum,
                    &state.peak_spectrum,
                    theme,
                ),
                VisualizerMode::Waterfall => {
                    render_waterfall(frame, bars_area, &state.spectrum_history, theme)
                }
            }
        }
        _ => render_idle(frame, content_area, state, theme),
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
pub(crate) fn bar_stride(area_width: usize, bands: usize) -> usize {
    if bands == 0 {
        return 1;
    }
    let fit = area_width / bands;
    if fit >= 2 {
        return fit;
    }
    // fit <= 1：频段数不少于可用列数。
    //
    // 这里**永远走 2 列步长**，哪怕宽度装不下 2×柱数——柱子少几根，
    // 但每根都带一格间隙。1 列步长在终端上会让相邻柱子的 `█` 直接相连，
    // 整片读成一团色块，完全看不出是一根根的柱子；少几根柱子换「看得出
    // 是频谱」是值得的。cava 本身在窄终端下也是这个取舍。
    2
}

/// 画柱状频谱：底部对齐，柱内自下而上做渐变，柱顶用半块字符收口，柱顶之上
/// 有一条缓慢下落的峰值刻度。
///
/// 三处和早先不同：
///
/// * **亚格分辨率**。柱高按 1/8 格取整，柱顶落在 `▁▂▃▄▅▆▇` 上，不再整行跳变。
/// * **柱内渐变**。颜色由「这一格在**本柱**里的相对高度」决定，于是每根柱子读起来
///   是一列（底暗顶亮），而不是所有柱子共用同一条横向色带。
/// * **峰值是独立标记**。柱顶之上的峰值画成 `▔` 并用对比色，和柱体的填充块区分开
///   ——不再是和柱子同色的、看着像下划线的浮线。
///
/// 每根柱子每行一个 `Span`（柱体同宽同色，一格 Span 就够）：最坏 `频段数 × 行数`
/// 个，量级很小，不必逐字符建 Span。
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
    // 柱体封顶，步长里多出来的列全给空隙
    let body = stride.saturating_sub(1).clamp(1, MAX_BODY);
    let gap = stride.saturating_sub(body);
    // 柱数比宽度允许的少时，把**余下的列均摊进间隙**，让柱子铺满整行。
    // 不这么做的话，频段数不足时右侧会空一大片（宽度 116、柱 58 时右边
    // 还有一半是空的），看着像图没画完。
    let used_before = bars * stride;
    let spare = (area.width as usize).saturating_sub(used_before);
    let extra_gap = if bars > 1 { spare / (bars - 1) } else { 0 };
    let gap = gap + extra_gap;

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

    // 每根柱子先把刻度算好：柱体高度（1/8 格为单位）和峰值落在第几格。
    let marks: Vec<BarMark> = columns
        .iter()
        .zip(&caps)
        .map(|(&level, &cap)| BarMark::new(level, cap, height))
        .collect();

    // 柱子整块居中。
    //
    // `bars = min(宽度 / stride, 频段数)`：当「宽度 / stride」比频段数大时，柱子
    // 只占 `bars * stride` 列，剩下的列全是空的。靠左铺的话右边会空出一大条
    // ——112 列的终端下实测空 15 列，看着像图没画完。居中的话两侧留白对称，
    // 一眼能看出是「画完了、就这么宽」。
    let used = (bars - 1) * stride + body;
    let pad = (area.width as usize).saturating_sub(used) / 2;

    let mut lines = Vec::with_capacity(height);
    for row in 0..height {
        // 从底部数起的行号，用来判断这一格要不要点亮
        let from_bottom = height - row;

        // 逐格生成 (字符, 样式)，再把**相邻同款**的格子并进同一个 Span。
        //
        // 逐柱各建一个 String 时，一帧要分配 `height × bars` 个字符串
        // （23 行 × 58 柱 ≈ 1300 个），30fps 下每秒四万次。合并之后一行通常
        // 只剩十来个 Span——大部分格子是「空格 + 同一个样式」。
        let mut spans: Vec<Span> = Vec::with_capacity(bars / 4 + 2);
        let mut run: String = String::with_capacity(area.width as usize);
        let mut run_style = Style::default();
        let mut have_run = false;

        let push_cell = |glyph: char,
                         style: Style,
                         run: &mut String,
                         run_style: &mut Style,
                         have_run: &mut bool,
                         spans: &mut Vec<Span>| {
            if *have_run && style == *run_style {
                run.push(glyph);
                return;
            }
            if *have_run && !run.is_empty() {
                spans.push(Span::styled(std::mem::take(run), *run_style));
            }
            run.push(glyph);
            *run_style = style;
            *have_run = true;
        };

        for _ in 0..pad {
            push_cell(
                ' ',
                Style::default(),
                &mut run,
                &mut run_style,
                &mut have_run,
                &mut spans,
            );
        }
        for (bar, mark) in marks.iter().enumerate() {
            let (glyph, style) = bar_cell(from_bottom, mark, theme);
            for _ in 0..body {
                push_cell(
                    glyph,
                    style,
                    &mut run,
                    &mut run_style,
                    &mut have_run,
                    &mut spans,
                );
            }
            // 柱间空隙。最后一根后面不留，否则右边会多出一列空白
            if bar + 1 < bars {
                for _ in 0..gap {
                    push_cell(
                        ' ',
                        Style::default(),
                        &mut run,
                        &mut run_style,
                        &mut have_run,
                        &mut spans,
                    );
                }
            }
        }
        if !run.is_empty() {
            spans.push(Span::styled(run, run_style));
        }
        lines.push(Line::from(spans));
    }

    frame.render_widget(Paragraph::new(lines), area);
}

/// 一根柱子的刻度。
struct BarMark {
    /// 柱体高度，以 1/8 格为单位。
    filled: usize,
    /// 峰值所在的行（从底部数）；0 表示这一根不画峰值（没有、或还没高过柱顶）。
    peak_cell: usize,
}

impl BarMark {
    fn new(level: f32, cap: f32, height: usize) -> Self {
        let scale = (height * EIGHTHS) as f32;
        let filled = (level.clamp(0.0, 1.0) * scale).round() as usize;
        let peak = (cap.clamp(0.0, 1.0) * scale).round() as usize;

        // 峰值只在**高过柱顶一整格**时才画：否则它会和柱顶撞在同一格里，
        // 要么被柱体盖住，要么糊成一团。柱顶追上去时峰值消失、回落时再浮现，
        // 正是频谱仪该有的行为。
        let fill_cell = filled.div_ceil(EIGHTHS);
        let peak_cell = peak.div_ceil(EIGHTHS);
        Self {
            filled,
            peak_cell: if peak_cell > fill_cell { peak_cell } else { 0 },
        }
    }
}

/// 一格该画什么字符、用什么颜色。
fn bar_cell(from_bottom: usize, mark: &BarMark, theme: &Theme) -> (char, Style) {
    let lo = (from_bottom - 1) * EIGHTHS;
    let hi = from_bottom * EIGHTHS;

    if mark.filled >= hi {
        ('█', bar_style(from_bottom, mark.filled, theme))
    } else if mark.filled > lo {
        // 柱顶落在这一格之内：取对应高度的填充块收口
        let partial = mark.filled - lo; // 1..=7
        (
            sub_block_glyph(partial),
            bar_style(from_bottom, mark.filled, theme),
        )
    } else if mark.peak_cell == from_bottom {
        ('▔', peak_style(theme))
    } else {
        (' ', Style::default())
    }
}

/// 柱子的颜色：按「这一格在**本柱**里的相对高度」取色，底部暗、柱顶亮。
///
/// 关键是**柱内**相对高度而不是绝对高度：用绝对高度的话所有柱子在同一行同色，
/// 又变回一整条横向色带。三段渐变（暗槽 → 中调 → 柱顶）给柱子一点纵深。
/// 全部走主题语义色，16 色模式下 [`mix`] 会退化成两端取一，不会拼出假 RGB。
fn bar_style(from_bottom: usize, filled: usize, theme: &Theme) -> Style {
    let rows = (filled as f32 / EIGHTHS as f32).max(1.0);
    let t = (from_bottom as f32 / rows).clamp(0.0, 1.0);

    // 单色纵向渐变：底暗、顶亮，全程只在「主题底槽 → 强调色」之间插值。
    // 早先中段掺了 `accent_dim` 做三段过渡，读起来像彩虹断层；Apple Music
    // 那套观感的关键是**一个色相内的明度变化**，不是多色相。
    //
    // 指数 0.7 让下半段更快地离开底槽色：线性插值时低矮的柱子整根都贴在
    // 深色里，安静段落像没在动。
    let color = mix(theme.progress_bg, theme.accent, t.powf(0.7));
    Style::default().fg(color)
}

/// 频谱瀑布：把最近若干帧的频谱按时间从上到下铺开。
///
/// 每列是一根频段，每行是一帧；新帧从底部长出来，旧的往顶上推。强度用
/// 主题的底槽色到强调色之间的单色渐变表达，安静的格子留空，不画噪点。
fn render_waterfall(
    frame: &mut Frame,
    area: Rect,
    history: &std::collections::VecDeque<Vec<f32>>,
    theme: &Theme,
) {
    let height = area.height as usize;
    let width = area.width as usize;
    if height == 0 || width == 0 || history.is_empty() {
        return;
    }
    let bands = history.front().map_or(0, |frame| frame.len());
    if bands == 0 {
        return;
    }

    // 和柱状图共用一套「可用宽 → 柱数」换算，保证两种模式横向尺度一致。
    let stride = bar_stride(width, bands);
    let bars = (width / stride).min(bands);
    if bars == 0 {
        return;
    }

    // 历史不足一屏时顶部留空，最旧的一帧往下顺延；新帧永远贴底。
    let start = history.len().saturating_sub(height);
    let blank_rows = height - (history.len() - start);

    let mut lines = Vec::with_capacity(height);
    for _ in 0..blank_rows {
        lines.push(Line::from(Span::raw(" ".repeat(width))));
    }
    for levels in history.iter().skip(start) {
        let mut spans: Vec<Span> = Vec::with_capacity(bars / 4 + 2);
        let mut run = String::with_capacity(width);
        let mut run_style = Style::default();
        let mut have_run = false;

        for bar in 0..bars {
            let start = bar * levels.len() / bars;
            let end = ((bar + 1) * levels.len() / bars).max(start + 1);
            let value = levels[start..end.min(levels.len())]
                .iter()
                .fold(0.0f32, |max, &level| max.max(level))
                .clamp(0.0, 1.0);
            let (glyph, style) = waterfall_cell(value, theme);
            for column in 0..stride {
                // 只在柱体的第一列画，其余作间隙；但间隙也要带背景，
                // 否则上一帧的残影会留下来。
                let cell_style = if column == 0 { style } else { Style::default() };
                let cell_glyph = if column == 0 { glyph } else { ' ' };
                if have_run && cell_style == run_style {
                    run.push(cell_glyph);
                    continue;
                }
                if have_run && !run.is_empty() {
                    spans.push(Span::styled(std::mem::take(&mut run), run_style));
                }
                run.push(cell_glyph);
                run_style = cell_style;
                have_run = true;
            }
        }
        if !run.is_empty() {
            spans.push(Span::styled(run, run_style));
        }
        lines.push(Line::from(spans));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

/// 瀑布图一格：强度越高，字符越实、颜色越靠近强调色。
fn waterfall_cell(value: f32, theme: &Theme) -> (char, Style) {
    if value < 0.05 {
        return (' ', Style::default());
    }
    let glyph = if value > 0.75 {
        '█'
    } else if value > 0.45 {
        '▓'
    } else if value > 0.20 {
        '▒'
    } else {
        '░'
    };
    let color = mix(theme.progress_bg, theme.accent, value.powf(0.7));
    (glyph, Style::default().fg(color))
}

/// cava 调参面板：↑↓ 选参数、←→ 调值、Esc/Enter 关闭并保存。
///
/// 面板是只读投影，不复制一份配置——改的就是 `state.config`，
/// 所以关掉时 `save()` 一次即可，实时重建由 `App::ensure_cava` 负责。
pub fn render_cava_panel(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let popup = crate::ui::widgets::centered_rect(area, 54, 11);
    frame.render_widget(Clear, popup);
    let block = panel("cava 参数", true, theme);
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    if inner.height == 0 {
        return;
    }

    let selected = state.cava_setting() as usize;
    let rows: Vec<(String, String)> = vec![
        (
            "平滑强度".to_string(),
            format!("{:.2}", state.config.cava_noise_reduction),
        ),
        (
            "自动增益".to_string(),
            if state.config.cava_autosens {
                "开"
            } else {
                "关"
            }
            .to_string(),
        ),
        (
            "低频下限".to_string(),
            format!("{} Hz", state.config.cava_low_cutoff),
        ),
        (
            "高频上限".to_string(),
            format!("{} Hz", state.config.cava_high_cutoff),
        ),
    ];

    let mut lines = Vec::with_capacity(rows.len() + 2);
    lines.push(Line::from(Span::styled(
        "↑↓ 选择 · ←→ 调整 · Enter/Esc 保存",
        theme.dim(),
    )));
    for (index, (label, value)) in rows.iter().enumerate() {
        let marker = if index == selected { "▸" } else { " " };
        let style = if index == selected {
            theme.selection().add_modifier(Modifier::BOLD)
        } else {
            theme.body()
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{marker} {label:<10}"), style),
            Span::styled(value.clone(), theme.now_playing()),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("参数会在下一帧生效", theme.dim())));
    frame.render_widget(Paragraph::new(lines), inner);
}

/// 峰值刻度：用正文色（中性、亮）配一个居中的 `▔`，和彩色的柱体区分开。
///
/// 不用强调色是因为柱顶本身就是强调色，同色的话那条浮线又会被读成柱子的一部分。
fn peak_style(theme: &Theme) -> Style {
    Style::default().fg(theme.text).add_modifier(Modifier::BOLD)
}

/// 未播放：给一句能直接照做的引导，而不是干瘪的「无数据」。
///
/// **停止和暂停必须分开说**。早先这里只判「有没有当前曲目」，于是 `Stopped` 也
/// 显示「已暂停 —— 按 Space 继续」。可停止是「播完了 / 还没开始」，按 Space 是
/// **从头播**；暂停是「停在半路」，按 Space 是**接着播**。混成一句的话，用户
/// 按下 Space 的下一秒就知道界面在骗人。
///
/// 观感上是一个「空状态」：一个安静的图标 + 主句 + 次级提示，整块居中，而不是
/// 一行孤零零的句子漂在中间。
fn render_idle(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    // 空格键可能在 `[keymap]` 里被改过，所以提示里的键名实时取。
    let play_pause = key_hint_for("Space");
    let (headline, hint) = match state.playback {
        PlaybackState::Paused => ("已暂停".to_string(), format!("按 {play_pause} 接着播")),
        PlaybackState::Stopped if state.current.is_some() => {
            ("已停止".to_string(), format!("按 {play_pause} 从头播"))
        }
        PlaybackState::Stopped => (
            "未在播放".to_string(),
            "到「歌单」或「排行榜」里按 Enter 播一首".to_string(),
        ),
        // 走到这里说明是 Playing 但还没采到频谱（刚起播的那一瞬）。
        // 说「已暂停」或「已停止」都是谎话，那就照实说在采数据
        PlaybackState::Loading | PlaybackState::Playing => {
            ("正在播放".to_string(), "频谱数据还没到".to_string())
        }
    };

    render_centered(
        frame,
        area,
        vec![
            Line::from(Span::styled("♪", Style::default().fg(theme.accent_dim))),
            Line::from(""),
            Line::from(Span::styled(
                headline,
                theme.body().add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(hint, theme.dim())),
        ],
    );
}

/// 缓冲中：转点 + 进度条 + 百分比。下载进度已有节流，这里只做显示。
fn render_buffering(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    // 每 3 拍换一帧，约 600ms 转一圈（按默认 tick），不刺眼
    let glyph = SPINNER[(state.ticks / 3) as usize % SPINNER.len()];

    let mut lines = vec![
        Line::from(Span::styled(glyph.to_string(), theme.now_playing())),
        Line::from(""),
        Line::from(Span::styled(
            "缓冲中",
            theme.body().add_modifier(Modifier::BOLD),
        )),
    ];

    // 只知道收了多少、不知道总量时不给百分比：宁可不画，也不给个假的分母
    if let Some((received, Some(total))) = state.download_progress
        && total > 0
    {
        let percent = (received * 100 / total).min(100);
        lines.push(Line::from(progress_spans(percent, theme)));
    }

    render_centered(frame, area, lines);
}

/// 缓冲进度条：填充段用强调色、空槽用暗色，末尾跟百分比。
fn progress_spans(percent: u64, theme: &Theme) -> Vec<Span<'static>> {
    const WIDTH: usize = 24;
    let filled = ((percent.min(100) as usize) * WIDTH / 100).min(WIDTH);

    vec![
        Span::styled("━".repeat(filled), theme.now_playing()),
        Span::styled(
            "─".repeat(WIDTH - filled),
            Style::default().fg(theme.border),
        ),
        Span::styled(format!("  {percent}%"), theme.dim()),
    ]
}

/// 曲目信息：歌名最重（强调色加粗），歌手与进度退到次要层级。
///
/// 早先是一整行同色的「歌手 - 歌名  时间」，没有主次，扫一眼分不出哪个是歌名。
fn render_track_info(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let mut spans: Vec<Span> = Vec::new();
    match &state.current {
        Some(song) => {
            spans.push(Span::styled(song.name.clone(), theme.now_playing()));
            spans.push(Span::styled("  ·  ", theme.dim()));
            spans.push(Span::styled(song.singer_text(), theme.dim()));
        }
        None => spans.push(Span::styled("—", theme.dim())),
    }
    spans.push(Span::styled("    ", theme.dim()));
    spans.push(Span::styled(
        format!(
            "{:02}:{:02}",
            state.position_ms / 60_000,
            (state.position_ms / 1000) % 60
        ),
        theme.dim(),
    ));

    frame.render_widget(
        Paragraph::new(Line::from(spans)).alignment(Alignment::Center),
        area,
    );
}

/// 把若干行整块水平居中 + 垂直居中地放进 `area`。
fn render_centered(frame: &mut Frame, area: Rect, lines: Vec<Line<'static>>) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let height = (lines.len() as u16).min(area.height);
    let target = if area.height > height {
        Rect {
            y: area.y + (area.height - height) / 2,
            height,
            ..area
        }
    } else {
        area
    };
    frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), target);
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

    /// 频段数不少于可用列数时，**一律走 2 列步长**换取间隙。
    ///
    /// 1 列步长会得到「1 列宽 + 0 空隙」的柱子，相邻柱子的 `█` 直接相连，
    /// 整片读成一团色块。宁可在窄终端里少画几根柱子，也要保住「一根根」的观感。
    #[test]
    fn stride_always_leaves_a_gap_when_bands_outnumber_columns() {
        assert_eq!(bar_stride(100, 64), 2, "有余量时应改走 2 列步长");
        assert_eq!(bar_stride(64, 64), 2, "正好等于频段数时也要留空隙");
        assert_eq!(bar_stride(40, 64), 2, "比频段数还少同样留空隙");
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

    /// 宽屏下柱子要横向铺开，而不是只占左边一段。
    ///
    /// 这是之前那个 bug：stride 写死 2，柱数被频段数（64）截断后，多出来的
    /// 列全空着，全屏看右边一大片空白。
    ///
    /// 现在柱体宽度封顶（[`MAX_BODY`]），宽屏下多出来的步长让给**空隙**，不再把
    /// 柱子拉成方砖——所以这里量的是「首尾柱子之间的跨度」和「单根柱体的最大宽度」，
    /// 而不是填充列数。
    #[test]
    fn bars_stretch_to_fill_wide_terminal() {
        let width: usize = 100;
        let buffer = bars_of(width as u16, 2, &[1.0; 4]);
        let lit: Vec<usize> = (0..width)
            .filter(|&x| buffer[(x as u16, 1)].symbol() == "█")
            .collect();
        assert!(!lit.is_empty(), "应当有柱子被点亮");

        // 单根柱体不超过上限
        let mut run = 1usize;
        let mut widest = 1usize;
        for pair in lit.windows(2) {
            if pair[1] == pair[0] + 1 {
                run += 1;
                widest = widest.max(run);
            } else {
                run = 1;
            }
        }
        assert!(
            widest <= MAX_BODY,
            "柱体被拉宽到 {widest} 列，超过了上限 {MAX_BODY}"
        );

        // 首尾仍横跨大部分宽度
        let span = lit[lit.len() - 1] - lit[0] + 1;
        assert!(span >= width / 2, "柱子只横跨 {span} / {width} 列");
    }

    /// 亚格分辨率：柱顶落在一格之内时用半块字符收口，不再整行跳变。
    #[test]
    fn bar_top_uses_sub_cell_blocks() {
        // 高度 3 行 = 24 个 1/8 刻度。0.5 → 12 刻度 → 底部一整行、第二行半格。
        let buffer = bars_of(2, 3, &[0.5]);
        assert_eq!(buffer[(0, 2)].symbol(), "█", "最底一行应满格");
        assert_eq!(buffer[(0, 1)].symbol(), "▄", "第二行应是半格");
        assert_eq!(buffer[(0, 0)].symbol(), " ", "顶行应是空的");
    }

    /// 峰值明显高于柱体时，在它所在的行画一个独立标记，而不是把柱体加高。
    #[test]
    fn peak_above_the_bar_draws_a_marker() {
        let area = Rect::new(0, 0, 2, 4);
        let mut terminal = Terminal::new(TestBackend::new(2, 4)).expect("测试后端可用");
        let theme = Theme::for_config(ThemeName::Default, false);
        // 高度 4 行 = 32 刻度。柱体 0.25 → 8 刻度（底行满）；峰值 0.9 → 29 刻度（顶行）。
        let levels = [0.25f32];
        let peaks = [0.9f32];
        let drawn = terminal
            .draw(|frame| render_bars(frame, area, &levels, &peaks, &theme))
            .expect("绘制成功");
        let buffer = &drawn.buffer;

        assert_eq!(buffer[(0, 3)].symbol(), "█", "柱体应在底行");
        let marker = (0..4).find(|&y| buffer[(0, y)].symbol() == "▔");
        let marker = marker.expect("应当画出峰值标记");
        assert!(marker < 3, "峰值标记应在柱体之上，实际在第 {marker} 行");
    }

    /// 窄屏下柱子减少但**保留间隙**，不会塌陷或错位。
    #[test]
    fn bars_degrade_gracefully_when_narrow() {
        // 4 列画 4 个频段 → stride 2，只能画 2 根（各占 1 列，间隔 1 列）。
        // 少画两根柱子换「看得出是一根根」，这是有意的取舍。
        let buffer = bars_of(4, 2, &[1.0; 4]);
        let filled = (0..4).filter(|&x| buffer[(x, 1)].symbol() == "█").count();
        assert_eq!(filled, 2, "窄屏下柱子减半，但每根之间留空隙");
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
