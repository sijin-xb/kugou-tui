//! 播放条、歌词面板、播放队列。
//!
//! 三者共同回答用户最关心的三个问题：**在放什么**（播放条）、**唱到哪了**
//! （歌词）、**接下来放什么**（队列）。

use image::imageops::FilterType;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Gauge, ListItem, ListState, Paragraph};
use ratatui_image::picker::ProtocolType;
use ratatui_image::{FontSize, Resize, StatefulImage};

use crate::api::model::format_duration_ms;
use crate::app::queue::PlayQueue;
use crate::app::state::{AppState, HitTarget};
use crate::audio::engine::PlaybackState;
use crate::config::CoverFill;
use crate::keymap::key_hint_for;
use crate::ui::theme::{Theme, mix};
use crate::ui::views::{empty_placeholder, failed_placeholder, loading_placeholder};
use crate::ui::widgets::{
    RowContext, display_width, panel, row_is_visible, selection_list, song_row, truncate_to_width,
};

/// 播放条高度：2 行内容 + 上下边框。
///
/// 2 行 = 曲目信息 / 进度条。
///
/// 这里**刻意不放封面**：播放条总共才 4 行，给封面最多 8×4 格——那个尺寸下
/// 专辑图只是一团糊色，既看不清又白占宽度。封面挪到「首页」和「歌词」页，
/// 那里有整块区域可以按真实比例放大（见 `draw_cover_block`）。
pub const PLAYER_HEIGHT: u16 = 4;

/// 播放条：封面 + 曲目信息 + 进度条 + 下一首。
pub fn render_player(frame: &mut Frame, area: Rect, state: &mut AppState, theme: &Theme) {
    if area.height < 3 || area.width < 20 {
        return;
    }

    let block = panel("播放", false, theme);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height == 0 || inner.width < 10 {
        return;
    }

    // 两行：曲目信息 / 进度条。高度只有 1 行时（极端窄终端）只给进度条。
    if inner.height >= 2 {
        let [info_area, gauge_area] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(inner);
        render_song_info(frame, info_area, state, theme);
        render_progress(frame, gauge_area, state, theme);
    } else {
        render_progress(frame, inner, state, theme);
    }
}

/// 第 1 行：播放状态 + 歌名 · 歌手 · 专辑（左），下一首与音量/模式（右）。
///
/// 播放条只有两行，所以把歌手、专辑并进歌名那一行——拆成独立一行的话进度条
/// 就得再让一行出来，列表能显示的内容反而更少。右侧那一小段用暗色，不抢歌名。
fn render_song_info(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let marker = match state.playback {
        PlaybackState::Playing => crate::ui::icons::now_playing(true),
        PlaybackState::Paused => crate::ui::icons::now_playing(false),
        PlaybackState::Loading => crate::ui::icons::loading(),
        PlaybackState::Stopped => crate::ui::icons::stopped(),
    };

    let (left_text, emphasis) = match state.current.as_ref() {
        Some(song) => {
            let mut text = song.name.clone();
            let singer = song.singer_text();
            if !singer.is_empty() {
                text.push_str(" · ");
                text.push_str(&singer);
            }
            if !song.album_name.is_empty() {
                text.push_str(" · ");
                text.push_str(&song.album_name);
            }
            (text, theme.title())
        }
        None => ("未在播放（在列表里按 Enter 播放）".to_string(), theme.dim()),
    };

    // 右侧：下一首 + 音量 + 循环模式。宽度不够就整段不画，别把歌名挤没了。
    //
    // 宽度必须按**显示宽度**算，不能按字符数：`下一首 <歌名>` 里歌名常是中文，
    // 一个字符占两列，按字符数算出来的宽度会明显偏小——明明有空间，
    // 右侧那段却被 `truncate_to_width` 截成「下一首 WE GO · 播放中 · …」。
    let right_text = next_up_text(state);
    let right_width = (display_width(&right_text) as u16).min(area.width / 2);
    let show_right = !right_text.is_empty() && area.width >= 60;

    let (left_area, right_area) = if show_right {
        let [left, right] =
            Layout::horizontal([Constraint::Min(20), Constraint::Length(right_width)])
                .spacing(1)
                .areas(area);
        (left, Some(right))
    } else {
        (area, None)
    };

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(format!("{marker} "), theme.playback(state.playback)),
            Span::styled(
                truncate_to_width(&left_text, left_area.width.saturating_sub(3) as usize),
                emphasis,
            ),
        ])),
        left_area,
    );

    if let Some(right_area) = right_area {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                truncate_to_width(&right_text, right_area.width as usize),
                theme.dim(),
            )))
            .alignment(Alignment::Right),
            right_area,
        );
    }
}

/// 播放条右侧那一小段：「下一首 X · 播放中 · 音量 80% · 顺序」。
///
/// 拼一整串再整体截断，而不是各段分别截——分别截会出现「下一首 稻… · 播」这种
/// 两半都被切坏的残句。
fn next_up_text(state: &AppState) -> String {
    let mut parts: Vec<String> = Vec::new();

    if let Some(next) = state.queue.peek_next() {
        parts.push(format!(
            "{} 下一首 {}",
            crate::ui::icons::next_up(),
            next.name
        ));
    }
    parts.push(state.playback.label().to_string());
    parts.push(if state.is_muted() {
        "静音".to_string()
    } else {
        format!("音量 {:.0}%", state.volume * 100.0)
    });
    parts.push(state.queue.mode().label().to_string());

    parts.join(" · ")
}

/// 第 3 行：进度条。自带居中标签，把时间放在条上。
fn render_progress(frame: &mut Frame, area: Rect, state: &mut AppState, theme: &Theme) {
    let label = format!(
        "{} / {}",
        format_duration_ms(state.position_ms),
        format_duration_ms(state.duration_ms)
    );
    frame.render_widget(
        Gauge::default()
            .gauge_style(Style::default().fg(theme.progress).bg(theme.progress_bg))
            .ratio(state.progress_ratio())
            .label(Span::styled(label, theme.body())),
        area,
    );

    // 点击进度条可跳转：登记命中区，由 app 层按 x 比例换算成目标时间。
    state.add_hit_zone(area, HitTarget::Progress, 0, 1);
}

/// 当前行**未唱部分**的底色，在 `text_dim` 与 `text` 之间的位置。
///
/// 取 0.3：比别的行亮一档，整行才压得住上下两行；再亮就会让还没唱的字抢戏，
/// 逐字推进的「亮点」反而不明显了。
const LYRIC_ACTIVE_BASE: f32 = 0.30;

/// 非当前行的淡出跨度：距离 1 用 `text_dim`，超过这个距离就到底色 `lyric_far`。
///
/// 取 4 是照着一屏能显示十几行调的——太短则只有紧邻的两行有层次，
/// 太长则远处的行还看得清，当前行就不突出了。
const LYRIC_FADE_SPAN: f32 = 4.0;

/// 非当前行在「近色 → 远色」之间的插值比例。
///
/// 距离 1 是紧邻当前行的那一行，它应当保持原来的 idle 色（比例 0），
/// 所以先把 1 减掉再算。
///
/// 参数是**浮点**而不是整数，因为换行过渡期间距离是插值出来的：`t = 0` 时按旧
/// 当前行算、`t = 1` 时按新的算，中间几帧是小数。稳态下传进来的仍是整数值，
/// 结果与改造前逐位相同。
fn fade_ratio(distance: f32) -> f32 {
    ((distance - 1.0).max(0.0) / LYRIC_FADE_SPAN).min(1.0)
}

/// 两个距离之间按 `t` 线性插值。`t = 0` 取 `from`、`t = 1` 取 `to`。
///
/// 单独写一个而不是用 `from + (to - from) * t`：`t = 1` 时后者会因为浮点误差
/// 差一个 ulp，而稳态配色是拿精确相等做断言的（`distant_lines_fade_out` 就是）。
fn lerp(from: f32, to: f32, t: f32) -> f32 {
    if t >= 1.0 {
        return to;
    }
    if t <= 0.0 {
        return from;
    }
    from + (to - from) * t
}

/// 歌词面板。
///
/// 当前行始终垂直居中——这是卡拉OK式滚动的关键：视线固定屏幕中央，
/// 而不是跟着文字往下跑。
pub fn render_lyric(frame: &mut Frame, area: Rect, state: &mut AppState, theme: &Theme) {
    if area.height < 3 || area.width < 8 {
        return;
    }

    let title = match state.current.as_ref() {
        Some(song) => format!("歌词 · {}", song.name),
        None => "歌词".to_string(),
    };

    let block = panel(title, false, theme);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    // 封面不在这里画：`render_lyric` 被首页/歌词页/封面页共用，封面由各自的
    // `draw_cover_block` 决定放不放、放多大。
    let lyric_area = inner;

    if state.lyric.load.is_loading() && state.lyric.lyric.is_empty() {
        frame.render_widget(loading_placeholder(theme), lyric_area);
        return;
    }
    // 取失败要说「没取到」，不能落到下面那句「暂无歌词」——那是在替这首歌
    // 断言「它本来就没有歌词」。没有刷新键可用，所以不给「按 X 重试」的提示：
    // 换首歌或等下一首会自动重取。
    if let Some(reason) = state.lyric.load.error() {
        frame.render_widget(failed_placeholder(reason, None, theme), lyric_area);
        return;
    }
    if state.lyric.lyric.is_empty() {
        let hint = if state.current.is_some() {
            "暂无歌词"
        } else {
            "播放歌曲后显示歌词"
        };
        frame.render_widget(empty_placeholder(hint, theme), lyric_area);
        return;
    }

    let total = state.lyric.lyric.lines.len();
    let viewport = lyric_area.height as usize;
    let active = state.lyric.active_line;

    // 让当前行居中，同时不允许滚出内容范围
    let focus_line = active.unwrap_or(0);

    // 把每一行歌词展平为「原文 + 译文（若有）」的若干个显示单元。
    // 滚动定位按歌词原文行号找显示位置，所以译文不会破坏对齐。
    let mut display: Vec<(usize, bool, String)> = Vec::with_capacity(total * 2);
    for (index, line) in state.lyric.lyric.lines.iter().enumerate() {
        display.push((index, false, line.text.clone()));
        // 译文与音译**都**显示（各占一行），不再二选一。
        // 日语歌尤其需要：原文看不懂，译文管理解，罗马音管跟唱，两者用途不同。
        if let Some(translation) = line.translation.as_deref()
            && !translation.trim().is_empty()
        {
            display.push((index, true, translation.to_string()));
        }
        if let Some(romanization) = line.romanization.as_deref()
            && !romanization.trim().is_empty()
        {
            display.push((index, true, romanization.to_string()));
        }
    }

    let focus_display = display
        .iter()
        .position(|(index, _, _)| *index == focus_line)
        .unwrap_or(0);
    let max_offset = display.len().saturating_sub(viewport);
    let offset = focus_display.saturating_sub(viewport / 2).min(max_offset);

    // 内容整体下沉的行数：`offset` 被 `focus_display - viewport/2` 压到 0 时，
    // `skip(offset)` 之后内容从区域的**第一行**开始排，不可能上移——于是可视首行
    // 对应的是 `first_display`（不是 `offset`）。底部同理：`offset` 撞上
    // `max_offset` 之后内容型对齐，末行显示的是最后一句，不是 `offset + viewport`。
    // 命中区两个都要按它算（见本函数末尾），否则末屏与首屏的点击必然错位。
    let first_display = offset.min(focus_display);

    // 逐字着色要用当前播放位置，取一次即可（毫秒）
    let position_ms = state.position_ms;

    // 回填给 `App::frame_interval`：歌词**真的画出来了**才值得为逐字推进提速。
    // 放在这里而不是在那边重新推一遍布局——标签页、歌词面板开关、终端尺寸
    // 都会影响它，渲染层才是唯一知道真相的地方。
    state.lyric_visible = true;

    // 本帧「显示行 → 歌词行」的映射，供点击跳转用（见 `App::click_lyric`）。
    // 与 `hit_zones` 同一套约定：渲染层回填，`begin_frame` 清空。
    state
        .lyric
        .display_line_index
        .extend(display.iter().map(|(index, _, _)| *index));

    // 换行过渡的进度：`1.0` 表示稳态（没有过渡在进行）。
    let progress = state.lyric.transition_progress();
    let prev_line = state.lyric.prev_line;
    // 稳态走整数距离，既不白算浮点，也保证既有断言的精确相等。
    let transitioning = progress < 1.0;
    let prev_display = if transitioning {
        prev_line.and_then(|line| display_of(&display, line))
    } else {
        None
    };

    // 当前行未唱部分的底色：比 `text_dim` 亮一档，整行才压得住上下两行
    let active_base = mix(theme.text_dim, theme.text, LYRIC_ACTIVE_BASE);

    let lines: Vec<Line> = display
        .iter()
        .skip(offset)
        .take(viewport)
        .enumerate()
        .map(|(row, (index, is_translation, text))| {
            let here = offset + row;
            // 离当前行多远（按**显示行**算，译文行也占一格，视觉间隔才均匀）。
            //
            // 过渡期间这个距离是**插值**出来的：`t = 0` 时按旧当前行算、`t = 1`
            // 时按新的算。于是换行时不只是「新行点亮、旧行熄灭」，中间几行按距离
            // 排开的明暗层次也会一起平滑地重排——这才是 Apple Music 那种「整块
            // 歌词跟着动」的观感。终端没有子单元格定位，位置动不了，能动的就是
            // 这个颜色维度。
            let distance = if let Some(prev) = prev_display {
                let from = here.abs_diff(prev) as f32;
                let to = here.abs_diff(focus_display) as f32;
                lerp(from, to, progress)
            } else {
                here.abs_diff(focus_display) as f32
            };

            // 译文比它的原文再暗一档，一眼能分出主次
            let fade = fade_ratio(distance) + if *is_translation { 0.25 } else { 0.0 };

            // 「非当前行」该有的颜色。
            let base = mix(theme.text_dim, theme.lyric_far, fade);

            // 进入 / 退出：一个标量同时表达两件事——新行 0 → 1 点亮，旧行 1 → 0
            // 淡出，其余行恒为 0。稳态下它是 0 / 1 的硬值，退化成改造前的行为。
            let heat = if !transitioning {
                if Some(*index) == active { 1.0 } else { 0.0 }
            } else if Some(*index) == active {
                progress
            } else if Some(*index) == prev_line {
                1.0 - progress
            } else {
                0.0
            };

            if heat <= 0.0 {
                return Line::from(Span::styled(text.clone(), Style::default().fg(base)));
            }

            // 当前行：拿得到逐字时间戳就逐字染色（唱到哪亮到哪），
            // 拿不到就退回整行高亮——绝不为了效果让歌词和时间错位。
            let words = state
                .lyric
                .lyric
                .lines
                .get(*index)
                .map(|line| line.words.as_slice())
                .unwrap_or(&[]);
            if words.len() != text.chars().count() {
                // 保留 `lyric_active()` 自带的 BOLD，只换前景色——稳态下
                // `mix(base, accent, 1.0)` 精确等于 `accent`，与改造前逐位相同。
                return Line::from(Span::styled(
                    text.clone(),
                    theme.lyric_active().fg(mix(base, theme.accent, heat)),
                ));
            }

            // 每个字按**它自己**的进度在「未唱底色 → 强调色」之间取值。
            // 边界字因此是两色之间的过渡，看上去是渐变扫过，不是一格一格硬跳。
            // 非真彩终端没有中间色阶，`mix` 会自动退回两端取一，行为等价于
            // 原来的三档离散——不需要在这里特判。
            //
            // 再叠一层 `heat`：过渡期间整行（含已唱的字）从它原来的暗色一起亮起来，
            // 而不是只有未唱部分变亮。
            let spans: Vec<Span> = text
                .chars()
                .zip(words.iter())
                .map(|(character, word)| {
                    let lit = mix(active_base, theme.accent, word.progress_at(position_ms));
                    let color = mix(base, lit, heat);
                    Span::styled(character.to_string(), Style::default().fg(color))
                })
                .collect();
            Line::from(spans)
        })
        .collect();

    // 注意：上面已经用 skip(offset).take(viewport) 裁好了要显示的行，
    // 这里**不能**再调 .scroll((offset, 0))——那会形成双重偏移（实际滚 2×offset），
    // 滚得越来越快，很快就滚过内容末尾，表现就是「歌词播到一半后再也不出现」。
    frame.render_widget(
        Paragraph::new(lines).alignment(Alignment::Center),
        lyric_area,
    );

    // 点击这一行的任意位置 → 跳到这一句的起始时间。
    //
    // 登记在最后：`hit_test` 取**后登记**的优先，这样歌词面板上浮出的右键菜单
    // 之类小区域不会被它盖住。范围只到 `lyric_area`，不会吃掉别的面板的点击。
    //
    // **只登记真正画出来的那些行**——这是「点歌词总是差好几行」的根治。
    // `hit_zones` 的 row 是**屏幕行号**，`index_at` 先做 `row - rect.top()`；
    // `Rect` 只能记一个 `top`，所以两个「上沿」不同的东西必然错位：
    //
    //   * `lyric_area` 这里是**内容区**（`block.inner(area)`，已扣掉边框）；
    //     而它的来源 `render_lyric_panel` 会先被 `draw_cover_block` 让出一块
    //     封面缩略图——内容区上沿比整块面板低 6~32 行。顶部的歌词行因此不是从
    //     区域第一行开始排的，点击坐标必须跟着下移同样的行数。
    //   * 歌词末尾滚动时 `offset` 被 `max_offset` 截断，`Paragraph` 的内容却从
    //     区域第一行开始排——`skip(offset)` 与可视首行不再一一对应。
    //   * 歌词只有一行时按区间折算出来的行数会大于真实内容，下方空白也成了可点区。
    //
    // 行数取「可视行」与「内容剩余行」的小者；上沿取内容区顶加**实际内边距**
    // （`offset < first_display` 时内容整体下移的行数）。两处都从这一帧真正渲染的
    // 布局反推，不以区域形状为假设——终端最大化 / 最小化改变的是这里，改错了
    // 就是「换个窗口大小偏移量还变」。
    //
    // 注意 `lyric_area` 已经是内容区（边框已扣）：上沿**不能再减边框**，
    // 否则整块命中区上移一行，点第 N 行会落到第 N-1 行——2026-09-29 的 pty
    // 逐行对照实验（点击 8 行、8 行全部偏一句）钉的就是这一处。
    let pad = offset.saturating_sub(first_display);
    let content_rows = viewport.saturating_sub(pad);
    let rows = content_rows.min(display.len().saturating_sub(offset));
    let top = lyric_area.y.saturating_add(pad as u16);
    state.add_hit_zone(
        Rect::new(lyric_area.x, top, lyric_area.width, rows as u16),
        HitTarget::LyricLine,
        offset,
        display.len(),
    );
}

/// 某个歌词行在**显示行**里的位置（译文/音译会让两者不再一一对应）。
fn display_of(display: &[(usize, bool, String)], line: usize) -> Option<usize> {
    display.iter().position(|(index, _, _)| *index == line)
}

/// 播放队列面板需要的外部状态。
///
/// 打包成结构体而不是摊成三个参数：`render_queue` 本来就要接 frame / area /
/// queue / cursor / theme，再散开就超过 clippy 的 7 参数上限了。
#[derive(Debug, Clone, Copy)]
pub struct QueueView<'a> {
    /// 队列面板是否拥有键盘焦点。
    pub focused: bool,
    /// 当前播放曲目的 hash。
    pub current_hash: Option<&'a str>,
    pub playback: PlaybackState,
}

/// 播放队列面板。
pub fn render_queue(
    frame: &mut Frame,
    area: Rect,
    queue: &PlayQueue,
    cursor: &mut ListState,
    view: QueueView<'_>,
    theme: &Theme,
) {
    if area.height < 3 || area.width < 12 {
        return;
    }

    let title = format!("播放队列 · {} 首 · {}", queue.len(), queue.mode().label());
    let block = panel(title, view.focused, theme);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if queue.is_empty() {
        frame.render_widget(
            empty_placeholder(
                &format!("队列为空 · 在列表里按 {} 加入", key_hint_for("a")),
                theme,
            ),
            inner,
        );
        return;
    }

    let row_width = inner.width.saturating_sub(1) as usize;
    // 可见行数（供 `row_is_visible` 判断哪些行值得真正构造）
    let visible_rows = inner.height as usize;
    // 同 `render_song_list`：`offset`/`selected` 是渲染前的旧值，列表刚缩短时可能越界，
    // 先按当前长度钳一遍，避免整屏被误判成窗口外。
    let last = queue.len().saturating_sub(1);
    let offset = cursor.offset().min(last);
    let selected = cursor.selected().map(|index| index.min(last));
    let items: Vec<_> = queue
        .items()
        .iter()
        .enumerate()
        .map(|(index, song)| {
            // 窗口外的行只放等高占位，理由见 `row_is_visible`。
            if !row_is_visible(index, offset, selected, visible_rows) {
                return ListItem::from("");
            }
            let context = RowContext {
                width: row_width,
                is_current: view.current_hash == Some(song.hash.as_str()),
                playback: view.playback,
                hover: false,
            };
            song_row(index, song, context, theme)
        })
        .collect();

    let widget = selection_list(items, theme);

    frame.render_stateful_widget(widget, inner, cursor);
}

/// 缩略图封面的行数下限 / 上限。
///
/// 上限不只是审美：图片协议按区域尺寸编码，区域越大单次编码的数据越多，
/// 而缩略图那块本来就窄，再大没有意义。
const COVER_MIN_ROWS: u16 = 6;
const COVER_MAX_ROWS: u16 = 32;

/// 封面块在区域里怎么摆。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CoverPlace {
    /// 只占区域上半部分，按图片比例居中（歌词面板上方、窄屏首页的封面）。
    ///
    /// 这块只有几十个字符大，裁剪只会更看不清，所以固定「完整显示」，
    /// 不跟随 [`CoverFill`] 配置。
    AboveContent,
    /// 铺满整块区域（宽屏首页左栏那块大封面），怎么铺由配置决定。
    Fill,
}

/// 在 `inner` 上方划出一块居中的封面缩略图区，返回（缩略图区, 剩余区）。
///
/// 抽成纯函数是为了能直接测：这块几何一变，图片协议就得重新编码（尺寸变了），
/// 值得有断言兜着。
///
/// 字符宽高比约 1:2，所以方形区域的列数取行数的两倍；最多占一半高，
/// 剩下的留给下方内容。区域太小时返回 `None`，调用方把整块都留给内容。
fn thumbnail_layout(inner: Rect, aspect: f32, cell_aspect: f32) -> Option<(Rect, Rect)> {
    if inner.width < 12 || inner.height < 8 {
        return None;
    }

    // 图被压成 0 宽会让下面的除法炸掉；宽高比拿不到时按方图算
    let aspect = aspect.max(0.05);
    let rows = (inner.height / 2).clamp(COVER_MIN_ROWS, COVER_MAX_ROWS);

    // 按真实比例算列数（横图列数大于行数）。缩略图**不追求填满**——它本来就只占
    // 上半部分，左右留白反而显得居中。真要填满的是首页那块，走 `prepare_cover`
    // 的裁剪路径。
    let columns = ((f32::from(rows) * aspect * cell_aspect).round() as u16).clamp(1, inner.width);

    // 太宽就按 inner.width 反算（缩略图不能横向溢出）
    let rows = if columns >= inner.width {
        let rows = (f32::from(inner.width) / (aspect * cell_aspect)).round() as u16;
        rows.clamp(COVER_MIN_ROWS, inner.height.saturating_sub(1))
    } else {
        rows
    };

    let [_cover_area, rest] =
        Layout::vertical([Constraint::Length(rows + 1), Constraint::Min(1)]).areas(inner);

    let x = inner.x + inner.width.saturating_sub(columns) / 2;
    Some((Rect::new(x, inner.y, columns, rows), rest))
}

/// 区域换算成像素尺寸：`列 × 单元格宽`, `行 × 单元格高`。
///
/// **必须用 `Picker` 报的 `FontSize`**——图片协议内部就是按它把像素折回单元格的。
/// 这里换个比例算，裁出来的图比例就对不上，渲染时又会留边。
fn pixel_size(area: Rect, font: FontSize) -> (u32, u32) {
    (
        (u32::from(area.width) * u32::from(font.width)).max(1),
        (u32::from(area.height) * u32::from(font.height)).max(1),
    )
}

/// 等比放大到**盖住** `(width, height)`，再居中裁到正好这个尺寸。
///
/// 等价 CSS 的 `object-fit: cover`：铺满 100%、不变形，代价是裁掉溢出的边。
fn crop_to_cover(image: &image::DynamicImage, (width, height): (u32, u32)) -> image::DynamicImage {
    let src_w = image.width().max(1);
    let src_h = image.height().max(1);
    // 取较大的那个比例：两个方向都要盖住，取小的会留边
    let scale = f64::max(
        f64::from(width) / f64::from(src_w),
        f64::from(height) / f64::from(src_h),
    );
    // 向上取整：宁可多放大半个像素，也不能因为取整让某一边差一点盖不满
    let scaled_w = (f64::from(src_w) * scale).ceil().max(f64::from(width)) as u32;
    let scaled_h = (f64::from(src_h) * scale).ceil().max(f64::from(height)) as u32;

    let scaled = image.resize_exact(scaled_w, scaled_h, FilterType::Lanczos3);
    scaled.crop_imm(
        (scaled_w - width) / 2,
        (scaled_h - height) / 2,
        width,
        height,
    )
}

/// 直接拉到目标尺寸——**不保持比例**，只给 [`CoverFill::Stretch`] 用。
fn stretch_to(image: &image::DynamicImage, (width, height): (u32, u32)) -> image::DynamicImage {
    image.resize_exact(width, height, FilterType::Lanczos3)
}

/// 在 `area` 里找出与图片像素比例一致的最大矩形，居中放置。
fn fit_box(image: &image::DynamicImage, area: Rect, font: FontSize) -> Rect {
    // 退化区域直接返回空矩形。**不能省这一步**：下面的 `clamp(1, area.height)`
    // 在 `area.height == 0` 时会 panic（`Ord::clamp` 要求 `min <= max`），
    // 而硬凑成 1×1 又会把框撑到区域外面去（`area.width - columns` 当场下溢）。
    // 现在 `render_home` 的布局恰好不会产出 0 行的封面区（`Min(6)` 优先于
    // `Length(8)`，实测 4~24 行时最低给到 4），但那是布局的巧合，不是这里的保证。
    if area.width == 0 || area.height == 0 {
        return Rect::new(area.x, area.y, 0, 0);
    }

    let image_aspect = image.width().max(1) as f32 / image.height().max(1) as f32;
    // 单元格是「高 : 宽 = font.height : font.width」，换算成列数要乘上去
    let cell_aspect = f32::from(font.height) / f32::from(font.width.max(1));

    let mut columns = (f32::from(area.height) * image_aspect * cell_aspect).round() as u16;
    let mut rows = area.height;
    if columns > area.width {
        columns = area.width;
        rows = (f32::from(columns) / (image_aspect * cell_aspect)).round() as u16;
    }
    let columns = columns.clamp(1, area.width);
    let rows = rows.clamp(1, area.height);

    Rect::new(
        area.x + (area.width - columns) / 2,
        area.y + (area.height - rows) / 2,
        columns,
        rows,
    )
}

/// 按 `mode` 把 `image` 塞进 `area`，返回（交给图片协议的图, 真正要渲染的矩形）。
///
/// # 为什么必须自己预处理像素
///
/// `ratatui-image` 的三种 `Resize` **全都保持宽高比**：
///
/// * `Fit` 是「装得下就不放大」；
/// * `Scale` 是「允许放大」，但仍然等比；
/// * `Crop` 甚至不放大，图比区域小就原样画。
///
/// 而封面区的像素比例（列 × 单元格宽 : 行 × 单元格高）几乎永远不等于图片比例，
/// 于是不管选哪个，图都只占区域的一部分，剩下的地方是空的——**这才是「封面没有
/// 完全填满」的真正原因，不是没放大**。想真正铺满，只能先把图裁/拉到与区域完全
/// 一致的比例，再交给它渲染。
pub fn prepare_cover(
    image: &image::DynamicImage,
    area: Rect,
    font: FontSize,
    mode: CoverFill,
) -> (image::DynamicImage, Rect) {
    match mode {
        CoverFill::Crop => (crop_to_cover(image, pixel_size(area, font)), area),
        CoverFill::Stretch => (stretch_to(image, pixel_size(area, font)), area),
        // 不裁不拉：把「要渲染的区域」缩到图片自己的比例，居中放进 area
        CoverFill::Fit => (image.clone(), fit_box(image, area, font)),
    }
}

/// 画封面，返回留给下方内容的区域。
///
/// 首页与歌词面板都要它，区别只在 [`CoverPlace`]。
///
/// 图片走 `ratatui-image` 的 widget：它把图写进 ratatui 的 Buffer，由框架的
/// diff 统一输出——不再自己往 stdout 写几百 KB 的转义序列（那会阻塞写入并打乱
/// 光标跟踪），而且内容不变时一个字节都不会重发。
fn draw_cover_block(
    frame: &mut Frame,
    inner: Rect,
    state: &mut AppState,
    place: CoverPlace,
) -> Rect {
    if !state.cover.is_drawable() {
        return inner;
    }

    // 字符高宽比：配的 qr_aspect 就是「字符高:宽」，同一个概念，直接复用。
    // 注意它只决定**框的形状**（看起来是不是方的）；裁图用的像素尺寸另算，
    // 那个必须跟图片协议内部的 `FontSize` 一致，见 `pixel_size`。
    let cell_aspect = state.config.qr_aspect.max(0.1);

    let (mode, area, rest) = match place {
        CoverPlace::AboveContent => {
            let Some((box_area, rest)) = thumbnail_layout(inner, state.cover.aspect, cell_aspect)
            else {
                return inner;
            };
            (CoverFill::Fit, box_area, rest)
        }
        CoverPlace::Fill => {
            let mode = state.config.cover_fill;
            (mode, inner, inner)
        }
    };

    if let Some(picker) = state.picker.as_ref()
        && let Some((protocol, render_area)) = state.cover.fit_to(mode, area, picker)
    {
        // `Scale`：允许放大，等比铺到 `render_area`。图的比例已经被
        // `prepare_cover` 对齐到区域了，所以这里正好铺满、不留边。
        frame.render_stateful_widget(
            StatefulImage::default().resize(Resize::Scale(None)),
            render_area,
            protocol,
        );
        // 编码发生在渲染时（只在区域或图片变化时）。失败只记日志：
        // 下一帧会重试，不该因为一张图把界面搞崩。
        if let Some(Err(error)) = protocol.last_encoding_result() {
            crate::logger::tlog!(crate::logger::LEVEL_WARN, "封面编码失败：{error}");
        }
    }
    // 没有终端图形能力（`picker` 为空）或没有原图时留白——画不出东西比画错好
    rest
}

/// 普通页面右下角的歌词面板：上方小封面 + 下方歌词。
///
/// 这里的面板**没有自己的边框**（外层已经由调用方画好了），所以不要在内部
/// 再套一层 `panel`。
pub fn render_lyric_panel(frame: &mut Frame, area: Rect, state: &mut AppState, theme: &Theme) {
    if area.height < 3 || area.width < 8 {
        return;
    }
    let rest = draw_cover_block(frame, area, state, CoverPlace::AboveContent);
    render_lyric(frame, rest, state, theme);
}

/// 头像占的列数：6 行内容 × 字符高宽比 2 = 12 列。
///
/// 这是给**真图片**的：12 列 × 6 行在支持 kitty / sixel / iTerm2 的终端上是
/// 约 108×108 像素，头像认得出来。
const AVATAR_COLUMNS: u16 = 12;

/// 半块字符模式下头像降级成昵称首字，只占 3 列（首字 + 两侧各留一列）。
///
/// 不沿用 12 列：一个首字占 12 列纯粹是浪费，而账号区的文字列本来就不够宽
/// ——实测 12 列时「概念版 TVIP · 至 09-28」会被截成「至 09…」，日期等于没写。
const AVATAR_INITIAL_COLUMNS: u16 = 3;

/// 首页账号区的高度：边框 2 行 + 内容 6 行（头像要能看清，2 行只能画 4 列宽）。
///
/// 固定值，不能被封面框挤掉——封面用剩下的空间。
const ACCOUNT_HEIGHT: u16 = 8;

/// 今天的「概念版」VIP 是否已经领过。
///
/// 比较的是**本地日期**，必须和领取时用的是同一套算法（[`crate::util::today_local`]）
/// ——两边不一致会出现「领了却显示没领」，于是每次都白打一遍接口。
///
/// 取不到本地日期时返回 `false`（显示成「还没领」）而不是 `true`：那种情况下
/// 按 `V` 会得到一句「取不到本地日期」的解释，比默默显示「已领取」这个谎话好。
fn claimed_today(state: &AppState) -> bool {
    let Some(today) = crate::util::today_local() else {
        return false;
    };
    state.vip_claimed_day.as_deref() == Some(today.as_str())
}

/// 首页左下角的账号区：头像 + 昵称 · 等级 · 累计听歌时长。
///
/// **刻意不放**粉丝数、关注数、访客、星座、勋章——`/user/detail` 返回十几个
/// 字段，全摆上来这里就成了数据表，而首页要回答的是「在放什么」。只留一眼
/// 能读完的三项。
fn render_account(frame: &mut Frame, area: Rect, state: &mut AppState, theme: &Theme) {
    let block = panel("我的资料", false, theme);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height == 0 || inner.width < 8 {
        return;
    }

    let Some(info) = state.user_info.clone() else {
        // 三种「没有资料」必须分开说：未登录 / 正在取 / 取失败。
        //
        // 早先只区分了前两种，于是接口挂掉时首页会**永远**停在「加载中…」——
        // 用户分不清是失败还是慢，也没有任何可以照做的动作。
        let (text, style) = if !state.logged_in {
            (
                format!("未登录（按 {} 扫码）", key_hint_for("L")),
                theme.dim(),
            )
        } else if let Some(reason) = state.user_info_load.error() {
            (
                format!("资料载入失败：{reason}"),
                theme.status(crate::app::state::StatusLevel::Error),
            )
        } else {
            ("加载中…".to_string(), theme.dim())
        };
        frame.render_widget(
            Paragraph::new(Span::styled(
                truncate_to_width(&text, inner.width as usize),
                style,
            )),
            inner,
        );

        // 失败时给一个能照做的入口。键盘上没有「重取资料」这个键位，所以只挂
        // 鼠标：整个账号区都可点（命中区取「后登记的优先」，这里没有别的区域）。
        if state.user_info_load.error().is_some() && inner.height >= 2 {
            frame.render_widget(
                Paragraph::new(Span::styled("点这里重试", theme.key_hint())),
                Rect::new(inner.x, inner.y + 1, inner.width, 1),
            );
            state.add_hit_zone(inner, HitTarget::ProfileRetry, 0, 1);
        }
        return;
    };

    // 终端只能退到半块字符时**不画照片**：12 列 × 6 行在半块模式下只有
    // 24×12 个「像素」，任何头像在这个尺寸下都只是一团噪点——比不画还糟，
    // 用户会以为渲染坏了。改画昵称首字，而且只占 3 列——首字用不了 12 列，
    // 省下来的宽度留给右边的文字（那里正放不下完整的会员摘要）。
    //
    // 支持 kitty / sixel / iTerm2 的终端照旧走图片：那里 12×6 个单元格是真实的
    // 像素尺寸（典型终端约 108×108），头像认得出来。
    let text_only = state
        .picker
        .as_ref()
        .is_none_or(|picker| picker.protocol_type() == ProtocolType::Halfblocks);
    let avatar_columns = if text_only {
        AVATAR_INITIAL_COLUMNS
    } else {
        AVATAR_COLUMNS
    };

    // 左头像 / 右文字。窄到放不下头像就整块给文字
    let (avatar_area, text_area) = if inner.width >= avatar_columns + 14 {
        let [left, right] =
            Layout::horizontal([Constraint::Length(avatar_columns), Constraint::Min(10)])
                .spacing(1)
                .areas(inner);
        (Some(left), right)
    } else {
        (None, inner)
    };

    let name = if info.nickname.is_empty() {
        "（无名）".to_string()
    } else {
        info.nickname.clone()
    };

    if let Some(avatar_area) = avatar_area {
        if text_only {
            render_avatar_initial(frame, avatar_area, &name, theme);
        } else if let Some(protocol) = state.avatar.protocol.as_mut() {
            frame.render_stateful_widget(StatefulImage::default(), avatar_area, protocol);
            if let Some(Err(error)) = protocol.last_encoding_result() {
                crate::logger::tlog!(crate::logger::LEVEL_WARN, "头像编码失败：{error}");
            }
        }
    }

    // 昵称 + 等级
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(vec![
        Span::styled(
            truncate_to_width(&name, text_area.width.saturating_sub(8) as usize),
            theme.title(),
        ),
        Span::styled(
            info.grade
                .map(|grade| format!("  Lv.{grade}"))
                .unwrap_or_default(),
            theme.dim(),
        ),
    ]));

    // 会员摘要（有就显示）。首页宽，用完整形态（带产品名）
    if let Some(label) = state
        .vip_info
        .as_ref()
        .map(crate::api::cloud::VipInfo::label)
    {
        lines.push(Line::from(Span::styled(
            truncate_to_width(&label, text_area.width as usize),
            theme.now_playing(),
        )));
    }

    // 领取今日 VIP。**只有概念版音源才显示**——这是概念版专属接口，标准版账号
    // 调了只会拿到错误码，摆一个按了没用的入口比不摆更糟。
    if state.config.active_source_kind() == crate::source::SourceKind::KugouConcept
        && (lines.len() as u16) < text_area.height
    {
        let (text, style) = if state.vip_claiming {
            ("领取中…".to_string(), theme.dim())
        } else if claimed_today(state) {
            ("今日 VIP 已领取".to_string(), theme.dim())
        } else {
            (
                format!("领取今日 VIP · 按 {}", key_hint_for("V")),
                theme.key_hint(),
            )
        };

        let row = text_area.y + lines.len() as u16;
        lines.push(Line::from(Span::styled(
            truncate_to_width(&text, text_area.width as usize),
            style,
        )));
        // 鼠标点这一行也能领（键盘是 V）
        state.add_hit_zone(
            Rect::new(text_area.x, row, text_area.width, 1),
            HitTarget::VipClaim,
            0,
            1,
        );
    }

    // 累计听歌时长
    if let Some(duration) = info.duration_text() {
        lines.push(Line::from(Span::styled(
            truncate_to_width(&format!("听过 {duration}"), text_area.width as usize),
            theme.dim(),
        )));
    }

    frame.render_widget(Paragraph::new(lines), text_area);
}

/// 头像区的降级画法：昵称首字，**与昵称同一行**。
///
/// 只在终端退到半块字符时用（见 `render_account` 里的判断）——那个模式下
/// 12 列 × 6 行只有 24×12 个「像素」，照片认不出来。
///
/// 顶对齐而不是垂直居中：账号区有 6 行，居中会把首字落到第 4 行，正好和
/// 「听过 N 小时」对齐——看上去跟昵称毫无关系。贴在首行才读得出是「这个人的头像」。
fn render_avatar_initial(frame: &mut Frame, area: Rect, nickname: &str, theme: &Theme) {
    let Some(initial) = nickname.chars().next() else {
        return;
    };
    if area.height == 0 || area.width == 0 {
        return;
    }

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(initial.to_string(), theme.title())))
            .alignment(Alignment::Center),
        Rect { height: 1, ..area },
    );
}

/// 首页：正在播放的总览——封面在左，曲目信息与歌词在右。
pub fn render_home(frame: &mut Frame, area: Rect, state: &mut AppState, theme: &Theme) {
    let block = panel("正在播放", false, theme);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height < 4 || inner.width < 20 {
        return;
    }

    if state.current.is_none() {
        frame.render_widget(
            empty_placeholder(
                &format!(
                    "还没有播放任何歌曲 · 去搜索页按 {} 找一首",
                    key_hint_for("/")
                ),
                theme,
            ),
            inner,
        );
        return;
    }

    // 宽屏左右分栏（封面 | 歌词），窄屏上下堆叠
    if inner.width >= 60 {
        let [left, right] =
            Layout::horizontal([Constraint::Percentage(45), Constraint::Min(24)]).areas(inner);

        // 左栏竖着切：上方封面（铺满整块）、下方账号信息（固定 8 行，不能被挤掉）。
        //
        // 之前让封面框按图片比例自己算高度，结果框能把整个左栏吃掉，
        // 账号区高度变成 0 —— 用户看到「个人信息被挤掉了」。
        // 现在账号区固定，封面用剩下的全部空间，按 `cover_fill` 铺满
        // （见 `prepare_cover`）。
        let [cover_col, account_col] =
            Layout::vertical([Constraint::Min(6), Constraint::Length(ACCOUNT_HEIGHT)]).areas(left);
        let left_block = panel("封面", false, theme);
        let left_inner = left_block.inner(cover_col);
        frame.render_widget(left_block, cover_col);
        draw_cover_block(frame, left_inner, state, CoverPlace::Fill);
        render_account(frame, account_col, state, theme);
        render_lyric(frame, right, state, theme);
    } else {
        // 窄屏没有左右分栏的余地：封面缩到上半部分，下半部分留给歌词
        let rest = draw_cover_block(frame, inner, state, CoverPlace::AboveContent);
        render_lyric(frame, rest, state, theme);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::theme::ThemeName;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    /// 造一张纯色图，用来断言「裁完的尺寸对不对」。
    fn solid(width: u32, height: u32) -> image::DynamicImage {
        image::DynamicImage::ImageRgb8(image::RgbImage::new(width, height))
    }

    /// 取屏幕文本并**去掉所有空白**。
    ///
    /// ratatui 会给每个宽字符后面补一个占位格，所以 buffer 里的「载入失败」实际是
    /// 「载 入 失 败」——直接 `contains` 会假失败。两边都先挤掉空白再比。
    fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
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

    /// 半块字符模式下头像要降级成昵称首字，而不是把照片糊成一团噪点。
    ///
    /// 12 列 × 6 行在半块模式下只有 24×12 个「像素」，照片在这个尺寸下认不出来。
    #[test]
    fn avatar_falls_back_to_the_nickname_initial() {
        let area = Rect::new(0, 0, AVATAR_COLUMNS, 6);
        let mut terminal = Terminal::new(TestBackend::new(AVATAR_COLUMNS, 6)).expect("建测试终端");
        let theme = Theme::for_config(ThemeName::Default, false);

        terminal
            .draw(|frame| render_avatar_initial(frame, area, "惜别", &theme))
            .expect("渲染降级头像");

        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains('惜'), "应画昵称首字：{text}");
        assert!(!text.contains('别'), "只画首字，不该整串铺上去：{text}");
    }

    /// 昵称取不到时不能崩，也不能画出个空框。
    #[test]
    fn avatar_fallback_tolerates_an_empty_nickname() {
        let area = Rect::new(0, 0, AVATAR_COLUMNS, 6);
        let mut terminal = Terminal::new(TestBackend::new(AVATAR_COLUMNS, 6)).expect("建测试终端");
        let theme = Theme::for_config(ThemeName::Default, false);

        terminal
            .draw(|frame| render_avatar_initial(frame, area, "", &theme))
            .expect("空昵称也不该 panic");
    }

    /// 歌词取失败要说「没取到」，不能显示「暂无歌词」。
    ///
    /// 「暂无歌词」是在替这首歌断言「它本来就没有歌词」——取失败和本来没有是
    /// 两回事，混成一句会让用户以为这首歌没歌词，转而去别处找原因。
    #[test]
    fn lyric_failure_is_not_reported_as_an_empty_lyric() {
        let mut state = AppState::new(crate::config::Config::default());
        state.lyric.hash = Some("h".to_string());
        state.lyric.load.fail("网络请求失败：连接被拒");

        let mut terminal = Terminal::new(TestBackend::new(40, 8)).expect("建测试终端");
        let theme = Theme::for_config(ThemeName::Default, false);
        terminal
            .draw(|frame| render_lyric(frame, Rect::new(0, 0, 40, 8), &mut state, &theme))
            .expect("渲染歌词面板");

        let text = buffer_text(&terminal);
        assert!(text.contains("载入失败"), "应说明是取失败：{text}");
        assert!(
            !text.contains("暂无歌词"),
            "不能把取失败说成「这首歌没有歌词」：{text}"
        );
    }

    /// 真的没有歌词（请求成功但内容为空）时，仍然显示「暂无歌词」。
    #[test]
    fn an_empty_lyric_still_reads_as_no_lyrics() {
        let mut state = AppState::new(crate::config::Config::default());
        // 有当前曲目才会落到「暂无歌词」；没有曲目时是「播放歌曲后显示歌词」
        state.current = Some(crate::api::model::Song::default());
        state.lyric.hash = Some("h".to_string());
        state.lyric.load.succeed();
        state.lyric.lyric = crate::api::model::Lyric::default();

        let mut terminal = Terminal::new(TestBackend::new(40, 8)).expect("建测试终端");
        let theme = Theme::for_config(ThemeName::Default, false);
        terminal
            .draw(|frame| render_lyric(frame, Rect::new(0, 0, 40, 8), &mut state, &theme))
            .expect("渲染歌词面板");

        let text = buffer_text(&terminal);
        assert!(text.contains("暂无歌词"), "确实没有歌词时应照常说：{text}");
        assert!(!text.contains("载入失败"), "这不是失败：{text}");
    }

    /// 缩略图区是「列 = 行 × 2」的方形（字符宽高比 1:2），并水平居中。
    #[test]
    fn thumbnail_is_twice_as_wide_as_tall_and_centered() {
        let inner = Rect::new(0, 0, 60, 20);
        let (cover, _rest) = thumbnail_layout(inner, 1.0, 2.0).expect("60x20 够放缩略图");

        assert_eq!(cover.height, 10, "最多占一半高");
        assert_eq!(cover.width, 20, "列数是行数的两倍");
        assert_eq!(cover.x, 20, "剩余宽度左右平分");
        assert_eq!(cover.y, inner.y);
    }

    /// 区域太小就整块留给内容——半个缩略图对阅读毫无帮助。
    #[test]
    fn thumbnail_is_skipped_when_area_is_tiny() {
        assert!(thumbnail_layout(Rect::new(0, 0, 11, 20), 1.0, 2.0).is_none());
        assert!(thumbnail_layout(Rect::new(0, 0, 60, 7), 1.0, 2.0).is_none());
    }

    /// 行数被夹在 [6, 32]：矮区域不至于缩成一条，高区域也不会把内容挤没。
    #[test]
    fn thumbnail_rows_are_clamped() {
        let (short, _) = thumbnail_layout(Rect::new(0, 0, 40, 8), 1.0, 2.0).expect("8 行够放");
        assert_eq!(short.height, COVER_MIN_ROWS);

        let (tall, _) = thumbnail_layout(Rect::new(0, 0, 80, 100), 1.0, 2.0).expect("100 行够放");
        assert_eq!(tall.height, COVER_MAX_ROWS);
        assert_eq!(tall.width, 64, "32 行 × 2（方图 + 字符 2:1）");
    }

    /// 非正方形封面按真实比例算列数——16:9 的头图不该被压成方的。
    #[test]
    fn thumbnail_respects_image_aspect_ratio() {
        let (square, _) = thumbnail_layout(Rect::new(0, 0, 80, 20), 1.0, 2.0).expect("够放");
        let (wide, _) = thumbnail_layout(Rect::new(0, 0, 80, 20), 16.0 / 9.0, 2.0).expect("够放");

        assert!(
            wide.width > square.width,
            "16:9 的图应该比方图宽：wide={} square={}",
            wide.width,
            square.width
        );

        // 竖图（比如 3:4 的歌手照）应该更窄
        let (tall, _) = thumbnail_layout(Rect::new(0, 0, 80, 20), 0.75, 2.0).expect("够放");
        assert!(tall.width < square.width, "竖图应该比方图窄");
    }

    /// 窄区域里宽度是硬约束：宁可矮一点也不让缩略图超出边界。
    #[test]
    fn thumbnail_width_is_capped_by_area_width() {
        let (cover, _) = thumbnail_layout(Rect::new(0, 0, 14, 20), 1.0, 2.0).expect("够放");
        assert_eq!(cover.width, 14, "10 行本该要 20 列，被宽度压到 14");
        assert_eq!(cover.x, 0);
    }

    /// 剩余区域紧接缩略图下方，且两者高度加起来仍是原区域高度。
    #[test]
    fn remainder_sits_below_the_thumbnail() {
        let inner = Rect::new(3, 5, 60, 20);
        let (cover, rest) = thumbnail_layout(inner, 1.0, 2.0).expect("够放");

        // rows + 1：多留一行当间距，不然缩略图和下面的内容会糊在一起
        assert_eq!(rest.y, cover.y + cover.height + 1);
        assert_eq!(rest.height, inner.height - cover.height - 1);
        assert_eq!(rest.width, inner.width, "剩余区用满宽度");
    }

    // ---- 铺满（`prepare_cover`）：这是「封面填不满」的根治点 ----

    /// 裁剪模式的**核心不变量**：裁完的图，像素比例与目标区域完全一致。
    ///
    /// 只要这一条成立，`ratatui-image` 的等比缩放就会正好铺满整个区域，不留黑边。
    /// 以前填不满就是因为区域是 45% 宽 × 剩下高（像素比例约 1.5:1），而图是方的。
    #[test]
    fn crop_matches_the_area_pixel_ratio_exactly() {
        let font = FontSize::new(10, 20);
        // 典型的宽扁封面区：34 列 × 11 行 → 340 × 220 像素
        let area = Rect::new(0, 0, 34, 11);
        let (width, height) = pixel_size(area, font);
        assert_eq!((width, height), (340, 220));

        let cropped = crop_to_cover(&solid(256, 256), (width, height));
        assert_eq!(cropped.width(), 340, "裁完正好是区域的像素宽");
        assert_eq!(cropped.height(), 220, "裁完正好是区域的像素高");
    }

    /// 裁剪模式是「盖住再裁」：短边不裁，长边裁掉，且两侧对称。
    #[test]
    fn crop_keeps_the_short_side_and_centers_the_overflow() {
        // 100x100 的方图 → 目标 200x100（宽是高的两倍）
        // 等比放大到盖住 → 200x200，再上下各裁 50 行
        let cropped = crop_to_cover(&solid(100, 100), (200, 100));
        assert_eq!((cropped.width(), cropped.height()), (200, 100));
    }

    /// 目标比原图小也要正确缩小（下载的封面 256 见方，区域常常比它小）。
    #[test]
    fn crop_also_shrinks() {
        let cropped = crop_to_cover(&solid(256, 256), (60, 40));
        assert_eq!((cropped.width(), cropped.height()), (60, 40));
    }

    /// 拉伸模式：直接变成目标尺寸，不保持比例。
    #[test]
    fn stretch_resizes_without_keeping_the_ratio() {
        let stretched = stretch_to(&solid(100, 400), (200, 100));
        assert_eq!((stretched.width(), stretched.height()), (200, 100));
    }

    /// 完整显示模式：区域缩到图片比例，居中，绝不超出区域。
    #[test]
    fn fit_box_matches_the_image_ratio_and_stays_inside() {
        let font = FontSize::new(10, 20);
        let area = Rect::new(0, 0, 34, 11);

        // 方图：列 = 行 × 1（图） × 2（字符高宽比） = 22，水平居中
        let square = fit_box(&solid(100, 100), area, font);
        assert_eq!((square.width, square.height), (22, 11));
        assert_eq!(square.x, (34 - 22) / 2, "居中");
        assert_eq!(square.y, 0);

        // 16:9 的横图：列 = 11 × 1.778 × 2 ≈ 39，比区域宽 → 反过来按宽算行
        let wide = fit_box(&solid(160, 90), area, font);
        assert!(wide.width <= area.width && wide.height <= area.height);
        assert!(wide.width > square.width, "横图应该更宽");
    }

    /// 三种模式都不该让图溢出区域——溢出会被 ratatui 裁掉，看起来就是「图缺了一块」。
    #[test]
    fn every_mode_stays_within_the_area() {
        let font = FontSize::new(10, 20);
        let area = Rect::new(0, 0, 34, 11);
        for mode in [CoverFill::Crop, CoverFill::Stretch, CoverFill::Fit] {
            let (image, render) = prepare_cover(&solid(256, 256), area, font, mode);
            assert!(
                render.width <= area.width && render.height <= area.height,
                "{mode:?} 溢出区域：{render:?} 不在 {area:?} 里"
            );
            assert!(
                image.width() > 0 && image.height() > 0,
                "{mode:?} 不该产生空图"
            );
        }
    }

    /// 端到端：封面真的画进了 ratatui 的 Buffer。
    ///
    /// 用 `TestBackend` 就是为了能拿到绘制后的 Buffer——「有没有偷偷写 stdout」
    /// 在真实终端上根本无从断言，而那正是之前卡死与闪烁的来源。
    /// 这里选 `Picker::halfblocks()`：它不碰 stdio，测试里能稳定跑。
    #[test]
    fn cover_is_rendered_into_the_frame_buffer() {
        // 渐变而不是纯色：半块字符在上下两格同色时会退化成空格（用底色表示），
        // 纯色图渲染出来就是一片空白，测不出东西。
        let mut pixels = image::RgbImage::new(32, 32);
        for (x, y, pixel) in pixels.enumerate_pixels_mut() {
            *pixel = image::Rgb([(x * 8) as u8, (y * 8) as u8, 128]);
        }

        let mut state = AppState::new(crate::config::Config::default());
        state.picker = Some(ratatui_image::picker::Picker::halfblocks());
        state.cover.set_image(
            "test-hash".to_string(),
            image::DynamicImage::ImageRgb8(pixels),
            1.0,
        );

        let area = Rect::new(0, 0, 60, 20);
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).expect("测试后端可用");
        let mut rest = Rect::default();
        let drawn = terminal
            .draw(|frame| {
                rest = draw_cover_block(frame, area, &mut state, CoverPlace::AboveContent);
            })
            .expect("绘制成功");

        assert_eq!(rest.y, 11, "下方内容从缩略图（10 行 + 1 行间距）之后开始");

        // 落在封面区里的非空格单元格：全是空格就说明图根本没进去
        let painted = drawn
            .buffer
            .content()
            .iter()
            .filter(|cell| cell.symbol() != " ")
            .count();
        assert!(painted > 0, "封面应当写进 Buffer，而不是 stdout");
    }

    /// 铺满模式画出来的格子必须**严格多于**「完整显示」——多出来的正是原先空着的那条。
    ///
    /// 用差分而不是绝对像素值：半块字符在上下同色时会退化成空格（用底色表示），
    /// 「某个格子是不是空格」并不可靠，但「哪种模式覆盖得更广」是可靠的：
    /// `Fit` 只能画进 22 列，`Crop` 铺满 34 列。
    #[test]
    fn fill_mode_paints_more_than_fit_mode() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut pixels = image::RgbImage::new(256, 256);
        for (x, y, pixel) in pixels.enumerate_pixels_mut() {
            *pixel = image::Rgb([(x * 7) as u8, (y * 7) as u8, 128]);
        }

        // 34 × 11 就是宽屏首页左栏那块封面的真实尺寸：像素比例约 1.5:1，而封面是方图
        let area = Rect::new(0, 0, 34, 11);

        let painted = |mode: CoverFill| {
            let mut state = AppState::new(crate::config::Config::default());
            state.picker = Some(ratatui_image::picker::Picker::halfblocks());
            state.config.cover_fill = mode;
            state.cover.set_image(
                "h".to_string(),
                image::DynamicImage::ImageRgb8(pixels.clone()),
                1.0,
            );

            let mut terminal = Terminal::new(TestBackend::new(34, 11)).expect("测试后端可用");
            let drawn = terminal
                .draw(|frame| {
                    draw_cover_block(frame, area, &mut state, CoverPlace::Fill);
                })
                .expect("绘制成功");
            drawn
                .buffer
                .content()
                .iter()
                .filter(|cell| cell.symbol() != " ")
                .count()
        };

        let fitted = painted(CoverFill::Fit);
        let cropped = painted(CoverFill::Crop);
        assert!(
            cropped > fitted,
            "铺满模式应当比完整显示覆盖得更广：crop={cropped} fit={fitted}"
        );
    }

    /// 端到端复现：**宽而矮**的终端 + 「不变形」铺满方式。
    #[test]
    fn render_home_in_a_short_wide_area_does_not_panic() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        for height in 4..=12u16 {
            let mut state = AppState::new(crate::config::Config::default());
            state.picker = Some(ratatui_image::picker::Picker::halfblocks());
            state.config.cover_fill = CoverFill::Fit;
            state.current = Some(crate::api::model::Song::default());
            state.cover.set_image(
                "x".to_string(),
                image::DynamicImage::ImageRgb8(image::RgbImage::new(64, 64)),
                1.0,
            );

            let area = Rect::new(0, 0, 62, height);
            let theme =
                crate::ui::theme::Theme::for_config(crate::ui::theme::ThemeName::default(), false);
            let mut terminal =
                Terminal::new(TestBackend::new(62, height.max(1))).expect("测试后端可用");
            terminal
                .draw(|frame| render_home(frame, area, &mut state, &theme))
                .unwrap_or_else(|error| panic!("{height} 行时渲染失败：{error}"));
        }
    }

    /// 零高度 / 零宽度的封面区不能 panic。
    ///
    /// **回归测试**：`fit_box` 里那句 `rows.clamp(1, area.height)` 在
    /// `area.height == 0` 时会炸——`Ord::clamp` 要求 `min <= max`，
    /// 而 `clamp(1, 0)` 直接 `assert!(min <= max)`。触发路径很普通：
    /// 终端**宽而矮**（比如 100×8）时，宽屏分支要求 `inner.width >= 60`，
    /// 而高度那一路 `Layout::vertical([Min(6), Length(ACCOUNT_HEIGHT)])` 在空间
    /// 不够时会把封面那一栏压到 0 行，`panel().inner()` 再减掉边框就还是 0。
    ///
    /// 另外两种铺满方式（`Crop` / `Stretch`）走 `pixel_size`，那里有 `.max(1)`，
    /// 所以只有 `Fit` 会炸——也正是用户得先在设置里选「不变形」才会撞上，
    /// 默认的 `Crop` 不会。
    #[test]
    fn fit_box_tolerates_a_degenerate_area() {
        let image = image::DynamicImage::ImageRgb8(image::RgbImage::new(64, 64));
        let font = FontSize::new(10, 20);

        // 高度为 0：修复前这里 panic
        let _ = fit_box(&image, Rect::new(0, 0, 34, 0), font);
        // 宽度为 0
        let _ = fit_box(&image, Rect::new(0, 0, 0, 11), font);
        // 两者都是 0
        let _ = fit_box(&image, Rect::new(0, 0, 0, 0), font);

        // 走完整路径也要安全（`Fit` 是唯一会碰 `fit_box` 的分支）
        for (width, height) in [(34, 0), (0, 11), (0, 0)] {
            let (_, area) =
                prepare_cover(&image, Rect::new(0, 0, width, height), font, CoverFill::Fit);
            assert!(
                area.width <= width && area.height <= height,
                "算出来的框不能超出区域：{area:?} 不在 {width}x{height} 里"
            );
        }
    }

    /// 区域变了才重新编码；区域不变时**一帧都不重编**。
    ///
    /// 之前的病根就是每帧重发——474KB 的转义序列堵死 stdout。这里直接断言
    /// 「第二帧没有任何编码动作」：不编码就没有新的图片数据，ratatui 的 diff
    /// 也就无从输出，自然不会阻塞写入、也不会闪。
    /// 这里直接驱动 widget 而不是走 `draw_cover_block`：后者为了记日志会把
    /// `last_encoding_result()` 取走（它是 `take()` 语义），读不到编码次数。
    #[test]
    fn cover_is_encoded_only_when_the_area_changes() {
        use ratatui::buffer::Buffer;
        use ratatui::widgets::StatefulWidget;

        let mut pixels = image::RgbImage::new(256, 256);
        for (x, y, pixel) in pixels.enumerate_pixels_mut() {
            *pixel = image::Rgb([x as u8, y as u8, 128]);
        }
        let mut protocol = ratatui_image::picker::Picker::halfblocks()
            .new_resize_protocol(image::DynamicImage::ImageRgb8(pixels));

        // 交给 widget 的区域由纯函数算出，因此连续两帧必然是同一个矩形——
        // 区域稳定是「不重发」的前提
        let (first_area, _) = thumbnail_layout(Rect::new(0, 0, 60, 20), 1.0, 2.0).expect("够放");
        let (second_area, _) = thumbnail_layout(Rect::new(0, 0, 60, 20), 1.0, 2.0).expect("够放");
        assert_eq!(first_area, second_area);

        let mut first = Buffer::empty(first_area);
        StatefulImage::default().render(first_area, &mut first, &mut protocol);
        assert!(
            protocol.last_encoding_result().is_some(),
            "首帧必须编码一次"
        );

        let mut second = Buffer::empty(second_area);
        StatefulImage::default().render(second_area, &mut second, &mut protocol);
        assert!(
            protocol.last_encoding_result().is_none(),
            "区域没变就不该再编码——每帧编码正是之前卡死的原因"
        );
        assert_eq!(
            first, second,
            "两帧内容一致 → ratatui 的 diff 一个字节都不会输出"
        );

        // 换到更大的区域才重新编码一次：切页 / 改窗口大小走的就是这条路
        let (bigger, _) = thumbnail_layout(Rect::new(0, 0, 60, 40), 1.0, 2.0).expect("够放");
        assert_ne!(bigger.height, first_area.height);
        let mut third = Buffer::empty(bigger);
        StatefulImage::default().render(bigger, &mut third, &mut protocol);
        assert!(
            protocol.last_encoding_result().is_some(),
            "区域变了应当重新编码一次"
        );
    }

    /// `CoverArt::fit_to` 的缓存语义：区域不变时协议被复用，**一帧都不重编**；
    /// 区域一变就必须重编。
    ///
    /// 重编意味着重新裁图 + 重新编码（几百 KB 的数据），每帧都做就是之前卡死的
    /// 那条路。这里必须真的渲染一次才能读到编码结果——`last_encoding_result()`
    /// 是 widget 在 `resize_encode_render` 里填的，光建协议不算编码。
    #[test]
    fn fit_to_reuses_the_protocol_while_the_area_is_stable() {
        use ratatui::buffer::Buffer;
        use ratatui::widgets::StatefulWidget;

        let picker = ratatui_image::picker::Picker::halfblocks();
        let mut cover = crate::app::state::CoverArt::default();
        cover.set_image("h".to_string(), solid(256, 256), 1.0);

        // 画一帧，返回这一帧是否发生了编码
        fn draw(
            cover: &mut crate::app::state::CoverArt,
            area: Rect,
            picker: &ratatui_image::picker::Picker,
        ) -> bool {
            let mut buffer = Buffer::empty(area);
            let (protocol, render) = cover.fit_to(CoverFill::Crop, area, picker).expect("有原图");
            assert_eq!(render, area, "裁剪模式的渲染区就是请求区");
            StatefulImage::default().resize(Resize::Scale(None)).render(
                render,
                &mut buffer,
                protocol,
            );
            protocol.last_encoding_result().is_some()
        }

        let area = Rect::new(0, 0, 34, 11);
        assert!(draw(&mut cover, area, &picker), "首帧必须编码一次");
        assert!(
            !draw(&mut cover, area, &picker),
            "区域没变就不该再编码——每帧编码正是之前卡死的原因"
        );

        // 换区域 → 必须重编，否则图还是旧尺寸、右下留空
        let wider = Rect::new(0, 0, 60, 11);
        assert!(
            draw(&mut cover, wider, &picker),
            "区域变了必须重新编码，否则图还是旧尺寸、填不满"
        );
    }

    /// 没有原图时 `fit_to` 返回 `None`——调用方据此留白，而不是画出半张图。
    #[test]
    fn fit_to_yields_nothing_without_a_source_image() {
        let picker = ratatui_image::picker::Picker::halfblocks();
        let mut cover = crate::app::state::CoverArt::default();
        assert!(!cover.is_drawable());
        assert!(
            cover
                .fit_to(CoverFill::Crop, Rect::new(0, 0, 34, 11), &picker)
                .is_none()
        );
    }

    // ---- 逐字歌词（仿 Apple Music）----
    //
    // 配色数学有 `theme::mix` 的单测兜着，但那只证明「插值算得对」，
    // 证明不了它**真的画到了屏幕上**。下面两条走 TestBackend 读真实 Buffer。

    /// 造一行 5 个字的歌词，每个字占 200ms，从 `time_ms` 开始。
    fn line_with_words(time_ms: u64, text: &str) -> crate::api::model::LyricLine {
        let words = (0..text.chars().count() as u64)
            .map(|i| crate::api::model::LyricWord {
                start_ms: time_ms + i * 200,
                end_ms: time_ms + 200 + i * 200,
            })
            .collect();
        crate::api::model::LyricLine {
            time_ms,
            text: text.to_string(),
            words,
            ..Default::default()
        }
    }

    /// 渲染一次歌词，返回绘制后的 Buffer。
    fn render_lyric_into(
        state: &mut AppState,
        width: u16,
        height: u16,
        theme: &Theme,
    ) -> ratatui::buffer::Buffer {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("测试后端可用");
        terminal
            .draw(|frame| render_lyric(frame, Rect::new(0, 0, width, height), state, theme))
            .expect("绘制成功");
        terminal.backend().buffer().clone()
    }

    /// 某个字符格的前景色。
    fn fg_at(buffer: &ratatui::buffer::Buffer, x: u16, y: u16) -> ratatui::style::Color {
        buffer
            .cell((x, y))
            .map(|cell| cell.fg)
            .expect("坐标在 Buffer 内")
    }

    /// 该行第一个有字的列（左起）。
    ///
    /// 比手算居中偏移可靠：水平居中是 `Paragraph` 按它的规则取整的，测试去猜
    /// 那个取整方式只会写出「差一列」的脆弱断言（这条最初就踩了）。
    ///
    /// 只扫内区：最左最右两列是面板边框，边框也有前景色，从 0 扫会先撞上它。
    fn first_text_column(buffer: &ratatui::buffer::Buffer, row: u16, width: u16) -> u16 {
        (1..width.saturating_sub(1))
            .find(|x| fg_at(buffer, *x, row) != ratatui::style::Color::Reset)
            .expect("这一行应当有歌词")
    }

    /// 感知亮度，用来断言「这一档比那一档亮」。
    fn luma(color: ratatui::style::Color) -> f32 {
        let ratatui::style::Color::Rgb(r, g, b) = color else {
            panic!("真彩主题不该出现非 Rgb 颜色：{color:?}");
        };
        0.299 * f32::from(r) + 0.587 * f32::from(g) + 0.114 * f32::from(b)
    }

    /// 当前行的颜色必须**沿着字逐个变亮**，而不是整行一个色或硬跳两档。
    ///
    /// 播放位置卡在第 2 个字中间，于是 5 个字应当拿到 3 档颜色：
    /// 已唱的（强调色）、正在唱的那个（插值出来的中间色）、还没唱的（底色）。
    #[test]
    fn active_line_ramps_from_pending_to_sung() {
        let theme = Theme::for_config(ThemeName::Default, false);
        let mut state = AppState::new(crate::config::Config::default());
        // 行数必须**多于**视口，居中定位才会生效——歌词行不够时 Paragraph 是顶对齐
        // 的，当前行根本不在中间（这条最初就踩了，断言全读到空格子）
        state.lyric.lyric = crate::api::model::Lyric {
            text: String::new(),
            lines: (0..15)
                .map(|_| line_with_words(1000, "一二三四五"))
                .collect(),
        };
        state.lyric.active_line = Some(7);
        // 第 1 个字（1200~1400）唱到一半
        state.position_ms = 1300;

        let buffer = render_lyric_into(&mut state, 40, 9, &theme);

        // 40×9 → 面板内区 (1,1,38,7)；15 行里偏移 4，当前行落在内区第 3 行
        let row = 1 + 3;
        let start = first_text_column(&buffer, row, 40);

        // 汉字是**双宽**的：每个字占 2 列，紧跟的那一格是续格、前景色是默认值。
        // 所以按 2 列步进取「第几个字」，按 1 列走会一半落在续格上。
        let colors: Vec<_> = (0..5).map(|i| fg_at(&buffer, start + i * 2, row)).collect();

        assert_eq!(colors[0], theme.accent, "唱完的字应当是强调色");
        assert_eq!(
            colors[4],
            mix(theme.text_dim, theme.text, LYRIC_ACTIVE_BASE),
            "没唱的字应当是底色"
        );

        let (sung, mid, pending) = (luma(colors[0]), luma(colors[1]), luma(colors[4]));
        assert!(
            pending < mid && mid < sung,
            "亮度必须逐个递增：未唱 {pending:.0} < 正在唱 {mid:.0} < 已唱 {sung:.0}"
        );

        let distinct: std::collections::BTreeSet<_> =
            colors.iter().map(|c| format!("{c:?}")).collect();
        assert_eq!(
            distinct.len(),
            3,
            "应当是「已唱 / 中间 / 未唱」三档，实际 {distinct:?}"
        );
    }

    /// 非当前行按离当前行的距离**线性变暗**，远处的行必须比近处的暗。
    #[test]
    fn distant_lines_fade_out() {
        let theme = Theme::for_config(ThemeName::Default, false);
        let mut state = AppState::new(crate::config::Config::default());
        state.lyric.lyric = crate::api::model::Lyric {
            text: String::new(),
            lines: (0..21)
                .map(|_| line_with_words(1000, "一二三四五"))
                .collect(),
        };
        state.lyric.active_line = Some(10);
        state.position_ms = 1300;

        let buffer = render_lyric_into(&mut state, 40, 13, &theme);

        // 40×13 → 面板内区 (1,1,38,11)；21 行里偏移 5，当前行落在内区第 5 行
        let center = 1 + 5;
        let start = first_text_column(&buffer, center, 40);
        let active = fg_at(&buffer, start, center);
        let near = fg_at(&buffer, start, center + 1);
        let far = fg_at(&buffer, start, center + 5);

        assert!(
            luma(near) < luma(active),
            "紧邻的那一行必须比当前行暗（当前行才突出）"
        );
        assert!(
            luma(far) < luma(near),
            "越远的行必须越暗：远 {:.0} 应当低于近 {:.0}",
            luma(far),
            luma(near)
        );
        assert_eq!(
            near,
            mix(theme.text_dim, theme.lyric_far, fade_ratio(1.0)),
            "距离 1 应当正好是 idle 色（比例 0）"
        );
        assert_eq!(
            far,
            mix(theme.text_dim, theme.lyric_far, 1.0),
            "距离超过跨度后应当到底色"
        );
    }

    /// 淡出比例：距离 1 不淡，超过跨度封顶。
    #[test]
    fn fade_ratio_starts_at_zero_and_caps() {
        assert_eq!(fade_ratio(1.0), 0.0, "紧邻当前行的那一行保持 idle 色");
        assert_eq!(
            fade_ratio(0.0),
            0.0,
            "当前行自身（不该走到这里）也不能出负数"
        );
        assert!(fade_ratio(2.0) > 0.0 && fade_ratio(2.0) < 1.0);
        assert_eq!(fade_ratio(100.0), 1.0, "再远也封顶，不能溢出");
    }

    /// 过渡期间距离是插值出来的小数，比例必须**连续**——不能像整数版那样
    /// 在 `d = 1` 处从 0 跳到 0.25 而中间没有过渡。
    #[test]
    fn fade_ratio_is_continuous_over_fractional_distances() {
        let mut previous = fade_ratio(0.0);
        for step in 1..=200 {
            let distance = step as f32 / 20.0;
            let current = fade_ratio(distance);
            assert!(
                current >= previous,
                "比例必须随距离单调不减：d={distance} 时 {current} < {previous}"
            );
            assert!(
                current - previous < 0.05,
                "相邻采样之间不该有跳变：d={distance} 时涨了 {}",
                current - previous
            );
            previous = current;
        }
    }

    /// 歌词真的画出来时回填 `lyric_visible`，否则 `App` 不会为逐字推进提速。
    #[test]
    fn rendering_marks_the_lyric_as_visible() {
        let theme = Theme::for_config(ThemeName::Default, false);
        let mut state = AppState::new(crate::config::Config::default());
        state.lyric.lyric = crate::api::model::Lyric {
            text: String::new(),
            lines: vec![line_with_words(1000, "一二三四五")],
        };
        state.lyric.active_line = Some(0);

        state.lyric_visible = false;
        render_lyric_into(&mut state, 40, 9, &theme);
        assert!(state.lyric_visible, "画出歌词后应当置位，否则逐字不会提速");

        // 没有歌词时只画占位提示，不算「可见」——不该为它提速
        state.lyric.lyric = crate::api::model::Lyric::default();
        state.lyric_visible = false;
        render_lyric_into(&mut state, 40, 9, &theme);
        assert!(!state.lyric_visible, "只有占位提示时不该置位");
    }

    // ---- 换行过渡 ----

    /// 一段 15 行的歌词，行距 `gap_ms`。行数必须**多于视口**，居中定位才生效。
    fn many_lines(count: usize, gap_ms: u64) -> crate::api::model::Lyric {
        crate::api::model::Lyric {
            text: String::new(),
            lines: (0..count)
                .map(|index| line_with_words(index as u64 * gap_ms, "一二三四五"))
                .collect(),
        }
    }

    /// 40×9 的歌词面板：内区 7 行，当前行居中落在内区第 4 行（屏幕第 4 行）。
    /// 显示行 `offset + 3` 对应当前行，所以当前行的屏幕行号恒为 `1 + 3`。
    const ACTIVE_ROW: u16 = 4;

    /// 换行过渡必须**两头都对**：旧行变暗、新行变亮。
    ///
    /// 这条是这次改动的核心断言。终端没有子单元格定位，位置动不了，能动的只有
    /// 颜色——所以「有没有动画」等价于「换行时那两行的颜色有没有随时间变」。
    #[test]
    fn line_change_cross_fades_between_the_two_lines() {
        let theme = Theme::for_config(ThemeName::Default, false);
        let mut state = AppState::new(crate::config::Config::default());
        state.lyric.lyric = many_lines(15, 1_000);
        // 第 1 个字唱完（1300 > 该字的 end_ms 1200），取色稳定，不受进度抖动影响
        state.position_ms = 1_300;

        // 换行瞬间：高亮还在**旧**行（第 6 句），新行（第 7 句）还没亮起来
        state.lyric.active_line = Some(6);
        state.lyric.retarget(Some(7), 200.0);
        let start = render_lyric_into(&mut state, 40, 9, &theme);
        let old_at_start = fg_at(
            &start,
            first_text_column(&start, ACTIVE_ROW - 1, 40),
            ACTIVE_ROW - 1,
        );
        let new_at_start = fg_at(
            &start,
            first_text_column(&start, ACTIVE_ROW, 40),
            ACTIVE_ROW,
        );

        // 走完：高亮移到**新**行
        state
            .lyric
            .advance_transition(std::time::Duration::from_millis(200));
        let end = render_lyric_into(&mut state, 40, 9, &theme);
        let old_at_end = fg_at(
            &end,
            first_text_column(&end, ACTIVE_ROW - 1, 40),
            ACTIVE_ROW - 1,
        );
        let new_at_end = fg_at(&end, first_text_column(&end, ACTIVE_ROW, 40), ACTIVE_ROW);

        assert!(
            luma(old_at_start) > luma(new_at_start),
            "换行瞬间高亮还应当在旧行上：旧 {:.0} 应当高于新 {:.0}",
            luma(old_at_start),
            luma(new_at_start)
        );
        assert!(
            luma(old_at_end) < luma(new_at_end),
            "走完之后高亮应当已经在新行上：旧 {:.0} 应当低于新 {:.0}",
            luma(old_at_end),
            luma(new_at_end)
        );
        assert!(
            luma(old_at_end) < luma(old_at_start),
            "旧行必须**随时间变暗**：{:.0} → {:.0}",
            luma(old_at_start),
            luma(old_at_end)
        );
        assert!(
            luma(new_at_end) > luma(new_at_start),
            "新行必须**随时间变亮**：{:.0} → {:.0}",
            luma(new_at_start),
            luma(new_at_end)
        );
    }

    /// 过渡中间那一帧，两行都该是**部分点亮**——这是「交叉淡化」与「硬切」的区别。
    #[test]
    fn mid_transition_leaves_both_lines_partially_lit() {
        let theme = Theme::for_config(ThemeName::Default, false);
        let mut state = AppState::new(crate::config::Config::default());
        state.lyric.lyric = many_lines(15, 1_000);
        state.position_ms = 1_300;
        state.lyric.active_line = Some(6);
        state.lyric.retarget(Some(7), 200.0);

        let settled_old = {
            let mut plain = AppState::new(crate::config::Config::default());
            plain.lyric.lyric = many_lines(15, 1_000);
            plain.position_ms = 1_300;
            plain.lyric.active_line = Some(6);
            let buffer = render_lyric_into(&mut plain, 40, 9, &theme);
            fg_at(
                &buffer,
                first_text_column(&buffer, ACTIVE_ROW, 40),
                ACTIVE_ROW,
            )
        };

        state
            .lyric
            .advance_transition(std::time::Duration::from_millis(100));
        let mid = render_lyric_into(&mut state, 40, 9, &theme);
        let old_mid = fg_at(
            &mid,
            first_text_column(&mid, ACTIVE_ROW - 1, 40),
            ACTIVE_ROW - 1,
        );

        assert!(
            luma(old_mid) < luma(settled_old),
            "中途的旧行应当已经比它满亮时暗：中途 {:.0} vs 满亮 {:.0}",
            luma(old_mid),
            luma(settled_old)
        );
        assert!(
            luma(old_mid)
                > luma(fg_at(
                    &mid,
                    first_text_column(&mid, ACTIVE_ROW + 2, 40),
                    ACTIVE_ROW + 2
                )),
            "但要比更远处那些完全没被点亮的行亮"
        );
    }

    /// 过渡走完之后，渲染结果必须与「直接设 active_line」**逐格相同**。
    ///
    /// 这条是既有那批配色断言的护栏：过渡状态不能污染稳态路径，否则
    /// `active_line_ramps_from_pending_to_sung` / `distant_lines_fade_out` 会在
    /// 未来的某次改动里悄悄失效。
    #[test]
    fn settled_transition_renders_identically_to_a_plain_line_change() {
        let theme = Theme::for_config(ThemeName::Default, false);

        let mut plain = AppState::new(crate::config::Config::default());
        plain.lyric.lyric = many_lines(15, 1_000);
        plain.position_ms = 1_300;
        plain.lyric.active_line = Some(7);
        let expected = render_lyric_into(&mut plain, 40, 9, &theme);

        let mut through_transition = AppState::new(crate::config::Config::default());
        through_transition.lyric.lyric = many_lines(15, 1_000);
        through_transition.position_ms = 1_300;
        through_transition.lyric.active_line = Some(6);
        through_transition.lyric.retarget(Some(7), 200.0);
        through_transition
            .lyric
            .advance_transition(std::time::Duration::from_millis(500));
        let actual = render_lyric_into(&mut through_transition, 40, 9, &theme);

        for row in 0..9 {
            for column in 0..40 {
                assert_eq!(
                    fg_at(&expected, column, row),
                    fg_at(&actual, column, row),
                    "({column}, {row}) 处颜色不同：过渡走完后必须与稳态逐格相同"
                );
            }
        }
    }

    /// 歌词行要登记命中区，并回填「显示行 → 歌词行」映射——点击跳转全靠这两个。
    #[test]
    fn lyric_lines_register_a_click_zone_and_a_row_mapping() {
        let theme = Theme::for_config(ThemeName::Default, false);
        let mut state = AppState::new(crate::config::Config::default());
        // 每行都带译文 → 显示行是歌词行的两倍，两者不再一一对应
        let mut lyric = many_lines(15, 1_000);
        for line in &mut lyric.lines {
            line.translation = Some("译文".to_string());
        }
        state.lyric.lyric = lyric;
        state.lyric.active_line = Some(7);
        state.position_ms = 1_300;

        state.begin_frame();
        render_lyric_into(&mut state, 40, 9, &theme);

        assert_eq!(
            state.lyric.display_line_index.len(),
            30,
            "15 句 × 2（原文 + 译文）"
        );
        assert_eq!(state.lyric.display_line_index[0], 0);
        assert_eq!(
            state.lyric.display_line_index[1], 0,
            "译文行仍属于第 0 句——点它也该跳到第 0 句"
        );
        assert_eq!(state.lyric.display_line_index[2], 1);

        let zone = state
            .hit_test(5, ACTIVE_ROW)
            .expect("歌词区应当登记了命中区");
        assert!(
            matches!(zone.target, crate::app::state::HitTarget::LyricLine),
            "点歌词不该被当成点别的东西"
        );
        assert_eq!(zone.len, 30, "命中区要覆盖全部显示行，而不只是这一屏");
        // 命中区记的是**显示行**下标，所以顶部那一行对应 offset，而不是 0
        assert_eq!(
            zone.index_at(zone.rect.top()),
            Some(zone.first_index),
            "屏幕第一行应当对应 offset 处那一行"
        );
        assert_eq!(
            state.lyric.line_index_at_display(zone.first_index),
            Some(5),
            "30 条显示行、当前行第 7 句居中 → 偏移 11 → 屏幕首行是第 5 句的译文"
        );
    }

    /// 没有歌词时只画占位提示，**不该**登记命中区——否则点空白处会跳到奇怪的时间。
    #[test]
    fn placeholder_does_not_register_a_click_zone() {
        let theme = Theme::for_config(ThemeName::Default, false);
        let mut state = AppState::new(crate::config::Config::default());
        state.lyric.lyric = crate::api::model::Lyric::default();

        state.begin_frame();
        render_lyric_into(&mut state, 40, 9, &theme);

        assert!(state.hit_test(5, ACTIVE_ROW).is_none(), "占位提示不该可点");
        assert!(state.lyric.display_line_index.is_empty());
    }

    /// 末屏（`offset` 已撞上 `max_offset`）点击必须落在**屏幕行下半段**。
    ///
    /// 这是「听后半首歌时点歌词总是跳到别的句」的直接回归：内容型对齐之后
    /// `offset ≠ 可视首行`，老的「`rect.top()` = 面板上沿」会把整块命中区
    /// 下移 `max_offset - offset`（这块区域里是 3 行），点屏幕第 k 行得到的是
    /// 第 k − 3 行的歌词。
    #[test]
    fn late_song_clicks_do_not_shift_by_the_offset_gap() {
        let theme = Theme::for_config(ThemeName::Default, false);
        let mut state = AppState::new(crate::config::Config::default());
        state.lyric.lyric = many_lines(15, 1_000);
        // 最后一句：focus_display = 14，viewport = 7，max_offset = 8
        state.lyric.active_line = Some(14);
        state.position_ms = 1_300;

        state.begin_frame();
        render_lyric_into(&mut state, 40, 9, &theme);

        let zone = state
            .hit_test(5, ACTIVE_ROW)
            .expect("歌词区应当登记了命中区");
        assert_eq!(
            zone.first_index, 8,
            "末屏的 offset 停在 max_offset = 8（内容型对齐），不是 11"
        );
        // 屏幕最底那一行（区域 0..7 的第 6 行）显示的是第 14 句，也就是最后一句
        let bottom = zone.rect.bottom() - 1;
        let display = zone.index_at(bottom).expect("最底一行仍在命中区内");
        assert_eq!(display, 14, "最底一行应当就是当前（最后）一句");
        assert_eq!(
            state.lyric.line_index_at_display(display),
            Some(14),
            "点最底一行要跳到第 14 句——错位时这里会得到第 11 句"
        );
        assert_eq!(
            zone.rect.top(),
            1,
            "区域 y=0、边框 1 行 → 内容区从第 1 行起（`block.inner` 已扣边框）"
        );
        assert_eq!(zone.rect.bottom(), 8, "命中区覆盖可视的 7 行，不含底部边框");
    }

    /// 命中区的上沿必须锚在**内容区**：`render_lyric` 自己画边框，所以
    /// 内容首行 = `area.y + 1`；命中区上沿必须正好是它，多一行少一行都会
    /// 让「点第 N 行」落到隔壁句。
    #[test]
    fn hit_zone_top_anchors_to_the_first_content_row() {
        let theme = Theme::for_config(ThemeName::Default, false);
        let mut state = AppState::new(crate::config::Config::default());
        state.lyric.lyric = many_lines(15, 1_000);
        state.lyric.active_line = Some(7);
        state.position_ms = 1_300;

        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        // 内容区上沿 26（外层已扣边框）
        let content_top = 26u16;
        let mut terminal = Terminal::new(TestBackend::new(60, 40)).expect("测试后端可用");
        let mut zone_top = None;
        terminal
            .draw(|frame| {
                render_lyric(frame, Rect::new(0, content_top, 40, 9), &mut state, &theme);
                zone_top = state
                    .hit_test(5, content_top + 4)
                    .map(|zone| zone.rect.top());
            })
            .expect("绘制成功");

        assert_eq!(
            zone_top,
            Some(content_top + 1),
            "`render_lyric` 自己画边框：内容区上沿 = area.y + 1，命中区必须与之一致"
        );
    }

    /// 诊断用探针：模拟「连续换歌 → 换封面」，逐轮打印 RSS。
    ///
    /// 不做断言——RSS 受分配器行为影响，跨平台不可靠，写死一个阈值只会变成
    /// 定时炸弹。它的用途是**拿数据**，区分三种情况：
    ///
    /// * RSS 稳定在峰值 → 没有泄漏，只是分配器不还页；
    /// * RSS 逐轮单调上升且不回落 → 有东西被长期持有（真泄漏）；
    /// * 上升一段后停住 → 碎片封顶。
    ///
    /// ```bash
    /// cargo test --release cover_swap_rss_probe -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "诊断用，靠 --ignored 手动跑"]
    fn cover_swap_rss_probe() {
        /// 读 `/proc/self/statm` 的第二个字段（驻留页数），换算成 KiB。
        fn rss_kib() -> u64 {
            let text = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
            let pages: u64 = text
                .split_whitespace()
                .nth(1)
                .and_then(|value| value.parse().ok())
                .unwrap_or(0);
            pages * 4
        }

        use ratatui::buffer::Buffer;
        use ratatui::widgets::StatefulWidget;

        let picker = ratatui_image::picker::Picker::halfblocks();
        let mut state = AppState::new(crate::config::Config::default());
        let area = Rect::new(0, 0, 60, 20);
        let mut buffer = Buffer::empty(area);

        println!("轮次  RSS(KiB)");
        for round in 0..40u8 {
            // 480×480 的 RGB 位图 = 691 KB，与酷狗封面同一量级
            let pixels = image::RgbImage::from_fn(480, 480, |x, y| {
                image::Rgb([(x % 256) as u8, (y % 256) as u8, round])
            });
            state.cover.set_image(
                format!("hash-{round}"),
                image::DynamicImage::ImageRgb8(pixels),
                1.0,
            );
            if let Some((protocol, render)) = state.cover.fit_to(CoverFill::Crop, area, &picker) {
                StatefulImage::default().resize(Resize::Scale(None)).render(
                    render,
                    &mut buffer,
                    protocol,
                );
            }
            println!("{round:>4} {}", rss_kib());
        }
    }
}
