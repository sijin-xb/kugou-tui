//! 列表类视图：歌曲列表、条目列表、搜索输入框。
//!
//! 所有列表都走同一条渲染路径（`List` + `ListState`），因此滚动行为、选中高亮、
//! 滚动条在五个标签页里完全一致——用户学一次就够了。

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{ListItem, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState};

use crate::api::model::{Artist, Playlist, RankBoard};
use crate::app::state::{EntryList, SearchPane, SongList};
use crate::app::update::{artist_subtitle, playlist_subtitle, rank_subtitle};
use crate::audio::engine::PlaybackState;
use crate::keymap::key_hint_for;
use crate::ui::theme::Theme;
use crate::ui::views::{empty_placeholder, failed_placeholder, loading_placeholder};
use crate::ui::widgets::{
    RowContext, display_width, entry_row, panel, row_is_visible, selection_list, song_row,
    truncate_to_width,
};

/// 面板太小时直接跳过绘制。
///
/// ratatui 对 0 尺寸区域是安全的，但边框加内容至少需要 3 行 10 列才有意义；
/// 提前返回也能省掉无谓的字符串构造。
fn too_small(area: Rect) -> bool {
    area.height < 3 || area.width < 10
}

/// 歌曲列表。五个标签页的「下半屏」都用它。
/// 歌曲列表的视图参数。收成结构与 `QueueView` 保持一致，避免参数个数触发 clippy。
pub struct SongView<'a> {
    pub focused: bool,
    pub current_hash: Option<&'a str>,
    pub playback: PlaybackState,
    /// 鼠标位置（列, 行）。用于算出悬停行（渲染函数内部换算，所以这里存原始坐标）。
    pub pointer: Option<(u16, u16)>,
}

pub fn render_song_list(
    frame: &mut Frame,
    area: Rect,
    list: &mut SongList,
    view: SongView,
    theme: &Theme,
) {
    if too_small(area) {
        return;
    }

    let title = if list.title.is_empty() {
        "歌曲".to_string()
    } else {
        list.title.clone()
    };

    let block = panel(title, view.focused, theme);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if list.load.is_loading() {
        frame.render_widget(loading_placeholder(theme), inner);
        return;
    }
    // 失败态优先于空态：没取到和「本来就是空的」是两回事
    //
    // 提示里的键名走 `key_hint_for`：刷新默认是 `R`，但用户可能在 `[keymap]`
    // 里改成了别的键（例如 `r`）。写死的话就会叫用户去按一个已经无效的键。
    if let Some(reason) = list.load.error() {
        frame.render_widget(
            failed_placeholder(
                reason,
                Some(&format!("按 {} 重试", key_hint_for("R"))),
                theme,
            ),
            inner,
        );
        return;
    }
    if list.songs.is_empty() {
        frame.render_widget(empty_placeholder(list.empty_text(), theme), inner);
        return;
    }

    // 右侧留 1 列给滚动条
    let row_width = inner.width.saturating_sub(1) as usize;
    // 可见行数（供 `row_is_visible` 判断哪些行值得真正构造）
    let visible_rows = inner.height as usize;

    // 把鼠标位置换算成「数据行下标」：List 内部按 cursor.offset() 滚动，
    // 屏幕第 r 行对应的是 offset + r。
    let hover_row = view.pointer.and_then(|(column, row)| {
        if row < inner.y
            || row >= inner.y + inner.height
            || column < inner.x
            || column >= inner.x + inner.width
        {
            return None;
        }
        let visible = (row - inner.y) as usize + list.cursor.offset();
        (visible < list.songs.len()).then_some(visible)
    });

    // `offset`/`selected` 是 ratatui 在 `render_stateful_widget` **内部**才钳位的，这里读到
    // 的是上一帧的旧值。列表刚缩短（删歌、换搜索结果）而光标还没来得及 clamp 时，旧下标
    // 可能越界，会把**全部**真实行判成窗口外、整屏占位（闪烁一帧）。先按当前长度钳一遍，
    // 让占位判定与 ratatui 内部口径一致。
    let last = list.songs.len().saturating_sub(1);
    let offset = list.cursor.offset().min(last);
    let selected = list.cursor.selected().map(|index| index.min(last));
    let items: Vec<_> = list
        .songs
        .iter()
        .enumerate()
        .map(|(index, song)| {
            // 窗口外的行只放等高占位：`List` 需要等长的 items 才能算滚动偏移，
            // 但没必要为看不见的行做整串格式化（见 `row_is_visible`）。
            if !row_is_visible(index, offset, selected, visible_rows) {
                return ListItem::from("");
            }
            let context = RowContext {
                width: row_width,
                is_current: view.current_hash == Some(song.hash.as_str()),
                playback: view.playback,
                hover: hover_row == Some(index),
            };
            song_row(index, song, context, theme)
        })
        .collect();

    let widget = selection_list(items, theme);

    frame.render_stateful_widget(widget, inner, &mut list.cursor);
    render_scrollbar(frame, area, list.songs.len(), list.cursor.selected());
}

/// 歌单条目列表。
pub fn render_playlist_entries(
    frame: &mut Frame,
    area: Rect,
    list: &mut EntryList<Playlist>,
    focused: bool,
    theme: &Theme,
) {
    let title = "歌单广场".to_string();
    render_entry_list(frame, area, list, &title, focused, theme, |playlist| {
        playlist_subtitle(playlist)
    });
}

/// 歌手条目列表。
pub fn render_artist_entries(
    frame: &mut Frame,
    area: Rect,
    list: &mut EntryList<Artist>,
    focused: bool,
    theme: &Theme,
) {
    render_entry_list(frame, area, list, "歌手", focused, theme, artist_subtitle);
}

/// 排行榜条目列表。
pub fn render_rank_entries(
    frame: &mut Frame,
    area: Rect,
    list: &mut EntryList<RankBoard>,
    focused: bool,
    theme: &Theme,
) {
    render_entry_list(frame, area, list, "排行榜", focused, theme, rank_subtitle);
}

/// 云端歌单条目列表。
pub fn render_cloud_entries(
    frame: &mut Frame,
    area: Rect,
    list: &mut EntryList<Playlist>,
    focused: bool,
    theme: &Theme,
) {
    render_entry_list(
        frame,
        area,
        list,
        "云端歌单 · s 收藏单曲 / S 同步队列",
        focused,
        theme,
        |playlist| {
            let mut subtitle = playlist_subtitle(playlist);
            if !playlist.is_writable() {
                subtitle.push_str(" · 只读");
            }
            subtitle
        },
    );
}

/// 条目列表的通用渲染。`subtitle` 由调用方决定每类条目显示什么副信息。
fn render_entry_list<T: EntryTitle>(
    frame: &mut Frame,
    area: Rect,
    list: &mut EntryList<T>,
    title: &str,
    focused: bool,
    theme: &Theme,
    subtitle: impl Fn(&T) -> String,
) {
    if too_small(area) {
        return;
    }

    let block = panel(title.to_string(), focused, theme);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if list.load.is_loading() {
        frame.render_widget(loading_placeholder(theme), inner);
        return;
    }
    if let Some(reason) = list.load.error() {
        frame.render_widget(
            failed_placeholder(
                reason,
                Some(&format!("按 {} 重试", key_hint_for("R"))),
                theme,
            ),
            inner,
        );
        return;
    }
    if list.entries.is_empty() {
        frame.render_widget(
            empty_placeholder(&format!("暂无数据 · {} 重新载入", key_hint_for("R")), theme),
            inner,
        );
        return;
    }
    let width = inner.width.saturating_sub(1) as usize;
    let items: Vec<_> = list
        .entries
        .iter()
        .enumerate()
        .map(|(index, entry)| entry_row(index, entry.title(), &subtitle(entry), None, width, theme))
        .collect();

    let widget = selection_list(items, theme);

    frame.render_stateful_widget(widget, inner, &mut list.cursor);
    render_scrollbar(frame, area, list.entries.len(), list.cursor.selected());
}

/// 条目标题：三类条目都有 `name` 字段，用一个小 trait 统一取出来。
trait EntryTitle {
    fn title(&self) -> &str;
}

impl EntryTitle for Playlist {
    fn title(&self) -> &str {
        &self.name
    }
}

impl EntryTitle for Artist {
    fn title(&self) -> &str {
        &self.name
    }
}

impl EntryTitle for RankBoard {
    fn title(&self) -> &str {
        &self.name
    }
}

/// 搜索输入框。
pub fn render_search_input(
    frame: &mut Frame,
    area: Rect,
    pane: &SearchPane,
    focused: bool,
    theme: &Theme,
) {
    if area.height < 3 || area.width < 12 {
        return;
    }

    let title = if pane.editing {
        "搜索 · 输入中（Enter 提交 / Esc 退出）"
    } else {
        "搜索 · 按 Enter 或 / 开始输入"
    };

    let block = panel(title, focused, theme);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height == 0 || inner.width < 4 {
        return;
    }

    let empty = pane.input.is_empty();
    let text = if empty {
        "输入关键词，例如「海阔天空」"
    } else {
        pane.input.text()
    };
    let style = if empty { theme.dim() } else { theme.body() };

    let line = Line::from(vec![
        Span::styled("> ", theme.title()),
        Span::styled(
            truncate_to_width(text, inner.width.saturating_sub(2) as usize),
            style,
        ),
    ]);
    frame.render_widget(Paragraph::new(line), inner);

    // 编辑态下把真实终端光标放到输入位置，方便用户看清插入点
    if pane.editing {
        let before_cursor = &pane.input.text()[..pane.input.cursor_byte_index()];
        let offset = display_width(before_cursor);
        let max_offset = inner.width.saturating_sub(3) as usize;
        let x = inner.x + 2 + offset.min(max_offset) as u16;
        frame.set_cursor_position((x, inner.y));
    }
}

/// 右侧滚动条。列表放得下时不画。
fn render_scrollbar(frame: &mut Frame, area: Rect, total: usize, position: Option<usize>) {
    if total == 0 || area.height < 5 {
        return;
    }
    // 内容比视口短就不需要滚动条
    if total <= area.height.saturating_sub(2) as usize {
        return;
    }

    let mut state = ScrollbarState::new(total).position(position.unwrap_or(0));
    frame.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .thumb_symbol("┃")
            .track_symbol(Some("│")),
        area,
        &mut state,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::theme::{Theme, ThemeName};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    /// 取屏幕文本，并去掉所有空白。
    ///
    /// ratatui 会给宽字符（CJK）后面补一个占位格，直接按原样比对字符串会失败，
    /// 所以这里统一把空白挤掉再断言。
    fn rendered_text(buffer: &ratatui::buffer::Buffer) -> String {
        buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect()
    }

    /// 条目列表的行里必须带上标题。
    ///
    /// 之前这里先把每一行用**空标题**构造了一遍（那份结果随后被整体丢弃），
    /// 再用真标题构造第二遍。除了每帧多一次全量分配，读代码的人也很难判断哪份
    /// 才是真正生效的。这个测试锁住「屏幕上真的有标题」。
    #[test]
    fn entry_list_rows_show_titles() {
        let mut list = EntryList::default();
        list.replace(vec![
            Playlist {
                name: "我的歌单".to_string(),
                ..Playlist::default()
            },
            Playlist {
                name: "第二张".to_string(),
                ..Playlist::default()
            },
        ]);

        let mut terminal = Terminal::new(TestBackend::new(36, 8)).expect("建测试终端");
        let area = Rect::new(0, 0, 36, 8);
        terminal
            .draw(|frame| {
                render_entry_list(
                    frame,
                    area,
                    &mut list,
                    "测试",
                    true,
                    &Theme::for_config(ThemeName::Default, false),
                    |_| "副标题".to_string(),
                );
            })
            .expect("渲染");

        let text = rendered_text(terminal.backend().buffer());
        assert!(text.contains("我的歌单"), "第一行应显示标题：{text:?}");
        assert!(text.contains("第二张"), "第二行应显示标题：{text:?}");
        assert!(text.contains("副标题"), "副标题也应显示：{text:?}");
    }

    /// 列表从底部缩短后，旧的滚动下标可能越界；渲染必须按当前长度钳位，
    /// 否则整屏都会被判成「窗口外」而空白（闪烁一帧）。
    #[test]
    fn song_list_renders_real_rows_after_shrinking_from_the_bottom() {
        use crate::api::model::Song;

        let song = |index: usize| Song {
            name: format!("曲目{index}"),
            hash: format!("h{index}"),
            ..Song::default()
        };
        let view = || SongView {
            focused: true,
            current_hash: None,
            playback: PlaybackState::Playing,
            pointer: None,
        };

        let mut list = SongList::default();
        list.replace("测试", (0..40).map(song).collect());
        // 模拟「滚到底」：选中并滚到最后一首。
        list.cursor.select(Some(39));
        *list.cursor.offset_mut() = 39;

        let mut terminal = Terminal::new(TestBackend::new(40, 8)).expect("建测试终端");
        terminal
            .draw(|frame| {
                render_song_list(
                    frame,
                    Rect::new(0, 0, 40, 8),
                    &mut list,
                    view(),
                    &Theme::for_config(ThemeName::Default, false),
                );
            })
            .expect("渲染滚到底的列表");

        // 模拟「删到很少」：光标还停在旧下标 39，但列表只剩 3 首。
        list.songs.truncate(3);
        terminal
            .draw(|frame| {
                render_song_list(
                    frame,
                    Rect::new(0, 0, 40, 8),
                    &mut list,
                    view(),
                    &Theme::for_config(ThemeName::Default, false),
                );
            })
            .expect("渲染缩短后的列表");

        let text = rendered_text(terminal.backend().buffer());
        assert!(text.contains("曲目0"), "缩短后仍要渲染真实行：{text:?}");
        assert!(text.contains("曲目2"), "缩短后仍要渲染真实行：{text:?}");

        // 删到空也不能 panic。
        list.songs.clear();
        terminal
            .draw(|frame| {
                render_song_list(
                    frame,
                    Rect::new(0, 0, 40, 8),
                    &mut list,
                    view(),
                    &Theme::for_config(ThemeName::Default, false),
                );
            })
            .expect("渲染空列表");
    }
}
