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
//! │  Player：曲目 + 进度条 + 歌词上下文 + 迷你频谱              │
//! ├─────────────────────────────────────────────────────────┤
//! │  Status：消息 + 忙碌指示                                  │
//! └─────────────────────────────────────────────────────────┘
//! ```
//!
//! 三个自适应点：
//!
//! * 侧边栏宽度随终端宽度伸缩（24~32 列），窄于 56 列时自动隐藏；
//! * 歌词面板在宽终端里占右侧一列，窄终端里改为上下对半；
//! * 播放队列为空时高度归零，把空间还给列表。
//!
//! 这样一套布局从 80 列的 SSH 窗口到 200 列的宽屏都能用，不需要用户配置。

pub mod hit;
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
///
/// 高度下限按最紧凑的可用布局算：播放条 6 行 + 状态栏 1 行 + 两段间距 2 行，
/// 内容区至少还要 4 行才画得下一块面板（`render_main` 的下限）。
const MIN_WIDTH: u16 = 30;
const MIN_HEIGHT: u16 = 13;
/// 侧边栏低于这个宽度就隐藏，把空间让给内容。
const SIDEBAR_MIN_TOTAL_WIDTH: u16 = 56;
/// 内容区达到这个宽度时，歌词改为右侧独立列。
///
/// 取 52 而不是更宽：`Min(20)` 的歌词列在这个宽度下仍有 22 列（够放一句中文），
/// 而上下对半会把本来就不高的中区再劈成两半——80×24 那种终端里歌词干脆整块
/// 消失，120×36 里也只剩两行。右侧独立列让两块都拿到中区的**全部高度**，比
/// 上下对半更耐矮终端。再窄（<52）才退回上下对半。
const LYRIC_SIDE_BY_SIDE_WIDTH: u16 = 52;

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
        // 两栏之间**不留 spacing**：分隔靠主区自己的左边框画一条竖线。
        // 留 1 列会变成「竖线-空列-竖线」的断口，整屏被切碎成三段。
        let [sidebar, main] = Layout::horizontal([
            Constraint::Length(views::sidebar_width(content_area.width)),
            Constraint::Min(24),
        ])
        .areas(content_area);
        (Some(sidebar), main)
    } else {
        (None, content_area)
    };

    if let Some(sidebar_area) = sidebar_area {
        // 命中区必须按**渲染时的实际行号**算，而唯一可靠的做法是复用渲染时那份
        // 逐行计划（`views::sidebar_nav_rows`）——两边各算各的，改一行内容就可能
        // 让点「搜索」跳到别的页，而且没有任何提示。
        //
        // 命中的是**显示顺序里的位置**（`SIDEBAR_ORDER` 的下标），app 层用
        // `Tab::from_sidebar_index` 换回标签页——两边都按同一份顺序走，才不会
        // 「屏幕上点第一个、跳到了数字键意义上的第一个」。
        let inner = views::render_sidebar(frame, sidebar_area, state, &theme);
        for (offset, row) in views::sidebar_nav_rows().iter().enumerate() {
            let views::SidebarRow::Tab { index, .. } = row else {
                continue;
            };
            let y = inner.y + offset as u16;
            if y >= inner.bottom() {
                break;
            }
            let rect = Rect::new(sidebar_area.x, y, sidebar_area.width, 1);
            state
                .hit_map
                .insert(rect, HitTarget::Tab(*index), 0, Tab::SIDEBAR_ORDER.len());
        }
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

    // 歌曲右键菜单：画在确认框之下（确认框是「要不要做」的最后一道闸）。
    // 弹窗登记命中区不能走 `state.hit_map`（它正被 `&` 借着），所以渲染函数
    // 返回自己的 `HitMap`，这里再并进去。
    if let Some(menu) = state.context_menu.as_ref() {
        let zones = views::render_context_menu(frame, menu, &state.hit_map, &theme);
        state.merge_hit_zones(zones);
    }

    // 下载音质选择框：画在右键菜单之上（它是从菜单里点出来的）
    if let Some(picker) = state.quality_picker.as_ref() {
        let zones = views::render_quality_picker(frame, picker, &state.hit_map, &theme);
        state.merge_hit_zones(zones);
    }

    // cava 调参面板：盖在可视化页之上，最后画，和确认框同级
    if state.cava_panel.is_some() {
        views::render_cava_panel(frame, area, state, &theme);
    }

    // 确认对话框优先级最高，画在最上层
    if let Some(action) = state.pending_confirm {
        let zones = views::render_confirm(frame, action, &state.hit_map, &theme);
        state.merge_hit_zones(zones);
    }
}

/// 队列页与音源页：一列列表 + （宽终端）右侧的「封面 + 歌词 / 接下来」。
///
/// 两个页面都只有一份列表，列表短的时候整页大半是空白。rmpc 的队列页把
/// 当前曲目面板摆在列表旁边，这里同样：宽终端左右分栏；窄终端没有分栏
/// 余地，列表整块铺满。
///
/// 两页的命中区在这里登记而不是渲染函数里：`render_queue` / `render_sources`
/// 是被两处共用的（各列表页底部还嵌着一块队列面板），命中区的行号换算依赖
/// 每帧的真实布局，只能由掌握布局的这一层回填。
fn render_single_list_page(frame: &mut Frame, area: Rect, state: &mut AppState, theme: &Theme) {
    let focused = state.focus == Focus::Primary;
    let playback = state.playback;
    // 先取出 hash：下面要可变借用 state，不能再持有它的引用
    let current_hash = state.current.as_ref().map(|song| song.hash.clone());

    let list_area = if area.width >= 60 {
        // 音源行的地址段很长（api_base 21 列起），列表给宽一点，URL 不至于
        // 整段被裁；队列行相对短，把更多的宽度让给右侧面板。
        let ratio = if state.tab == Tab::Sources { 64 } else { 56 };
        let [list_area, lyric_area] =
            Layout::horizontal([Constraint::Percentage(ratio), Constraint::Min(20)])
                .spacing(1)
                .areas(area);
        views::render_lyric_panel(frame, lyric_area, state, theme);
        list_area
    } else {
        area
    };

    match state.tab {
        Tab::Queue => {
            views::render_queue(
                frame,
                list_area,
                &state.queue,
                &mut state.queue_cursor,
                views::QueueView {
                    focused,
                    current_hash: current_hash.as_deref(),
                    playback,
                },
                theme,
            );
            // 整页队列此前**没有**命中区：点行 / 拖行 / 滚轮全都没反应——
            // 只有其它页面里嵌着的那块队列面板登记过。
            if list_area.height > 3 {
                state.hit_map.insert(
                    Rect::new(
                        list_area.x + 1,
                        list_area.y + 1,
                        list_area.width.max(1).saturating_sub(2),
                        list_area.height.saturating_sub(2),
                    ),
                    HitTarget::Queue,
                    state.queue_cursor.offset(),
                    state.queue.len(),
                );
            }
        }
        Tab::Sources => {
            views::render_sources(frame, list_area, state, focused, theme);
            if list_area.height > 3 {
                state.hit_map.insert(
                    Rect::new(
                        list_area.x + 1,
                        list_area.y + 1,
                        list_area.width.max(1).saturating_sub(2),
                        list_area.height.saturating_sub(2),
                    ),
                    HitTarget::Entries,
                    state.sources_cursor.offset(),
                    state.config.sources.ordered().len(),
                );
            }
        }
        // 调用方只在这两个标签页进来
        _ => {}
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

    // 首页 / 队列 / 设置 / 音源都是单一用途的页面——塞进下面那套
    // 「条目 + 歌曲 + 队列」的分割里，每块都会小到没法用。
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
        // 队列与音源：宽终端把「封面 + 歌词 / 接下来」摆在列表右边。队列短
        // 的时候一整列空白，看着像面板坏了；音源页此前更糟——中区左半没有
        // 歌曲列表（音源页没有歌），歌词面板悬在右半的空地上。
        Tab::Queue | Tab::Sources => {
            render_single_list_page(frame, area, state, theme);
            return;
        }
        _ => {}
    }

    let primary_height = match state.tab {
        // 搜索框：类型标签 1 行 + 输入 1 行，加上上下边框共 4 行。
        // 这里给 2 会让 render_search_input 因高度不足直接整块不画。
        Tab::Search => 4,
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
    // 与外层一致：相邻面板之间留一空行。紧贴着两块边框会连成一条双线，
    // 整屏看起来像一堵墙（外层布局、侧边栏/主区之间也都是这么留的）。
    .spacing(1)
    .areas(area);

    // 命中区：Primary（搜索框不计入），Secondary（歌曲），Queue。
    // 内缩 1 行跳过边框，避免点到边框上被当成点到了列表。
    if state.tab != Tab::Search && primary_area.height > 3 {
        let entries_len = match state.tab {
            Tab::Playlists => state.playlists.list.len(),
            Tab::Artists => state.artists.list.len(),
            Tab::Ranks => state.ranks.list.len(),
            Tab::Cloud => state.cloud.list.len(),
            // 其余页面没有条目列表（音源页与队列页走 render_single_list_page）
            Tab::Search | Tab::Home | Tab::Queue | Tab::Visualizer | Tab::Settings => 0,
            Tab::Sources => unreachable!("音源页走 render_single_list_page"),
        };
        state.hit_map.insert(
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
        state.hit_map.insert(
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
            state.hit_map.insert(
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
            // 搜索页、可视化页没有条目列表；音源页走 render_single_list_page
            Tab::Search
            | Tab::Home
            | Tab::Queue
            | Tab::Visualizer
            | Tab::Settings
            | Tab::Sources => 0,
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
/// 两栏之间同样留一空行/一列，与外层及其它面板保持一致。
fn split_middle(area: Rect, lyrics_enabled: bool) -> (Rect, Option<Rect>) {
    if !lyrics_enabled || area.width < 40 {
        return (area, None);
    }

    if area.width >= LYRIC_SIDE_BY_SIDE_WIDTH {
        // 左右分栏只要够画一个面板就分（两块共用同一高度）。
        if area.height < 3 {
            return (area, None);
        }
        // 列表略宽（56%），歌词拿剩下的、但不低于 20 列——这是「歌词右侧一列」
        // 在窄终端仍然读得出来的下限。
        let [list, lyric] = Layout::horizontal([Constraint::Percentage(56), Constraint::Min(20)])
            .spacing(1)
            .areas(area);
        (list, Some(lyric))
    } else {
        // 上下对半：两块面板各要 3 行，再加中间一空行——不够就整块让位，
        // 免得挤出一块只有边框、没有内容的空面板。歌词给一个 4 行的下限，
        // 免得对半之后它只剩两行、连一句完整的都读不下来。
        if area.height < 7 {
            return (area, None);
        }
        let [list, lyric] = Layout::vertical([Constraint::Percentage(50), Constraint::Min(4)])
            .spacing(1)
            .areas(area);
        (list, Some(lyric))
    }
}

/// Primary 区域按标签页分派。
fn render_primary(frame: &mut Frame, area: Rect, state: &mut AppState, theme: &Theme) {
    let focused = state.focus == Focus::Primary;

    match state.tab {
        // 搜索输入框不画竖线：它与下面的结果列表在同一个主区里，
        // 两条左边框并排会变成双线。输入区靠占位文字和光标辨识就够了。
        Tab::Search => views::render_search_input(frame, area, &state.search, focused, theme),
        // `hits` 要在可变借用 state 之前取出来：` &state.hit_map` 与
        // `&mut state.playlists.list` 是同一个 struct 的两个字段，Rust 允许
        // 同时借用，但顺序反过来（先 &mut 再 &）就编不过。
        Tab::Playlists => views::render_playlist_entries(
            frame,
            area,
            &mut state.playlists.list,
            focused,
            &state.hit_map,
            theme,
        ),
        Tab::Artists => views::render_artist_entries(
            frame,
            area,
            &mut state.artists.list,
            focused,
            &state.hit_map,
            theme,
        ),
        Tab::Ranks => views::render_rank_entries(
            frame,
            area,
            &mut state.ranks.list,
            focused,
            &state.hit_map,
            theme,
        ),
        Tab::Cloud => views::render_cloud_entries(
            frame,
            area,
            &mut state.cloud.list,
            focused,
            &state.hit_map,
            theme,
        ),
        // 这几页都由 render_main 的单页分支或整页渲染处理，不会走到这里
        Tab::Home | Tab::Queue | Tab::Visualizer | Tab::Settings | Tab::Sources => {}
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
    // 搜索页按类型分派：歌手 / 歌单结果区是条目列表，不是歌曲列表。
    // 展开后（`expanded` 有值）则回到歌曲列表，显示该歌手 / 歌单的歌曲。
    if state.tab == Tab::Search {
        match state.search.kind {
            crate::app::state::SearchKind::Artist if state.search.expanded.is_none() => {
                views::render_artist_entries_titled(
                    frame,
                    area,
                    &mut state.search.artists,
                    "搜索歌手",
                    focused,
                    &state.hit_map,
                    theme,
                );
                return;
            }
            crate::app::state::SearchKind::Playlist if state.search.expanded.is_none() => {
                views::render_playlist_entries_titled(
                    frame,
                    area,
                    &mut state.search.playlists,
                    "搜索歌单",
                    focused,
                    &state.hit_map,
                    theme,
                );
                return;
            }
            _ => {}
        }
    }

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
        },
        &state.hit_map,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn playing_state() -> AppState {
        let song = |name: &str| crate::api::model::Song {
            name: name.to_string(),
            hash: format!("h{}", name.len()),
            singers: vec![crate::api::model::Singer {
                name: "周杰伦".to_string(),
                ..Default::default()
            }],
            album_name: "十一月的萧邦".to_string(),
            duration_ms: 226_000,
            ..Default::default()
        };
        let mut state = AppState::new(Config::default());
        state.playback = PlaybackState::Playing;
        state.current = Some(song("夜曲"));
        state.duration_ms = 226_000;
        state.position_ms = 74_000;
        state
            .queue
            .replace_with(vec![song("夜曲"), song("富士山下"), song("起风了")], 0);
        state
    }

    fn frame_text(state: &mut AppState, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("建测试终端");
        terminal
            .draw(|frame| render(frame, state))
            .expect("渲染整帧");
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

    /// 队列页的行必须有命中区：此前整页队列没登记过命中区，点行 / 拖行 /
    /// 滚轮全都没反应——只有其它页面里嵌的那块队列面板可点。
    #[test]
    fn queue_tab_rows_are_clickable() {
        let mut state = playing_state();
        state.tab = Tab::Queue;
        let mut terminal = Terminal::new(TestBackend::new(120, 36)).expect("建测试终端");
        terminal
            .draw(|frame| render(frame, &mut state))
            .expect("渲染整帧");

        // 在内容区里探出一处队列命中区：命中区的行数应覆盖队列长度
        let mut found = None;
        'outer: for y in 1..30u16 {
            for x in 1..60u16 {
                if let Some(zone) = state.hit_test(x, y)
                    && zone.target == HitTarget::Queue
                {
                    found = Some(zone);
                    break 'outer;
                }
            }
        }
        let zone = found.expect("队列页上应有 HitTarget::Queue 命中区");
        assert_eq!(zone.len, 3, "命中区覆盖整个队列");
    }

    /// 音源页此前把歌词面板悬在「中区右半」的空地上——音源页没有歌曲列表，
    /// 中区左半整块是空的，封面缩略图 + 歌词面板浮在半空，看着像渲染坏了。
    /// 现在音源列表在左、面板在右，且没歌词时右侧是「接下来」而不是歌词。
    #[test]
    fn sources_tab_never_floats_the_lyric_panel() {
        let mut state = playing_state();
        state.tab = Tab::Sources;
        let text = frame_text(&mut state, 120, 36);
        assert!(text.contains("音源管理"), "音源列表在左：{text}");
        assert!(
            text.contains("接下来"),
            "右侧应是「接下来」面板（当前曲无歌词）：{text}"
        );
        assert!(
            // 操作说明里本来就有「词=歌词」这种字样，所以只查面板标题：
            // 悬空的那块面板标题是「歌词 · 夜曲」
            !text.contains("歌词·夜曲"),
            "音源页不该再出现悬空的歌词面板：{text}"
        );
    }

    /// 首页的「我的资料」面板按内容收紧：未登录时只有一行字，不该给一块
    /// 半栏高的空面板（160×48 下此前是 14 行面板装 1 行字）。
    #[test]
    fn home_account_panel_hugs_its_content() {
        // 未登录：面板总高应为 4 行（标题行 + 1 行内容 + 上下边框）
        let mut state = playing_state();
        state.tab = Tab::Home;
        let height = 36u16;
        let mut terminal = Terminal::new(TestBackend::new(120, height)).expect("建测试终端");
        terminal
            .draw(|frame| render(frame, &mut state))
            .expect("渲染整帧");

        // 找到「我的资料」标题行，向下找面板下边框，量出面板高度
        let buffer = terminal.backend().buffer();
        let row_text = |y: u16| -> String {
            (0..120u16)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect()
        };
        let title_y = (0..height)
            .find(|&y| row_text(y).contains("我的资料"))
            .expect("账号面板应有标题");
        let bottom_y = (title_y..height)
            .find(|&y| y > title_y && row_text(y).contains('╰'))
            .expect("账号面板应有下边框");
        assert!(
            bottom_y - title_y <= 4,
            "未登录时账号面板应贴着内容，不该撑成空板（标题行 {title_y}，底 {bottom_y}）"
        );
    }

    /// 侧边栏每个标签的鼠标命中区，必须落在它**渲染时**的那一行上。
    ///
    /// 命中区与渲染共用 `views::sidebar_nav_rows()` 这一份逐行计划。这条测试从渲染
    /// 结果反推每个标签的行号，再验证点上去确实切到那个标签——一旦两边行号错位
    /// （比如以后又在导航前加了一行），这里会立刻失败，而不是让用户点「搜索」
    /// 跳到别的页。
    #[test]
    fn sidebar_click_zones_line_up_with_the_rendered_tabs() {
        let mut state = AppState::new(Config::default());
        state.sidebar_visible = true;

        let (width, height) = (100u16, 40u16);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("建测试终端");
        terminal
            .draw(|frame| render(frame, &mut state))
            .expect("渲染整帧");

        // 侧边栏面板上边框占 1 行，内区从 y=1 开始；导航行依次排开。
        // 侧边栏最窄 24 列（`sidebar_width`），第 2 列落在标签行内。
        const INNER_Y: u16 = 1;
        let mut checked = 0usize;
        for (offset, row) in views::sidebar_nav_rows().iter().enumerate() {
            let views::SidebarRow::Tab { tab, .. } = row else {
                continue;
            };
            let y = INNER_Y + offset as u16;
            let zone = state
                .hit_test(2, y)
                .unwrap_or_else(|| panic!("第 {y} 行的导航项应有命中区"));
            match zone.target {
                HitTarget::Tab(index) => assert_eq!(
                    Tab::from_sidebar_index(index),
                    Some(*tab),
                    "第 {y} 行的命中区指向的标签与渲染不符"
                ),
                other => panic!("第 {y} 行应是标签命中区，实际是 {other:?}"),
            }
            checked += 1;
        }
        assert_eq!(checked, Tab::SIDEBAR_ORDER.len(), "每个标签都要有命中区");
    }

    /// 迷你频谱要**铺满整行**：频段重采样到每一列，而不是按柱状图的步长
    /// 只画半行——空半行就是用户说的「没有自适应长度」。
    #[test]
    fn mini_spectrum_fills_its_row_inside_the_player() {
        let mut state = AppState::new(Config::default());
        state.playback = PlaybackState::Playing;
        state.smooth_spectrum = vec![1.0; 128];

        let (width, height) = (90u16, 20u16);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("建测试终端");
        terminal
            .draw(|frame| render(frame, &mut state))
            .expect("渲染整帧");

        // 布局：content | 空1行 | player(6) | 空1行 | status。频谱是播放条
        // 边框内第 4 行内容行。面板左右各占 2 列（边框 + 内边距），频谱
        // 铺满的是去掉这 4 列之后的内区。
        let player_y = height - PLAYER_HEIGHT - STATUS_HEIGHT - 1;
        let row = player_y + 4;
        let line: String = (2..width - 2)
            .map(|x| terminal.backend().buffer()[(x, row)].symbol())
            .collect();
        assert!(
            line.chars().all(|character| character == '█'),
            "满格频谱应铺满内区整行：|{line}|"
        );
    }

    /// 两层平滑后电平普遍落在 0.1~0.4，那种量级也要看得见——此前低于
    /// 0.33 只画一个暗点，整行看上去几乎不动。
    #[test]
    fn quiet_levels_still_show_visible_amplitude() {
        let mut state = AppState::new(Config::default());
        state.playback = PlaybackState::Playing;
        state.smooth_spectrum = vec![0.25; 128];

        let (width, height) = (90u16, 20u16);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("建测试终端");
        terminal
            .draw(|frame| render(frame, &mut state))
            .expect("渲染整帧");

        let player_y = height - PLAYER_HEIGHT - STATUS_HEIGHT - 1;
        let row = player_y + 4;
        let line: String = (2..width - 2)
            .map(|x| terminal.backend().buffer()[(x, row)].symbol())
            .collect();
        assert!(
            line.chars().all(|character| character == '▄'),
            "0.25 的电平应画成中档块字符：|{line}|"
        );
    }

    /// 静音就是一整行空白，不该冒出噪点。
    #[test]
    fn silent_spectrum_renders_as_an_empty_row() {
        let mut state = AppState::new(Config::default());
        state.playback = PlaybackState::Playing;
        state.smooth_spectrum = vec![0.0; 128];

        let (width, height) = (90u16, 20u16);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("建测试终端");
        terminal
            .draw(|frame| render(frame, &mut state))
            .expect("渲染整帧");

        let player_y = height - PLAYER_HEIGHT - STATUS_HEIGHT - 1;
        let row = player_y + 4;
        let line: String = (2..width - 2)
            .map(|x| terminal.backend().buffer()[(x, row)].symbol())
            .collect();
        assert!(line.chars().all(|character| character == ' '), "|{line}|");
    }

    /// 各种终端尺寸下整帧渲染都不得 panic。
    ///
    /// 布局里到处加了 1 行/1 列的间距，窗口缩小时某些区域很容易变成 0 尺寸；
    /// 这条把常见尺寸与各标签页都扫一遍，防止某个分支在极小尺寸下崩掉。
    #[test]
    fn frame_renders_without_panicking_across_sizes() {
        let widths = [30u16, 40, 55, 56, 57, 80, 100, 120, 160, 200];
        let heights = [8u16, 9, 10, 16, 24, 30, 36, 50];
        let tabs = [
            Tab::Home,
            Tab::Search,
            Tab::Playlists,
            Tab::Queue,
            Tab::Visualizer,
            Tab::Settings,
        ];

        for &tab in &tabs {
            for &width in &widths {
                for &height in &heights {
                    let mut state = AppState::new(Config::default());
                    state.tab = tab;
                    let mut terminal =
                        Terminal::new(TestBackend::new(width, height)).expect("建测试终端");
                    terminal
                        .draw(|frame| render(frame, &mut state))
                        .unwrap_or_else(|error| {
                            panic!("{tab:?} 在 {width}x{height} 渲染失败：{error}")
                        });
                }
            }
        }
    }
}

#[cfg(test)]
mod visual_dump {
    use super::*;
    use crate::app::state::Tab;
    use crate::config::Config;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    /// 把整帧渲染成纯文本打出来，用于人工检视观感。
    ///
    /// 不做断言——它是「截图」而不是测试，加 `--ignored` 平时不跑。
    #[test]
    #[ignore = "观感检视用，打印整帧文本"]
    fn dump_visualizer() {
        let mut state = AppState::new(Config::default());
        state.tab = Tab::Visualizer;
        state.sidebar_visible = false;
        state.playback = crate::audio::engine::PlaybackState::Playing;
        state.current = Some(crate::api::model::Song {
            name: "夜的第七章".to_string(),
            singers: vec![crate::api::model::Singer {
                name: "周杰伦".to_string(),
                ..Default::default()
            }],
            duration_ms: 280_000,
            ..crate::api::model::Song::default()
        });
        state.duration_ms = 280_000;
        state.position_ms = 95_000;
        // 造一条接近真实音乐的频谱：低频最高，随频率衰减，叠加几个随机峰
        state.spectrum = (0..128)
            .map(|index| {
                let t = index as f32 / 128.0;
                let base = (1.0 - t).powf(1.4);
                let bump = 0.25 * (12.0 * t * std::f32::consts::PI).sin().abs();
                (base * 0.85 + bump).clamp(0.0, 1.0)
            })
            .collect();
        state.smooth_spectrum = state.spectrum.clone();
        state.peak_spectrum = state.spectrum.iter().map(|value| value * 1.15).collect();

        let (width, height) = (120u16, 30u16);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("建测试终端");
        terminal
            .draw(|frame| render(frame, &mut state))
            .expect("渲染整帧");

        let buffer = terminal.backend().buffer();
        for y in 0..height {
            let line: String = (0..width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>();
            println!("|{line}|");
        }
    }

    #[test]
    #[ignore = "观感检视用，打印整帧文本"]
    fn dump_a_frame() {
        let mut state = AppState::new(Config::default());
        state.tab = Tab::Search;
        state.sidebar_visible = true;
        state.focus = Focus::Secondary;

        let song = |index: usize, name: &str, singer: &str, album: &str| crate::api::model::Song {
            name: name.to_string(),
            hash: format!("h{index}"),
            singers: vec![crate::api::model::Singer {
                name: singer.to_string(),
                ..Default::default()
            }],
            album_name: album.to_string(),
            duration_ms: 180_000 + index as u64 * 7000,
            ..crate::api::model::Song::default()
        };
        state.search.results.replace(
            "搜索结果",
            vec![
                song(0, "夜曲", "周杰伦", "十一月的萧邦"),
                song(1, "富士山下", "陈奕迅", "What's Going On…?"),
                song(2, "起风了", "买辣椒也用券", "起风了"),
                song(3, "Lemon", "米津玄師", "STRAY SHEEP"),
                song(4, "海阔天空", "Beyond", "乐与怒"),
            ],
        );
        state.search.results.cursor.select(Some(1));
        state.current = Some(song(0, "夜曲", "周杰伦", "十一月的萧邦"));
        state
            .queue
            .append(song(5, "富士山下", "陈奕迅", "What's Going On…?"));
        state.playback = crate::audio::engine::PlaybackState::Playing;
        state.duration_ms = 226_000;
        state.position_ms = 74_000;
        state.volume = 0.7;
        state.status = "已连接 · 内嵌接口".to_string();
        // 歌词与频谱：让播放条新加的两行（歌词上下文 / 迷你频谱）有内容可看
        state.lyric.lyric = crate::api::model::Lyric {
            text: String::new(),
            lines: (0..40)
                .map(|index| crate::api::model::LyricLine {
                    time_ms: index as u64 * 4_000,
                    text: format!("漆黑的夜里滑过一道微光{index}"),
                    ..crate::api::model::LyricLine::default()
                })
                .collect(),
        };
        state.lyric.active_line = Some(18);
        state.lyric.load.succeed();
        state.smooth_spectrum = (0..128)
            .map(|index| {
                let t = index as f32 / 128.0;
                ((1.0 - t).powf(1.4) * 0.9).clamp(0.0, 1.0)
            })
            .collect();

        let (width, height) = (120u16, 36u16);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("建测试终端");
        terminal
            .draw(|frame| render(frame, &mut state))
            .expect("渲染整帧");

        let buffer = terminal.backend().buffer();
        for y in 0..height {
            let line: String = (0..width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>();
            println!("|{line}|");
        }
    }

    /// 布局矩阵：所有标签页 × 常见终端尺寸，逐帧打印。
    ///
    /// 排查「某页在某尺寸下空间浪费 / 面板挤压 / 折行难看」用。每帧前打印
    /// `== {tab:?} {width}x{height} ==` 头，方便在输出里定位。
    #[test]
    #[ignore = "布局矩阵检视用，打印多帧文本"]
    fn dump_layout_matrix() {
        let song = |name: &str| crate::api::model::Song {
            name: name.to_string(),
            hash: format!("h{}", name.len()),
            singers: vec![crate::api::model::Singer {
                name: "周杰伦".to_string(),
                ..Default::default()
            }],
            album_name: "十一月的萧邦".to_string(),
            duration_ms: 226_000,
            ..crate::api::model::Song::default()
        };
        let base = || {
            let mut state = AppState::new(Config::default());
            state.sidebar_visible = true;
            state.playback = crate::audio::engine::PlaybackState::Playing;
            state.current = Some(song("夜曲"));
            state.duration_ms = 226_000;
            state.position_ms = 74_000;
            state.volume = 0.7;
            // 封面要在 dump 里出现，得给 picker 与一张渐变图——真机走的是
            // 自动探测的协议（kitty/sixel），这里用半块字符模拟。
            state.picker = Some(ratatui_image::picker::Picker::halfblocks());
            let mut pixels = image::RgbImage::new(64, 64);
            for (x, y, pixel) in pixels.enumerate_pixels_mut() {
                *pixel = image::Rgb([(x * 4) as u8, (y * 4) as u8, 128]);
            }
            state.cover.set_image(
                "h2".to_string(),
                image::DynamicImage::ImageRgb8(pixels),
                1.0,
            );
            state.search.results.replace(
                "搜索结果",
                vec![
                    song("夜曲"),
                    song("富士山下"),
                    song("起风了"),
                    song("Lemon"),
                    song("海阔天空"),
                ],
            );
            state
                .playlists
                .list
                .replace(vec![crate::api::model::Playlist::default(); 6]);
            state
                .queue
                .replace_with(vec![song("夜曲"), song("富士山下"), song("起风了")], 0);
            state
        };

        let sizes = [(80u16, 24u16), (120, 30), (120, 36), (160, 48)];
        let tabs = [
            Tab::Home,
            Tab::Search,
            Tab::Playlists,
            Tab::Artists,
            Tab::Ranks,
            Tab::Cloud,
            Tab::Queue,
            Tab::Sources,
            Tab::Visualizer,
            Tab::Settings,
        ];
        for &(width, height) in &sizes {
            for &tab in &tabs {
                let mut state = base();
                state.tab = tab;
                let mut terminal =
                    Terminal::new(TestBackend::new(width, height)).expect("建测试终端");
                terminal
                    .draw(|frame| render(frame, &mut state))
                    .expect("渲染整帧");
                println!("== {tab:?} {width}x{height} ==");
                let buffer = terminal.backend().buffer();
                for y in 0..height {
                    let line: String = (0..width)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>();
                    println!("|{line}|");
                }
            }
        }
    }

    /// 迷你频谱的观感：播放条上方那一行在不同电平分布下的样子。
    ///
    /// 造一条「低频高、随频率衰减、叠随机峰」的接近真实的频谱（与
    /// `dump_visualizer` 同一套），看对比度与铺满程度。
    #[test]
    #[ignore = "观感检视用，打印整帧文本"]
    fn dump_mini_spectrum() {
        let mut state = AppState::new(Config::default());
        state.playback = crate::audio::engine::PlaybackState::Playing;
        state.current = Some(crate::api::model::Song {
            name: "夜的第七章".to_string(),
            ..crate::api::model::Song::default()
        });
        state.spectrum = (0..128)
            .map(|index| {
                let t = index as f32 / 128.0;
                let base = (1.0 - t).powf(1.4);
                let bump = 0.25 * (12.0 * t * std::f32::consts::PI).sin().abs();
                (base * 0.85 + bump).clamp(0.0, 1.0)
            })
            .collect();
        state.smooth_spectrum = state.spectrum.clone();

        let (width, height) = (120u16, 20u16);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("建测试终端");
        terminal
            .draw(|frame| render(frame, &mut state))
            .expect("渲染整帧");

        let buffer = terminal.backend().buffer();
        for y in 0..height {
            let line: String = (0..width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>();
            println!("|{line}|");
        }
    }

    /// 窄窗口首页的观感（复刻用户截图的场景）：侧边栏 + 封面贴顶 + 「我的资料」
    /// 折行。看三件事：封面是否贴面板顶部、账号区各行是否折行而不是 `…` 截断、
    /// 侧边栏缓存目录名是否完整。
    #[test]
    #[ignore = "观感检视用，打印整帧文本"]
    fn dump_home_narrow() {
        let mut state = AppState::new(Config::default());
        state.playback = crate::audio::engine::PlaybackState::Playing;
        state.current = Some(crate::api::model::Song {
            name: "Where Is the Love（哪里是爱）".to_string(),
            singers: vec![crate::api::model::Singer {
                name: "Josh Vietti".to_string(),
                ..Default::default()
            }],
            album_name: "Best Of Both Worlds".to_string(),
            duration_ms: 273_000,
            ..Default::default()
        });
        state.duration_ms = 273_000;
        state.position_ms = 145_000;
        state.user_info = Some(crate::api::cloud::UserInfo {
            nickname: "惜别".to_string(),
            grade: Some(9),
            duration_min: Some(79_239),
            ..Default::default()
        });
        state.vip_info = Some(crate::api::cloud::VipInfo {
            kind: crate::api::cloud::VipKind::Concept,
            product: "svip".to_string(),
            end_time: "2026-11-06 12:00:00".to_string(),
        });
        state.config.sources.active = crate::source::SourceKind::KugouConcept;
        state
            .config
            .switch_source(crate::source::SourceKind::KugouConcept);
        state.config.cache_dir = std::path::PathBuf::from("/home/xibie/kugou-proxy-cache");
        // 统计按「今天」喂：资料区的本周摘要只看最近 7 天，写死的日期
        // 过了窗口就显示不出来，dump 也跟着失真。
        let today = crate::util::today_local();
        state.stats.record_complete_on(today.clone(), "Josh Vietti");
        state.stats.add_listen_on(today, 145_000);
        // 渐变而不是纯色：半块字符在上下同色时会退化成空格（用底色表示），
        // 纯色图整片空白，看不出贴顶效果。
        let mut pixels = image::RgbImage::new(64, 64);
        for (x, y, pixel) in pixels.enumerate_pixels_mut() {
            *pixel = image::Rgb([(x * 4) as u8, (y * 4) as u8, 128]);
        }
        state
            .cover
            .set_image("h".to_string(), image::DynamicImage::ImageRgb8(pixels), 1.0);
        state.picker = Some(ratatui_image::picker::Picker::halfblocks());

        let (width, height) = (92u16, 36u16);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("建测试终端");
        terminal
            .draw(|frame| render(frame, &mut state))
            .expect("渲染整帧");

        let buffer = terminal.backend().buffer();
        for y in 0..height {
            let line: String = (0..width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>();
            println!("|{line}|");
        }
    }

    /// 播放中一帧的成本探针：四条 30fps 热路径各跑 300 帧取平均。
    ///
    /// `cargo test --release perf_probe -- --ignored --nocapture`。数字是
    /// AGENTS.md 性能基线表的来源——改渲染热路径或 cava 前后各跑一次对比。
    #[test]
    #[ignore = "性能探针，--release 下运行才有意义"]
    fn perf_probe_playing_frames() {
        use std::time::Instant;
        const FRAMES: u32 = 300;

        // 接近真实音乐的一条频谱：低频高、随频率衰减、叠几个随机峰
        let spectrum: Vec<f32> = (0..128)
            .map(|index| {
                let t = index as f32 / 128.0;
                let base = (1.0 - t).powf(1.4);
                let bump = 0.25 * (12.0 * t * std::f32::consts::PI).sin().abs();
                (base * 0.85 + bump).clamp(0.0, 1.0)
            })
            .collect();

        // 首页：歌词面板可见 + 迷你频谱 + 逐帧变化的时间与频谱
        let mut home = AppState::new(Config::default());
        home.playback = crate::audio::engine::PlaybackState::Playing;
        home.current = Some(crate::api::model::Song {
            name: "夜的第七章".to_string(),
            duration_ms: 280_000,
            ..crate::api::model::Song::default()
        });
        home.duration_ms = 280_000;
        home.lyric.lyric = crate::api::model::Lyric {
            text: String::new(),
            lines: (0..150)
                .map(|index| crate::api::model::LyricLine {
                    time_ms: index as u64 * 1_800,
                    text: format!("夜色慢慢摊开露出月光{index}"),
                    ..crate::api::model::LyricLine::default()
                })
                .collect(),
        };
        home.lyric.active_line = Some(60);
        home.lyric.load.succeed();

        // 可视化页：柱状与瀑布两种模式。瀑布预先灌满 240 帧历史。
        let mut bars = AppState::new(Config::default());
        bars.tab = Tab::Visualizer;
        bars.playback = crate::audio::engine::PlaybackState::Playing;
        bars.spectrum = spectrum.clone();
        bars.smooth_spectrum = spectrum.clone();
        bars.peak_spectrum = spectrum.iter().map(|value| value * 1.15).collect();
        let mut waterfall = AppState::new(Config::default());
        waterfall.tab = Tab::Visualizer;
        waterfall.visualizer_mode = crate::app::state::VisualizerMode::Waterfall;
        waterfall.playback = crate::audio::engine::PlaybackState::Playing;
        waterfall.spectrum = spectrum.clone();
        waterfall.smooth_spectrum = spectrum.clone();
        for _ in 0..crate::app::state::SPECTRUM_HISTORY_LIMIT {
            waterfall.advance_visualizer(std::time::Duration::from_millis(33));
        }

        let mut terminal = Terminal::new(TestBackend::new(120, 36)).expect("建测试终端");

        let mut elapsed = std::time::Duration::ZERO;
        for frame in 0..FRAMES {
            home.position_ms = u64::from(frame) * 33;
            home.lyric.active_line = Some((frame as usize / 4) % 150);
            home.smooth_spectrum = spectrum.clone();
            let start = Instant::now();
            terminal.draw(|f| render(f, &mut home)).expect("渲染");
            elapsed += start.elapsed();
        }
        println!(
            "首页: {:.0} µs/帧",
            elapsed.as_secs_f32() * 1e6 / FRAMES as f32
        );

        let mut elapsed = std::time::Duration::ZERO;
        for frame in 0..FRAMES {
            let phase = frame as f32 * 0.1;
            for (index, value) in bars.smooth_spectrum.iter_mut().enumerate() {
                *value = (spectrum[index] * (0.8 + 0.2 * (phase + index as f32 * 0.3).sin()))
                    .clamp(0.0, 1.0);
            }
            let start = Instant::now();
            terminal.draw(|f| render(f, &mut bars)).expect("渲染");
            elapsed += start.elapsed();
        }
        println!(
            "柱状: {:.0} µs/帧",
            elapsed.as_secs_f32() * 1e6 / FRAMES as f32
        );

        let mut elapsed = std::time::Duration::ZERO;
        for frame in 0..FRAMES {
            let phase = frame as f32 * 0.1;
            for (index, value) in waterfall.smooth_spectrum.iter_mut().enumerate() {
                *value = (spectrum[index] * (0.8 + 0.2 * (phase + index as f32 * 0.3).sin()))
                    .clamp(0.0, 1.0);
            }
            waterfall.advance_visualizer(std::time::Duration::from_millis(33));
            let start = Instant::now();
            terminal.draw(|f| render(f, &mut waterfall)).expect("渲染");
            elapsed += start.elapsed();
        }
        println!(
            "瀑布: {:.0} µs/帧",
            elapsed.as_secs_f32() * 1e6 / FRAMES as f32
        );

        // cava 分析：30fps、44.1kHz 下每帧约 1455 个增量采样
        let mut cava = crate::audio::cava::Cava::with_options(128, 44_100, 0.77, true, 50, 10_000);
        let samples: Vec<f32> = (0..1455)
            .map(|index| {
                let t = index as f32 / 44_100.0;
                0.5 * (2.0 * std::f32::consts::PI * 220.0 * t).sin()
                    * (0.5 + 0.5 * (2.0 * std::f32::consts::PI * 2.0 * t).sin())
            })
            .collect();
        let start = Instant::now();
        for _ in 0..FRAMES {
            cava.execute(&samples);
        }
        let micros = start.elapsed().as_secs_f32() * 1e6 / FRAMES as f32;
        println!("cava: {micros:.0} µs/帧");
    }
}
