//! 终端界面。
//!
//! 布局是固定骨架 + 自适应比例：
//!
//! ```text
//! ┌──────────┬──────────────────────────────────────────────┐
//! │          │  Primary：搜索框 / 歌单·歌手·榜单条目列表        │
//! │  Sidebar ├───────────────────────┬──────────────────────┤
//! │          │  Secondary：歌曲列表   │  Lyrics：歌词面板      │
//! ├──────────┴───────────────────────┴──────────────────────┤
//! │  Player：曲目 + 进度条                                    │
//! ├─────────────────────────────────────────────────────────┤
//! │  Status：消息 + 忙碌指示                                  │
//! └─────────────────────────────────────────────────────────┘
//! ```
//!
//! 三个自适应点：
//!
//! * 侧边栏宽度随终端宽度伸缩（20~30 列），窄于 56 列时自动隐藏；
//! * 歌词面板在宽终端里占右侧一列，窄终端里改为上下对半；
//! * 播放队列为空时高度归零，把空间还给列表。
//!
//! 这样一套布局从 80 列的 SSH 窗口到 200 列的宽屏都能用，不需要用户配置。

pub mod icons;
pub mod theme;
pub mod views;
pub mod widgets;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::widgets::Paragraph;

use crate::app::state::{AppState, Focus, HitTarget, Tab};
use crate::audio::engine::PlaybackState;
use theme::Theme;
use views::player::PLAYER_HEIGHT;

/// 状态栏占 1 行。
const STATUS_HEIGHT: u16 = 1;
/// 终端小于这个尺寸时只显示提示。
const MIN_WIDTH: u16 = 30;
const MIN_HEIGHT: u16 = 8;
/// 侧边栏低于这个宽度就隐藏，把空间让给内容。
const SIDEBAR_MIN_TOTAL_WIDTH: u16 = 56;
/// 内容区达到这个宽度时，歌词改为右侧独立列。
const LYRIC_SIDE_BY_SIDE_WIDTH: u16 = 100;

/// 渲染一帧。
pub fn render(frame: &mut Frame, state: &mut AppState) {
    let theme = Theme::for_config(state.config.theme, state.config.basic_color);
    let area = frame.area();

    // 命中区每帧重建，保证鼠标坐标换算始终对应当前布局
    state.begin_frame();

    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        render_too_small(frame, area);
        return;
    }

    // 提前取出来，避免后面可变借用 state 时冲突
    let busy = state.busy_label();

    let [content_area, player_area, status_area] = Layout::vertical([
        Constraint::Min(6),
        Constraint::Length(PLAYER_HEIGHT),
        Constraint::Length(STATUS_HEIGHT),
    ])
    // 三段之间各留一空行：紧贴着会让整屏看起来像一堵墙
    .spacing(1)
    .areas(area);

    let show_sidebar = state.sidebar_visible && content_area.width >= SIDEBAR_MIN_TOTAL_WIDTH;
    let (sidebar_area, main_area) = if show_sidebar {
        let [sidebar, main] = Layout::horizontal([
            Constraint::Length(views::sidebar_width(content_area.width)),
            Constraint::Min(24),
        ])
        .spacing(1)
        .areas(content_area);
        (Some(sidebar), main)
    } else {
        (None, content_area)
    };

    if let Some(sidebar_area) = sidebar_area {
        // 命中区必须按**渲染时的实际行号**算。
        //
        // 侧边栏按分组渲染（`Tab::SIDEBAR_ORDER`，顺序与 `ALL` 不同）且每组
        // 前面插一行标题，所以行号不再是「标题行 + 序号」这么简单。这里照着
        // `render_sidebar` 的推进方式走一遍：遇到新分组先跳过标题行，再登记
        // 该标签那一行。算错的话点「搜索」会跳到别的页，而且没有任何提示。
        //
        // 命中的是**显示顺序里的位置**（`SIDEBAR_ORDER` 的下标），app 层用
        // `Tab::from_sidebar_index` 换回标签页——两边都按同一份顺序走，才不会
        // 「屏幕上点第一个、跳到了数字键意义上的第一个」。
        let mut row = sidebar_area.y + 2; // 跳过上边框与 "kugou-tui 1/10" 标题行
        let mut current_group: Option<&'static str> = None;
        for (index, tab) in Tab::SIDEBAR_ORDER.iter().enumerate() {
            if Some(tab.group()) != current_group {
                current_group = Some(tab.group());
                row += 1; // 分组标题行，不可点击
            }
            if row >= sidebar_area.bottom() {
                break;
            }
            let rect = Rect::new(sidebar_area.x, row, sidebar_area.width, 1);
            state.add_hit_zone(rect, HitTarget::Tab(index), 0, Tab::SIDEBAR_ORDER.len());
            row += 1;
        }
        views::render_sidebar(frame, sidebar_area, state, &theme);
    }

    render_main(frame, main_area, state, &theme);
    views::render_player(frame, player_area, state, &theme);
    views::render_status(frame, status_area, state, busy, &theme);

    // 帮助面板是模态的，最后画，盖住其它一切
    if state.help.is_open() {
        views::render_help(frame, area, state, &theme);
    }

    // 登录弹窗优先级高于帮助
    if state.login_picker.is_some() {
        let area = crate::ui::widgets::centered_rect(frame.area(), 46, 12);
        views::render_login_picker(frame, area, state, &theme);
        return;
    }

    if let Some(login) = state.login.as_ref() {
        views::render_login(
            frame,
            login,
            state.config.active_source_kind(),
            state.config.qr_aspect,
            &theme,
        );
    }

    // 文本输入弹窗画在登录弹窗之上
    if let Some(prompt) = state.prompt.as_ref() {
        views::render_prompt(frame, prompt, &theme);
    }

    // 歌曲右键菜单：画在确认框之下（确认框是「要不要做」的最后一道闸）
    if let Some(menu) = state.context_menu.as_ref() {
        views::render_context_menu(frame, menu, &theme);
    }

    // 下载音质选择框：画在右键菜单之上（它是从菜单里点出来的）
    if let Some(picker) = state.quality_picker.as_ref() {
        views::render_quality_picker(frame, picker, &theme);
    }

    // 确认对话框优先级最高，画在最上层
    if let Some(action) = state.pending_confirm {
        views::render_confirm(frame, action, &theme);
    }
}

/// 主内容区：Primary（输入/条目）+ Secondary（歌曲）+ 歌词 + 队列。
fn render_main(frame: &mut Frame, area: Rect, state: &mut AppState, theme: &Theme) {
    if area.height < 4 || area.width < 16 {
        return;
    }

    // 可视化页没有列表，整块主区都给它——走下面那套「条目 + 歌曲 + 队列」的分割
    // 会白白浪费一大半空间。
    if state.tab == Tab::Visualizer {
        let focused = state.focus == Focus::Primary;
        views::render_visualizer(frame, area, state, focused, theme);
        return;
    }

    // 首页 / 队列 / 设置都是单一用途的页面，整块主区都给它——
    // 塞进下面那套「条目 + 歌曲 + 队列」的分割里，每块都会小到没法用。
    match state.tab {
        Tab::Home => {
            views::render_home(frame, area, state, theme);
            return;
        }
        Tab::Settings => {
            // 先把值算出来：下面要可变借用 state 去登记命中区
            let values = crate::app::settings::values(state);
            let focused = state.focus == Focus::Primary;
            views::render_settings(frame, area, state, &values, focused, theme);
            return;
        }
        Tab::Queue => {
            let focused = state.focus == Focus::Primary;
            let playback = state.playback;
            // 先取出 hash：下面要可变借用 queue_cursor，不能再持有 state 的引用
            let current_hash = state.current.as_ref().map(|song| song.hash.clone());
            views::render_queue(
                frame,
                area,
                &state.queue,
                &mut state.queue_cursor,
                views::QueueView {
                    focused,
                    current_hash: current_hash.as_deref(),
                    playback,
                },
                theme,
            );
            return;
        }
        _ => {}
    }

    let primary_height = match state.tab {
        // 搜索框只有一行输入，给 3 行（含边框）就够
        Tab::Search => 3,
        // 条目列表占三分之一，但不小于 5 行也不大于 12 行
        _ => (area.height / 3).clamp(5, 12),
    };
    let queue_height = if state.queue.is_empty() {
        0
    } else {
        (area.height / 4).clamp(4, 10)
    };

    let [primary_area, middle_area, queue_area] = Layout::vertical([
        Constraint::Length(primary_height),
        Constraint::Min(3),
        Constraint::Length(queue_height),
    ])
    .areas(area);

    // 命中区：Primary（搜索框不计入），Secondary（歌曲），Queue。
    // 内缩 1 行跳过边框，避免点到边框上被当成点到了列表。
    if state.tab != Tab::Search && primary_area.height > 3 {
        let entries_len = match state.tab {
            Tab::Playlists => state.playlists.list.len(),
            Tab::Artists => state.artists.list.len(),
            Tab::Ranks => state.ranks.list.len(),
            Tab::Cloud => state.cloud.list.len(),
            // 音源页的「条目列表」就是音源本身——不返回真实长度的话，
            // 鼠标命中区长度为 0，点上去不会有任何反应
            Tab::Sources => state.config.sources.ordered().len(),
            // 其余页面没有条目列表
            Tab::Search | Tab::Home | Tab::Queue | Tab::Visualizer | Tab::Settings => 0,
        };
        state.add_hit_zone(
            Rect::new(
                primary_area.x + 1,
                primary_area.y + 1,
                primary_area.width.max(1).saturating_sub(2),
                primary_area.height.saturating_sub(2),
            ),
            HitTarget::Entries,
            state.entries_offset(),
            entries_len,
        );
    }

    render_primary(frame, primary_area, state, theme);

    // 先取出后续要用到的状态，避免与 state 的可变借用冲突
    let current_hash = state.current.as_ref().map(|song| song.hash.clone());
    let playback = state.playback;
    let secondary_focused = state.focus == Focus::Secondary;

    let (list_area, lyric_area) = split_middle(middle_area, state.show_lyric_panel);

    render_song_pane(
        frame,
        list_area,
        state,
        secondary_focused,
        current_hash.as_deref(),
        playback,
        theme,
    );

    if list_area.height > 3 {
        state.add_hit_zone(
            Rect::new(
                list_area.x + 1,
                list_area.y + 1,
                list_area.width.max(1).saturating_sub(2),
                list_area.height.saturating_sub(2),
            ),
            HitTarget::Songs,
            state.songs_offset(),
            state.songs_len(),
        );
    }

    if let Some(lyric_area) = lyric_area {
        views::render_lyric_panel(frame, lyric_area, state, theme);
    }

    if queue_area.height > 0 {
        let queue_focused = state.focus == Focus::Queue;
        views::render_queue(
            frame,
            queue_area,
            &state.queue,
            &mut state.queue_cursor,
            views::QueueView {
                focused: queue_focused,
                current_hash: current_hash.as_deref(),
                playback,
            },
            theme,
        );

        if queue_area.height > 3 {
            state.add_hit_zone(
                Rect::new(
                    queue_area.x + 1,
                    queue_area.y + 1,
                    queue_area.width.max(1).saturating_sub(2),
                    queue_area.height.saturating_sub(2),
                ),
                HitTarget::Queue,
                state.queue_cursor.offset(),
                state.queue.len(),
            );
        }
    }
}

/// 主区「条目列表」当前的滚动偏移（用于鼠标行号换算）。
impl AppState {
    fn entries_offset(&self) -> usize {
        match self.tab {
            Tab::Playlists => self.playlists.list.cursor.offset(),
            Tab::Artists => self.artists.list.cursor.offset(),
            Tab::Ranks => self.ranks.list.cursor.offset(),
            Tab::Cloud => self.cloud.list.cursor.offset(),
            // 搜索页、可视化页、音源页都没有条目列表
            Tab::Search
            | Tab::Home
            | Tab::Queue
            | Tab::Visualizer
            | Tab::Sources
            | Tab::Settings => 0,
        }
    }

    fn songs_offset(&self) -> usize {
        self.songs().map(|list| list.cursor.offset()).unwrap_or(0)
    }

    fn songs_len(&self) -> usize {
        self.songs().map(|list| list.len()).unwrap_or(0)
    }
}

/// 把主区域中部切成「列表 + 歌词」。
///
/// 宽终端左右分栏（列表更宽，符合阅读顺序）；窄终端上下对半，两者都能看到。
fn split_middle(area: Rect, lyrics_enabled: bool) -> (Rect, Option<Rect>) {
    if !lyrics_enabled || area.width < 40 || area.height < 6 {
        return (area, None);
    }

    if area.width >= LYRIC_SIDE_BY_SIDE_WIDTH {
        let [list, lyric] =
            Layout::horizontal([Constraint::Percentage(56), Constraint::Min(30)]).areas(area);
        (list, Some(lyric))
    } else {
        let [list, lyric] =
            Layout::vertical([Constraint::Percentage(50), Constraint::Min(3)]).areas(area);
        (list, Some(lyric))
    }
}

/// Primary 区域按标签页分派。
fn render_primary(frame: &mut Frame, area: Rect, state: &mut AppState, theme: &Theme) {
    let focused = state.focus == Focus::Primary;

    match state.tab {
        Tab::Search => views::render_search_input(frame, area, &state.search, focused, theme),
        Tab::Playlists => {
            views::render_playlist_entries(frame, area, &mut state.playlists.list, focused, theme)
        }
        Tab::Artists => {
            views::render_artist_entries(frame, area, &mut state.artists.list, focused, theme)
        }
        Tab::Ranks => {
            views::render_rank_entries(frame, area, &mut state.ranks.list, focused, theme)
        }
        Tab::Cloud => {
            views::render_cloud_entries(frame, area, &mut state.cloud.list, focused, theme)
        }
        Tab::Sources => views::render_sources(frame, area, state, focused, theme),
        // 这几页都由 render_main 整屏渲染，不会走到这里
        Tab::Home | Tab::Queue | Tab::Visualizer | Tab::Settings => {}
    }
}

/// Secondary 区域按标签页分派，都是同一套歌曲列表。
fn render_song_pane(
    frame: &mut Frame,
    area: Rect,
    state: &mut AppState,
    focused: bool,
    current_hash: Option<&str>,
    playback: PlaybackState,
    theme: &Theme,
) {
    // 可视化页没有歌曲列表（它整屏都用来画频谱），这里提前返回。
    // render_main 对它会短路，走到这里只是为其它页兜底。
    // 先取出鼠标位置：下面 list 会可变借用 state，之后再读就冲突了
    let pointer = state.hover;

    let Some(list) = (match state.tab {
        Tab::Search => Some(&mut state.search.results),
        Tab::Playlists => Some(&mut state.playlists.songs),
        Tab::Artists => Some(&mut state.artists.songs),
        Tab::Ranks => Some(&mut state.ranks.songs),
        Tab::Cloud => Some(&mut state.cloud.songs),
        // 音源页没有歌曲列表
        Tab::Home | Tab::Queue | Tab::Visualizer | Tab::Sources | Tab::Settings => None,
    }) else {
        return;
    };

    views::render_song_list(
        frame,
        area,
        list,
        views::SongView {
            focused,
            current_hash,
            playback,
            pointer,
        },
        theme,
    );
}

fn render_too_small(frame: &mut Frame, area: Rect) {
    let text = format!(
        "终端窗口过小（当前 {}x{}），请放大到至少 {}x{}",
        area.width, area.height, MIN_WIDTH, MIN_HEIGHT
    );
    frame.render_widget(Paragraph::new(text).alignment(Alignment::Center), area);
}
