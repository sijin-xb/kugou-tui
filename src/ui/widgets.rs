//! 可复用的渲染原语。
//!
//! 这些函数只依赖 [`AppState`] 的片段或纯数据，不感知「当前在哪个标签页」，
//! 因此各个视图可以自由组合它们。

use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Padding;
use ratatui::widgets::{Block, BorderType, Borders, HighlightSpacing, List, ListItem, Paragraph};

use crate::api::model::Song;
use crate::audio::engine::PlaybackState;
use crate::ui::theme::Theme;

/// 判断**已经渲染好**的二维码行能否完整塞进 `max_width × max_rows`；装得下就返回一份。
///
/// # 为什么收「已渲染的行」而不是「内容」
///
/// 二维码在收到时就按当前 `aspect` 渲染好并存进 `login.qr` 了（见 `Loaded::LoginQr`），
/// 这里再编码一遍纯属浪费——实测单次编码约 0.9ms，而登录弹窗每帧都会走到这里。
///
/// # 为什么装不下返回 `None`
///
/// 二维码的行数由内容长度决定、不能压缩：半块字符一个字符承载上下两个模块，
/// H 个模块恒定占 `ceil(H/2)` 行、W 列，缩放等于毁掉可扫性。装不下时返回 `None`，
/// 让界面改成给一句能照做的提示——比画一个被裁掉、扫不出来的二维码有用。
pub fn qr_lines_fitted(lines: &[String], max_width: usize, max_rows: usize) -> Option<Vec<String>> {
    let width = lines.first().map(|line| line.chars().count()).unwrap_or(0);
    if width == 0 || width > max_width || lines.len() > max_rows {
        return None;
    }
    Some(lines.to_vec())
}

/// 渲染二维码**所需**的行列数（用来在装不下时告诉用户差多少）。
pub fn qr_needed_size(content: &str, aspect: f32) -> Option<(usize, usize)> {
    let modules = qr_modules(content)?;
    let width = modules.first().map(Vec::len).unwrap_or(0);
    let height = modules.len();
    if width == 0 || height == 0 {
        return None;
    }
    let rows = if aspect < 1.5 {
        height
    } else {
        height.div_ceil(2)
    };
    Some((width, rows))
}

/// 编码成「带静默区的模块矩阵」。
fn qr_modules(content: &str) -> Option<Vec<Vec<bool>>> {
    /// 静默区（二维码外围留白）的模块数。
    ///
    /// 规范要求 4 个模块，但终端里每多一圈就多占一列和半行。这里取 1——
    /// 配合固定的纯白底，留白本身就是静默区。取 2 时实测多占 2 列 1 行，
    /// 而扫码距离与成功率没有可感差别。
    const QUIET: usize = 1;

    // 纠错级别刻意用 **L**（约 7%）而不是默认的 M（约 15%）。
    //
    // 这是尺寸的唯一杠杆：二维码的模块数由「内容长度 + 纠错级别」决定，
    // 内容（服务端下发的扫码地址）我们改不了，所以想变小只能降纠错。
    // 屏幕上的二维码是理想条件——像素精确、无污损、无眩光，L 足够；
    // 而汽水的扫码地址明显比酷狗长，不降的话在窄终端上根本放不下。
    let code =
        qrcode::QrCode::with_error_correction_level(content.as_bytes(), qrcode::EcLevel::L).ok()?;
    let image = code.render::<char>().quiet_zone(false).build();

    let core: Vec<Vec<bool>> = image
        .lines()
        .map(|line| line.chars().map(|character| character != ' ').collect())
        .collect();
    let core_width = core.first().map(Vec::len).unwrap_or(0);
    if core_width == 0 {
        return None;
    }

    let width = core_width + QUIET * 2;
    let mut modules: Vec<Vec<bool>> = Vec::with_capacity(core.len() + QUIET * 2);
    for _ in 0..QUIET {
        modules.push(vec![false; width]);
    }
    for row in &core {
        let mut padded = vec![false; width];
        padded[QUIET..QUIET + core_width].copy_from_slice(row);
        modules.push(padded);
    }
    for _ in 0..QUIET {
        modules.push(vec![false; width]);
    }
    Some(modules)
}

/// 把二维码画成终端字符行（**不设上限**，供已知画得下的场景使用）。
///
/// \`aspect\` 是终端「字符高:宽」比：标准终端是 2:1，用半块字符（一个字符
/// 承载上下两行模块）正好正方形；低于 1.5 时（宽字符字体）改用一字符一行的
/// 全块字符，避免被横向拉长。
pub fn qr_lines(content: &str, aspect: f32) -> Option<Vec<String>> {
    let modules = qr_modules(content)?;
    if aspect < 1.5 {
        return Some(render_full_blocks(&modules));
    }
    Some(render_half_blocks(&modules))
}

/// 每个模块占 1 个字符、1 行。用全块字符（\`█\`/\` \`）——视觉上每个模块是「高:宽 = 1:aspect」。
fn render_full_blocks(modules: &[Vec<bool>]) -> Vec<String> {
    modules
        .iter()
        .map(|row| {
            let mut line = String::with_capacity(row.len());
            for dark in row {
                line.push(if *dark { '█' } else { ' ' });
            }
            line
        })
        .collect()
}

/// 每个模块占 1 个字符、½ 行。用半块字符（▀▄█/空格）——视觉上每个模块
/// 是「高:宽 = (aspect/2):1」，aspect=2.0 时正好正方形。
fn render_half_blocks(modules: &[Vec<bool>]) -> Vec<String> {
    let mut lines = Vec::with_capacity(modules.len().div_ceil(2));
    let mut row = 0;
    while row < modules.len() {
        let top = &modules[row];
        let bottom = modules.get(row + 1);

        let mut line = String::with_capacity(top.len());
        for (column, top_dark) in top.iter().enumerate() {
            let bottom_dark = bottom.is_some_and(|row| row[column]);
            line.push(match (*top_dark, bottom_dark) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            });
        }
        lines.push(line);
        row += 2;
    }
    lines
}

/// 构造带**统一选中表现**的列表。
///
/// # 为什么不能只靠背景色
///
/// 早期版本只用 `highlight_style` 的背景色表示选中。在深色终端上
/// `#264260` 这种暗蓝与背景对比极低，用户移动光标时**看不出任何变化**，
/// 会误以为程序卡死或按键无效。所以这里固定加一个 `> ` 文字标记：
/// 文字在任何终端、任何配色下都看得见。
///
/// `HighlightSpacing::Always` 给所有行预留同样宽度的标记位，选中行与其它行
/// 才不会错位。
pub fn selection_list<'a>(items: Vec<ListItem<'a>>, theme: &Theme) -> List<'a> {
    List::new(items)
        .highlight_symbol("> ")
        .highlight_style(theme.selection())
        .highlight_spacing(HighlightSpacing::Always)
        // 上下各留 2 行上下文，光标移动时视线不用重新找位置
        .scroll_padding(2)
}

/// 列表行是否落在「可能出现在屏幕上」的窗口内。
///
/// [`List`] 必须收到与真实列表**等长**的 items（它靠 `items.len()` 维护滚动
/// 偏移、并按 offset 迭代），所以不能只把可见那几行传进去。但真正昂贵的只有
/// [`song_row`] / [`entry_row`] 里的字符串格式化与显示宽度扫描；窗口外的行用
/// 等高的空占位顶上去即可，滚动、高亮、滚动条的行为分毫不动。上千首的歌单或
/// 队列下，这一层能把每帧的构造量从 O(整表) 降到 O(可见)。
///
/// 窗口取「当前偏移」与「选中项」两侧各一屏多：按 End 或鼠标跳转时 ratatui 会把
/// 选中项滚进视野，那一屏也得是真行，否则会闪一帧空白。
pub fn row_is_visible(
    index: usize,
    offset: usize,
    selected: Option<usize>,
    visible_rows: usize,
) -> bool {
    let focus = selected.unwrap_or(offset);
    let margin = visible_rows.saturating_add(1);
    let low = offset.min(focus).saturating_sub(margin);
    let high = offset.max(focus).saturating_add(margin);
    index >= low && index <= high
}

/// 统一的带边框面板。
///
/// `focused` 决定边框亮度——这是界面里唯一表示「键盘焦点在哪」的视觉线索，
/// 比给每个面板加标题后缀更省空间。
pub fn panel(title: impl Into<Line<'static>>, focused: bool, theme: &Theme) -> Block<'static> {
    let border_style = if focused {
        theme.focused_border()
    } else {
        theme.idle_border()
    };

    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border_style)
        .title(title.into())
        .title_style(if focused { theme.title() } else { theme.dim() })
        // 左右各留一列：内容贴着边框会显得很挤（rmpc 全项目都这么做）
        .padding(Padding::horizontal(1))
}

/// 在 `area` 内居中放置一个固定尺寸的矩形，用于弹窗。
///
/// 尺寸会被钳到 `area` 之内，避免小终端下算出越界矩形。
pub fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

/// 单行提示，居中显示。
pub fn placeholder(text: &str, theme: &Theme) -> Paragraph<'static> {
    Paragraph::new(Line::from(Span::styled(text.to_string(), theme.dim())))
}

/// 一行的渲染上下文。
#[derive(Debug, Clone, Copy)]
pub struct RowContext {
    /// 可用列宽，用于计算各列宽度。
    pub width: usize,
    /// 是否为正在播放的曲目。
    pub is_current: bool,
    /// 当前播放状态。
    pub playback: PlaybackState,
    /// 鼠标是否悬停在这一行。
    ///
    /// 终端不是网页，没有 CSS `:hover`；这里是靠 crossterm 的鼠标移动事件 +
    /// 命中测试算出来的。终端没开鼠标捕获时永远是 false，属于无害降级。
    pub hover: bool,
}

/// 把一首歌渲染成列表行。
///
/// 列宽按可用宽度自适应：歌名占大头，歌手与专辑依次让位，时长固定靠右。
/// 所有文本都按**显示宽度**截断（CJK 记 2 列），因此中英文混排不会串列。
pub fn song_row(
    number: usize,
    song: &Song,
    context: RowContext,
    theme: &Theme,
) -> ListItem<'static> {
    // 序号 4 列 + 时长 6 列 + 4 个空格分隔
    const NUMBER_WIDTH: usize = 4;
    const DURATION_WIDTH: usize = 6;
    const GAPS: usize = 4;

    let flexible = context
        .width
        .saturating_sub(NUMBER_WIDTH + DURATION_WIDTH + GAPS);
    let name_width = (flexible * 45 / 100).max(8);
    let singer_width = (flexible * 25 / 100).max(6);
    let album_width = flexible.saturating_sub(name_width + singer_width + 2);

    // ASCII 标记，避免字体缺字。
    //
    // 播放中刻意**不用** `>`：列表的选中标记已经占用了 `> `，两个 `>` 并排
    // （形如 `> >  1 歌名`）很容易被看成一个符号，分不清哪条是选中、哪条在播。
    let marker = if context.is_current {
        match context.playback {
            PlaybackState::Playing => "*",
            PlaybackState::Paused => "=",
            PlaybackState::Loading => "~",
            PlaybackState::Stopped => " ",
        }
    } else {
        " "
    };

    let number_style = if context.is_current {
        theme.now_playing()
    } else {
        theme.dim()
    };

    let name_style = if context.is_current {
        theme.now_playing()
    } else if context.hover {
        // 悬停：加粗一下就够，不用换背景——终端里大面积反色很刺眼
        theme.body().add_modifier(ratatui::style::Modifier::BOLD)
    } else {
        theme.body()
    };

    let line = Line::from(vec![
        // 序号从 1 开始：用户看的是「第几首」，不是数组下标
        Span::styled(format!("{marker}{:>3} ", number + 1), number_style),
        Span::styled(
            format!("{} ", truncate_to_width(&song.name, name_width)),
            name_style,
        ),
        Span::styled(
            format!("{} ", truncate_to_width(&song.singer_text(), singer_width)),
            theme.dim(),
        ),
        Span::styled(
            format!(
                "{} ",
                truncate_to_width(display_album(song), album_width.max(1))
            ),
            theme.dim(),
        ),
        Span::styled(song.duration_text(), theme.dim()),
    ]);

    ListItem::new(line)
}

/// 专辑名缺失时给个占位，避免列塌陷。
fn display_album(song: &Song) -> &str {
    if song.album_name.is_empty() {
        "—"
    } else {
        &song.album_name
    }
}

/// 两段式列表行：左侧主标题 + 右侧副标题。
pub fn entry_row(
    index: usize,
    title: &str,
    subtitle: &str,
    marker: Option<&str>,
    width: usize,
    theme: &Theme,
) -> ListItem<'static> {
    let prefix = marker.unwrap_or(" ");
    let index_text = format!("{prefix}{:>3} ", index + 1);
    // 分隔空格并入副标题，这样它不会被后面的截断逻辑单独吃掉
    let subtitle_text = if subtitle.is_empty() {
        String::new()
    } else {
        format!(" {subtitle}")
    };

    // 一律按**显示宽度**计算。用 `str::len()`（字节数）会让 CJK 副标题
    // 把可用宽度估得过小，歌名被多截一截，末尾的分隔符还会被挤掉。
    let reserved = display_width(&index_text) + display_width(&subtitle_text);
    let available = width.saturating_sub(reserved).max(6);

    let line = Line::from(vec![
        Span::styled(index_text, theme.dim()),
        Span::styled(truncate_to_width(title, available), theme.body()),
        Span::styled(subtitle_text, theme.dim()),
    ]);

    ListItem::new(line)
}

/// 显示宽度：CJK 与全角字符按 2 列计算。
///
/// 没有引入 `unicode-width`：歌词和歌名里出现的字符绝大多数落在下面这些区间，
/// 误差只影响个别生僻符号的对齐，不值得多一条依赖。
pub fn char_width(character: char) -> usize {
    match character as u32 {
        0x1100..=0x115F      // 谚文字母
        | 0x2E80..=0x303E    // CJK 部首、假名标点
        | 0x3041..=0x33FF    // 假名、注音、CJK 兼容
        | 0x3400..=0x4DBF    // CJK 扩展 A
        | 0x4E00..=0x9FFF    // CJK 基本区
        | 0xA000..=0xA4CF    // 彝文
        | 0xAC00..=0xD7A3    // 谚文音节
        | 0xF900..=0xFAFF    // CJK 兼容表意
        | 0xFE30..=0xFE6F    // CJK 兼容形式
        | 0xFF00..=0xFF60    // 全角形式
        | 0xFFE0..=0xFFE6
        | 0x1F300..=0x1F64F  // 常用 emoji
        | 0x1F900..=0x1F9FF
        | 0x20000..=0x3FFFD => 2,
        _ => 1,
    }
}

pub fn display_width(text: &str) -> usize {
    text.chars().map(char_width).sum()
}

/// 按终端显示宽度截断，超出部分用 `…` 收尾。
pub fn truncate_to_width(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if display_width(text) <= max_width {
        return text.to_string();
    }

    // 留一列给省略号
    let budget = max_width.saturating_sub(1);
    let mut output = String::new();
    let mut width = 0usize;

    for character in text.chars() {
        let character_width = char_width(character);
        if width + character_width > budget {
            break;
        }
        output.push(character);
        width += character_width;
    }

    output.push('…');
    output
}

/// 把字节数格式化成人类可读的形式。
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0usize;

    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }

    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measures_cjk_as_double_width() {
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width("海阔天空"), 8);
        assert_eq!(display_width("a海b"), 4);
    }

    #[test]
    fn truncates_within_budget() {
        assert_eq!(truncate_to_width("abcdef", 10), "abcdef");
        assert_eq!(truncate_to_width("abcdef", 4), "abc…");
        assert_eq!(truncate_to_width("海阔天空", 4), "海…");
        assert_eq!(truncate_to_width("海阔天空", 5), "海阔…");
    }

    #[test]
    fn truncation_of_zero_width_is_empty() {
        assert_eq!(truncate_to_width("abc", 0), "");
    }

    #[test]
    fn formats_bytes() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KiB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0 MiB");
    }

    #[test]
    fn centered_rect_is_clamped_to_area() {
        let area = Rect::new(0, 0, 40, 10);
        let popup = centered_rect(area, 100, 100);
        assert_eq!(popup.width, 40);
        assert_eq!(popup.height, 10);
        assert_eq!(popup.x, 0);
        assert_eq!(popup.y, 0);
    }

    #[test]
    fn centered_rect_centers_smaller_popup() {
        let area = Rect::new(0, 0, 40, 20);
        let popup = centered_rect(area, 20, 10);
        assert_eq!(popup.x, 10);
        assert_eq!(popup.y, 5);
        assert_eq!(popup.width, 20);
        assert_eq!(popup.height, 10);
    }

    // ---- 只构造可见窗口的行 ----

    #[test]
    fn row_window_covers_the_offset_plus_a_screenful() {
        // 没有选中项时以偏移为焦点：一屏 10 行，上下各多留一屏多。
        assert!(row_is_visible(0, 0, None, 10));
        assert!(row_is_visible(11, 0, None, 10));
        assert!(!row_is_visible(12, 0, None, 10));
    }

    #[test]
    fn row_window_follows_the_selected_item_off_screen() {
        // 选中项被滚到视野外时（例如按 End），它那一屏也得是真行，
        // 否则 ratatui 把选中项滚进来时会闪一帧空白。
        assert!(row_is_visible(100, 90, Some(100), 10));
        assert!(row_is_visible(85, 90, Some(100), 10));
        assert!(!row_is_visible(78, 90, Some(100), 10));
        assert!(!row_is_visible(112, 90, Some(100), 10));
    }

    #[test]
    fn row_window_survives_empty_and_tiny_lists() {
        // 空列表（visible_rows 为 0）也不能越界或反向。
        assert!(row_is_visible(0, 0, None, 0));
        assert!(!row_is_visible(2, 0, None, 0));
    }

    // ---- 二维码按可用区域自适应 ----

    /// 汽水的扫码地址明显比酷狗长，编出来的二维码也更大——这正是
    /// 「二维码太大撑满屏幕」的来源。这里用它当样本。
    const SODA_SCAN_URL: &str = "https://bff-pc.qishui.com/ucenter_web/app/sdk-next?aid=386088&token=0123456789abcdef0123456789abcdef&uc_sdk=scan-auth";

    #[test]
    fn fitted_qr_renders_when_there_is_enough_room() {
        let (width, rows) = qr_needed_size(SODA_SCAN_URL, 2.0).expect("能编码");
        let rendered = qr_lines(SODA_SCAN_URL, 2.0).expect("能编码");
        // 给足空间就该原样返回，行数要与「所需尺寸」一致
        let lines = qr_lines_fitted(&rendered, width, rows).expect("空间够时应渲染");
        assert_eq!(lines.len(), rows, "渲染行数应与所需行数一致");
        assert!(lines.iter().all(|line| line.chars().count() == width));
    }

    /// 空间不够时**返回 None**，而不是画一个被裁掉的码——裁掉的码扫不出来，
    /// 比一句「放不下」的提示更糟。
    #[test]
    fn fitted_qr_refuses_when_the_area_is_too_small() {
        let (width, rows) = qr_needed_size(SODA_SCAN_URL, 2.0).unwrap();
        let rendered = qr_lines(SODA_SCAN_URL, 2.0).unwrap();
        // 少一行就不画
        assert!(qr_lines_fitted(&rendered, width, rows - 1).is_none());
        // 少一列也不画
        assert!(qr_lines_fitted(&rendered, width - 1, rows).is_none());
        // 还没收到二维码（空输入）也不能当作「装得下」
        assert!(qr_lines_fitted(&[], width, rows).is_none());
    }

    /// 所需尺寸要与实际渲染对得上——否则提示里的「需要多少行」会误导用户。
    #[test]
    fn needed_size_matches_what_gets_rendered() {
        for aspect in [1.0_f32, 2.0] {
            let (width, rows) = qr_needed_size("https://example.com/short", aspect).unwrap();
            let rendered = qr_lines("https://example.com/short", aspect).unwrap();
            let lines = qr_lines_fitted(&rendered, width, rows).unwrap();
            assert_eq!(lines.len(), rows, "aspect={aspect}");
            assert_eq!(lines[0].chars().count(), width, "aspect={aspect}");
        }
    }

    /// 宽字符字体（aspect < 1.5）用全块、一行一个模块，所以行数更多。
    #[test]
    fn narrow_aspect_uses_full_blocks_and_more_rows() {
        let (_, wide_rows) = qr_needed_size("https://example.com/x", 2.0).unwrap();
        let (_, narrow_rows) = qr_needed_size("https://example.com/x", 1.0).unwrap();
        assert!(
            narrow_rows > wide_rows,
            "宽字符字体一行只放一个模块，行数应更多：{narrow_rows} vs {wide_rows}"
        );
    }

    /// 编不出来的内容不能 panic。
    #[test]
    fn unencodable_content_is_none() {
        // 极长内容超过二维码容量上限
        let huge = "x".repeat(10_000);
        assert!(qr_needed_size(&huge, 2.0).is_none());
        assert!(qr_lines(&huge, 2.0).is_none());
    }

    // ---- 尺寸回归 ----

    /// 二维码的模块数只由「内容长度 + 纠错级别」决定，所以尺寸是**内容决定的**，
    /// 不是画法能优化的。这条把实测值钉住，防止哪天纠错级别或静默区被无意改大。
    ///
    /// 汽水的扫码地址接近 200 字符（酷狗只有 40 上下），这也是「汽水二维码
    /// 特别大」的根因：它本来就该更大，不是渲染出了问题。
    #[test]
    fn qr_size_is_driven_by_content_length() {
        let kugou = "https://m.kugou.com/qr?key=abc123def456";
        let soda = "https://bff-pc.qishui.com/ucenter_web/app/sdk-next?aid=386088&device_id=2204957404565290&token=0123456789abcdef0123456789abcdef&next=https%3A%2F%2Fapi.qishui.com&uc_sdk=scan-auth&is_new_login=1";

        let (kugou_w, kugou_rows) = qr_needed_size(kugou, 2.0).unwrap();
        let (soda_w, soda_rows) = qr_needed_size(soda, 2.0).unwrap();

        // 短地址落在 31x16 左右；长地址落在 51x26 左右
        assert!((30..=33).contains(&kugou_w), "酷狗宽度异常：{kugou_w}");
        assert!(
            (15..=17).contains(&kugou_rows),
            "酷狗高度异常：{kugou_rows}"
        );
        assert!((50..=53).contains(&soda_w), "汽水宽度异常：{soda_w}");
        assert!((25..=28).contains(&soda_rows), "汽水高度异常：{soda_rows}");

        // 内容越长尺寸越大：这是二维码的固有性质，不是 bug
        assert!(soda_w > kugou_w && soda_rows > kugou_rows);
    }

    /// 纠错级别用 L 而不是默认的 M——这是尺寸的**唯一**杠杆（内容改不了）。
    /// 换回 M 会让长地址明显变大，这条挡住那次回归。
    #[test]
    fn low_error_correction_keeps_the_qr_small() {
        let soda = "https://bff-pc.qishui.com/ucenter_web/app/sdk-next?aid=386088&device_id=2204957404565290&token=0123456789abcdef0123456789abcdef&next=https%3A%2F%2Fapi.qishui.com&uc_sdk=scan-auth&is_new_login=1";
        let (width, _) = qr_needed_size(soda, 2.0).unwrap();
        assert!(
            width <= 53,
            "长地址的宽度应被 L 级别压在 53 以内，实际 {width}（是否被改回 M 了？）"
        );
    }

    // ---- 基准（默认不跑：`cargo test -- --ignored --nocapture`）----

    /// 量化「每帧重新编码二维码」的代价，用来决定要不要缓存渲染结果。
    ///
    /// 阈值 300µs：低于它就不值得为缓存引入状态字段与失效逻辑——缓存漏失效
    /// 会画出扫不出来的旧码，比省下的几百微秒贵得多。
    #[test]
    #[ignore = "基准测试，靠 --ignored 手动跑"]
    fn bench_qr_encoding_cost() {
        const ITERATIONS: u32 = 200;
        // 热身一次，把首次的惰性分配排除在外。
        let _ = qr_lines(SODA_SCAN_URL, 2.0).expect("能编码");

        let start = std::time::Instant::now();
        for _ in 0..ITERATIONS {
            let lines = qr_lines(SODA_SCAN_URL, 2.0).expect("能编码");
            std::hint::black_box(&lines);
        }
        let per_call = start.elapsed() / ITERATIONS;
        println!("二维码编码（汽水地址）：{per_call:?}/次（{ITERATIONS} 次平均）");
    }
}
