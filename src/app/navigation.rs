//! 导航：切页、焦点、选择移动。
//!
//! 从 `update.rs` 里切出来的第二块。这里的代码**只改「用户现在看哪儿」**：不发请求、
//! 不碰播放。需要加载时调 `ensure_tab_loaded`（它再去请 `update.rs` 拉数据）。
//!
//! 分清三层，改之前先认准自己在哪一层（这是这套界面唯一容易绕晕的地方）：
//!
//! ```text
//! Sidebar   ← 左侧导航（歌单广场 / 歌手 / 排行榜 / 云端 / 搜索…）
//! Primary   ← 当前页的条目列表（歌单列表、歌手列表…）；搜索页是输入框
//! Secondary ← 条目展开后的歌曲列表（某个歌单里的歌）；搜索页是结果列表
//! Queue     ← 播放队列面板
//! ```
//!
//! `move_selection` 按**焦点所在层**决定移动哪个列表，`go_back` 是它的逆操作。
//!
//! **`activate`（Enter 键）不在这里。** 它按 `(Tab, Focus)` 决定该搜索、该播放还是
//! 该打开，横跨 search / playback / 数据加载三个方向，所以住在 `update.rs` 的分派层。
//! 曾经放在这里，结果是 navigation 同时依赖 search 与 playback——「导航」去调播放，
//! 边界就说不通了。
//!
//! **依赖方向（单向，别改回去）**：`update.rs`（加载层）与 `settings.rs`
//! （设置页的光标移动）。设置页那一项是列表导航，所以由 `move_selection` 调；
//! 它住在 `settings.rs` 而不是这里，是因为「设置页的三个交互」应当在一起。

use crate::app::App;
use crate::app::state::{EntryList, Focus, Tab, move_selection, select_first, select_last};

impl App {
    // ==================================================================
    // 导航
    // ==================================================================

    pub fn switch_tab(&mut self, tab: Tab) {
        self.switch_tab_inner(tab, true);
    }

    /// 切到指定标签页。
    ///
    /// `move_focus` 为 `false` 时**保留当前焦点**。用方向键在侧边栏里连续上下走时
    /// 必须这样：若每次移动都把焦点甩回主区，用户按第二下就没反应了。
    fn switch_tab_inner(&mut self, tab: Tab, move_focus: bool) {
        self.state.tab = tab;
        self.state.search.editing = false;
        if move_focus {
            self.state.focus = Focus::Primary;
        }
        self.ensure_tab_loaded(tab);
    }

    /// 侧边栏里上下移动：切换标签，焦点留在侧边栏。
    ///
    /// 走 [`Tab::SIDEBAR_ORDER`]——那是**屏幕上真正显示的顺序**。以前走的是
    /// `ALL`（数字键落点），而两者顺序不同：从「队列」按一下 j 会跳到「歌词」，
    /// 可屏幕上下一个明明写着「首页」。首页置顶之后这种错位会出现在第一屏，
    /// 所以按显示顺序走是硬要求。
    fn move_sidebar(&mut self, delta: isize) {
        let current = Tab::SIDEBAR_ORDER
            .iter()
            .position(|tab| *tab == self.state.tab)
            .unwrap_or(0);
        let next = (current as isize + delta).clamp(0, Tab::SIDEBAR_ORDER.len() as isize - 1);
        let tab = Tab::SIDEBAR_ORDER[next as usize];

        if tab != self.state.tab {
            self.switch_tab_inner(tab, false);
        }
    }

    /// 首次进入某个标签页时惰性加载数据。
    ///
    /// 加载失败时列表保持为空、`loading` 复位，于是下次再切回来会重试——
    /// 对「服务刚启动完还没就绪」这类瞬时故障很友好。
    pub fn ensure_tab_loaded(&mut self, tab: Tab) {
        match tab {
            Tab::Playlists
                if self.state.playlists.list.is_empty()
                    && !self.state.playlists.list.load.is_loading() =>
            {
                self.load_plaza_playlists();
            }
            Tab::Artists
                if self.state.artists.list.is_empty()
                    && !self.state.artists.list.load.is_loading() =>
            {
                self.load_artists();
            }
            Tab::Ranks
                if self.state.ranks.list.is_empty() && !self.state.ranks.list.load.is_loading() =>
            {
                self.load_ranks();
            }
            Tab::Cloud
                if self.state.cloud.list.is_empty() && !self.state.cloud.list.load.is_loading() =>
            {
                self.load_cloud_playlists();
            }
            _ => {}
        }
    }

    /// 侧边栏：跳到第一个 / 最后一个标签（同样是**显示顺序**上的首尾）。
    fn select_sidebar_edge(&mut self, to_first: bool) {
        let target = if to_first {
            Tab::SIDEBAR_ORDER[0]
        } else {
            Tab::SIDEBAR_ORDER[Tab::SIDEBAR_ORDER.len() - 1]
        };
        if target != self.state.tab {
            self.switch_tab_inner(target, false);
        }
    }

    /// 条目列表（歌单 / 歌手 / 榜单）跳到首/末项。泛型是因为三类条目的类型不同。
    fn select_entry_edge<T>(list: &mut EntryList<T>, to_first: bool) {
        if to_first {
            list.select_first();
        } else {
            list.select_last();
        }
    }

    /// 移动当前列表的选择（↑/↓、PgUp/PgDn）。
    pub(super) fn move_selection(&mut self, delta: isize) {
        match (self.state.tab, self.state.focus) {
            // 侧边栏里上下移动 = 切换标签页（侧边栏的高亮就是当前标签）
            (_, Focus::Sidebar) => self.move_sidebar(delta),
            (_, Focus::Queue) => {
                move_selection(&mut self.state.queue_cursor, self.state.queue.len(), delta);
            }
            // 首页与可视化页都是纯展示页，没有任何列表，焦点在哪都退化成切换
            // 标签页——否则这两页会「上下键完全无响应」（可视化页实测踩过）。
            // 放在 Sidebar / Queue 之后即可，那两个焦点的行为仍由上面的分支决定。
            (Tab::Home | Tab::Visualizer, _) => self.move_sidebar(delta),
            // 队列页的主区就是队列本身
            (Tab::Queue, Focus::Primary) => {
                move_selection(&mut self.state.queue_cursor, self.state.queue.len(), delta);
            }
            // 搜索页主区就是结果列表
            (Tab::Search, Focus::Primary | Focus::Secondary) => {
                self.state.search.results.move_by(delta);
            }
            // 其余标签页的 Primary 是「条目列表」（歌单 / 歌手 / 榜单）
            (Tab::Playlists, Focus::Primary) => self.state.playlists.list.move_by(delta),
            (Tab::Artists, Focus::Primary) => self.state.artists.list.move_by(delta),
            (Tab::Ranks, Focus::Primary) => self.state.ranks.list.move_by(delta),
            (Tab::Cloud, Focus::Primary) => self.state.cloud.list.move_by(delta),
            // 音源页有自己的列表（音源条目），单独走一套
            (Tab::Sources, Focus::Primary) => {
                let len = self.state.config.sources.ordered().len();
                move_selection(&mut self.state.sources_cursor, len, delta);
            }
            // 设置页是一列设置项，焦点在哪都归它——否则这一页方向键没反应
            (Tab::Settings, _) => self.move_settings(delta),
            // Secondary 一律是当前标签页的歌曲列表。走 `songs_mut()` 而不是逐个
            // 枚举标签，新增标签页时这里不用跟着改。
            (_, Focus::Secondary) => {
                if let Some(list) = self.state.songs_mut() {
                    list.move_by(delta);
                }
            }
        }
    }

    /// 移动到当前列表的首/末项（Home/End、g/G）。
    pub(super) fn move_selection_edge(&mut self, to_first: bool) {
        let len = self.state.queue.len();
        match (self.state.tab, self.state.focus) {
            (_, Focus::Sidebar) => self.select_sidebar_edge(to_first),
            (_, Focus::Queue) => {
                if to_first {
                    select_first(&mut self.state.queue_cursor, len);
                } else {
                    select_last(&mut self.state.queue_cursor, len);
                }
            }
            // 首页与可视化页没有列表，首/末项退化成侧边栏的首/末个标签
            (Tab::Home | Tab::Visualizer, _) => self.select_sidebar_edge(to_first),
            (Tab::Queue, Focus::Primary) => {
                if to_first {
                    select_first(&mut self.state.queue_cursor, len);
                } else {
                    select_last(&mut self.state.queue_cursor, len);
                }
            }
            (Tab::Settings, _) => {
                let last = crate::app::settings::Setting::ALL.len() - 1;
                self.state.settings_cursor = if to_first { 0 } else { last };
            }
            (Tab::Search, _) => {
                if to_first {
                    self.state.search.results.select_first();
                } else {
                    self.state.search.results.select_last();
                }
            }
            (Tab::Playlists, Focus::Primary) => {
                Self::select_entry_edge(&mut self.state.playlists.list, to_first);
            }
            (Tab::Artists, Focus::Primary) => {
                Self::select_entry_edge(&mut self.state.artists.list, to_first);
            }
            (Tab::Ranks, Focus::Primary) => {
                Self::select_entry_edge(&mut self.state.ranks.list, to_first);
            }
            (Tab::Cloud, Focus::Primary) => {
                Self::select_entry_edge(&mut self.state.cloud.list, to_first);
            }
            (Tab::Sources, Focus::Primary) => {
                let len = self.state.config.sources.ordered().len();
                if to_first {
                    select_first(&mut self.state.sources_cursor, len);
                } else {
                    select_last(&mut self.state.sources_cursor, len);
                }
            }
            (_, Focus::Secondary) => {
                if let Some(list) = self.state.songs_mut() {
                    if to_first {
                        list.select_first();
                    } else {
                        list.select_last();
                    }
                }
            }
        }
    }

    /// Esc：回到上一层。
    pub(super) fn go_back(&mut self) {
        match self.state.focus {
            Focus::Queue => self.state.focus = Focus::Secondary,
            // 歌曲列表是两层浏览的第二层，退回条目列表（搜索页也一样：
            // Secondary 是结果列表，Primary 是输入框）
            Focus::Secondary | Focus::Primary | Focus::Sidebar => {
                self.state.focus = Focus::Primary;
                self.state.sidebar_visible = true;
            }
        }
    }

    // ==================================================================
    // 标签页入口
    // ==================================================================

    /// 数字键 1-9 与 0：切换到对应的标签页。
    ///
    /// 早先这里按焦点区分语义（侧边栏里切页、列表里跳到第 N 项）。但「同一个键
    /// 两种行为」并不直观——用户按下去之前得先想「现在焦点在哪」。列表内定位
    /// 有 `g`/`G` 与 `PgUp`/`PgDn` 已经够用，数字键留给导航更清晰。
    pub(super) fn handle_digit(&mut self, number: u8) {
        if let Some(tab) = Tab::from_number(number) {
            self.switch_tab(tab);
        }
    }

    /// `v`：打开音源管理页。
    ///
    /// 以前这个键是「循环切到下一个音源」，但音源多了之后循环切很盲目——用户
    /// 不知道下一个是谁，切错了还得再切一圈回来。改为打开管理页，把选择摆出来。
    /// 真正的切换动作（`switch_source_to`）由页面里的操作触发。
    pub(super) fn open_sources_page(&mut self) {
        if self.state.tab == Tab::Sources {
            // 已在音源页，再按一次回到搜索页，省得去找数字键
            self.switch_tab(Tab::Search);
            return;
        }
        self.switch_tab(Tab::Sources);
        // 焦点必须落到音源列表上：留在侧边栏的话 j/k 会去切标签页，
        // 用户按了却像没反应。
        self.state.focus = Focus::Primary;
        self.state
            .info("音源管理：Enter 启用/禁用 · E 设为默认 · K/J 调优先级");
    }
}
