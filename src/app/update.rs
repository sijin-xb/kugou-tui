//! 事件 → 状态变更。主线程的调度器。
//!
//! 这是整个程序的「大脑」：所有输入（按键、异步结果、音频事件、定时心跳）都在
//! 这里被翻译成对 [`AppState`] 的修改。它**不**渲染、不阻塞、不直接做 IO——
//! 需要 IO 时一律 `runtime.spawn` 一个任务，结果通过事件总线回来。
//!
//! 这条纪律带来的直接好处：`handle_*` 全是纯同步函数，可以逐个单元测试，
//! 也不会因为某个网络请求卡住而冻结界面。
//!
//! # 这个文件里该留什么
//!
//! 从 4700 行拆到现在的样子，判据一直是**「调用方在哪、结果谁处理」**，不是行数。
//! 留下来的是三类东西：
//!
//! 1. **分派**：`handle_event` / `handle_action` / `activate`。它们按
//!    `(Tab, Focus)` 或模态状态决定该叫谁，横跨多个模块——搬进任何一个模块都会
//!    让那个模块反过来依赖别的模块（`navigation.rs` 顶部那段说明就是为此写的）。
//! 2. **加载层**：`load_*` / `open_selected_*`。`navigation.rs` 的
//!    `ensure_tab_loaded` 只请它们拉数据，自己不发请求。
//! 3. **结果与收尾**：`handle_loaded`（每条异步结果的身份判据都在这里）、
//!    `handle_audio_event`、`tick`。
//!
//! 已经搬走的：`navigation.rs`（导航）、`search.rs`（搜索）、`playback.rs`
//! （播放控制）、`desktop.rs`（MPRIS / 托盘 / 窗口）、`cloud.rs`（登录 / 音源 /
//! VIP / 云端歌单）、`settings.rs`（设置页的三个交互）。
//!
//! 要守的形状是**星形**：`update` 指向所有模块，模块之间互不调用（只回指
//! `update` / `mod` / `settings`）。出现叶子 ↔ 叶子就说明切错了地方，
//! 回去找那个「按 Tab 分派」的函数。

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use ratatui::crossterm::event::MouseEvent;

use crate::api::catalog::PAGE_LIMIT;
use crate::api::model::{Artist, Playlist, RankBoard, Song, format_duration_ms};
use crate::app::App;
use crate::app::settings::quality_label;
use crate::app::state::{
    ConfirmAction, Connection, CoverArt, Focus, HitTarget, HitZone, LoginState, PromptAction,
    PromptState, QualityPicker, Tab,
};
use crate::audio::engine::AudioSource;
use crate::error::AppError;
use crate::source::{PlaylistRef, SourceKind};

use crate::audio::download::{Downloader, PREROLL_BYTES, StreamOutcome};
use crate::audio::engine::{AudioEvent, PlaybackState, SEEK_STEP_MS, VOLUME_STEP};
use crate::audio::spectrum::BAND_COUNT;
use crate::event::{Event, Loaded, LoadingTarget, PlaylistSource, VipClaimOutcome};
use crate::keymap::{Action, CHEATSHEET, KeyMode};
use crate::logger::tlog;

/// 翻页时跳过的行数。
const PAGE_STEP: isize = 10;

/// 下载进度上报的字节间隔。太小会让事件通道被高频消息淹没。
const PROGRESS_STEP_BYTES: u64 = 256 * 1024;

/// 每帧最多处理的事件数，防止事件洪水导致画面完全停止刷新。
const MAX_EVENTS_PER_FRAME: usize = 64;

/// 每隔多少拍测量一次缓存占用。
///
/// 默认 200ms 一拍，50 拍约 10 秒。目录扫描放在阻塞线程池里，
/// 因此这个频率不会影响界面流畅度。
const CACHE_MEASURE_TICKS: u64 = 50;

/// 歌手列表一次取多少个。接口叫 `hotsize`，与「歌曲每页条数」不是一个概念，
/// 因此刻意不跟着 `config.page_size` 走。
const ARTIST_LIST_SIZE: u32 = 60;

/// 封面取图的边长（像素）：URL 里 `{size}` 占位符的替换值。
///
/// 决定的是**下载与解码**的规模，不是显示精度——显示时由 `ratatui-image`
/// 按字符单元格的实际像素尺寸重新缩放。取 256 是因为封面最大只画到 48 列 × 24 行，
/// 常见字号下约合 380 像素见方，再往上下载变慢而肉眼几乎无差别。
const COVER_PIXEL_SIZE: u32 = 256;

/// 展开封面 URL 里的 \`{size}\` 占位符。
fn expand_cover_size(url: &str, size: u32) -> String {
    if url.contains("{size}") {
        url.replace("{size}", &size.to_string())
    } else {
        url.to_string()
    }
}

/// 图片真实宽高比（宽/高）。
///
/// 宽或高为 0、比值算出非有限值时按 1.0（方图）处理——宁可稍微变形，也别让
/// 布局算出 0 列或 NaN 把面板撑坏。
fn image_aspect(image: &image::DynamicImage) -> f32 {
    let (width, height) = (image.width(), image.height());
    if height == 0 || width == 0 {
        return 1.0;
    }
    let aspect = width as f32 / height as f32;
    if aspect.is_finite() && aspect > 0.0 {
        aspect
    } else {
        1.0
    }
}

/// 未登录时各音源该说什么——按音源分引导路径，否则把网易云用户怼到
/// 「去配置 cookie」会让人无所适从（网易云没 cookie 这概念）。
fn not_logged_in_hint(source: SourceKind) -> String {
    match source {
        SourceKind::Netease => "云端歌单需要登录，请按 L 扫码登录网易云".to_string(),
        SourceKind::Kugou | SourceKind::KugouConcept => {
            "云端歌单需要登录，请配置 cookie（--cookie 或配置文件）".to_string()
        }
        // 汽水没有云端歌单（`Capability::cloud` 为假），这个分支走不到；
        // 仍给一条明确的话而不是 unreachable!()——万一将来接上了，
        // 用户看到的是「怎么登录」而不是程序崩溃。
        SourceKind::Sodam => "汽水音乐没有云端歌单功能".to_string(),
    }
}

/// ↑↓ 在当前 (标签页, 焦点) 下是否表示「切歌」。
///
/// 首页与可视化页没有任何可导航的列表，↑↓ 退化成切歌——这是有意为之：
/// 这两页正是「看着歌词 / 频谱听歌」的页面，切歌是最高频的操作。
///
/// 但**焦点在侧边栏或队列时不算**：那两个焦点各有自己的 ↑↓ 语义
/// （侧边栏切栏目、队列移动队列项，见 `App::move_selection`）。
///
/// # 为什么要看焦点
///
/// 早先这里只判标签页。`Tab` 会把焦点轮流交给侧边栏 / 主区 / 队列，于是在
/// 首页按 `Tab` 选中侧边栏之后，↑↓ 走的仍是「切歌」分支：歌换了、侧边栏高亮
/// 纹丝不动。用户看来就是「在导航里按上下键没反应」（其实切歌了，还会突然
/// 换歌，更莫名其妙）。
fn up_down_switches_track(tab: Tab, focus: Focus) -> bool {
    matches!(tab, Tab::Home | Tab::Visualizer) && matches!(focus, Focus::Primary | Focus::Secondary)
}

impl App {
    // ==================================================================
    // 事件分发
    // ==================================================================

    pub fn handle_event(&mut self, event: Event) {
        match event {
            Event::Key(key) => {
                let mode = if self.state.is_editing() {
                    KeyMode::TextInput
                } else {
                    KeyMode::Normal
                };
                let action = crate::keymap::resolve(key, mode);
                // 排查「按键没反应」时这条日志是决定性的：它能区分
                // 「按键根本没到程序」和「到了但映射成了 None」。
                // 默认不输出，用 KUGOU_TUI_DEBUG=1 开启。
                tlog!(
                    crate::logger::LEVEL_DEBUG,
                    "按键 {:?}（模式 {mode:?}）→ {action:?}",
                    key
                );
                self.handle_action(action);
            }
            // 布局每帧按终端实际尺寸重算，无需额外处理
            Event::Resize => {}
            Event::Audio(audio) => self.handle_audio_event(audio),
            Event::Loaded(loaded) => {
                // 先按这次结果更新连通性：它在「已连接」和具体报错之间决定谁留在
                // 状态栏上，必须在 `handle_loaded` 写消息之前跑
                self.note_connection(&loaded);
                self.handle_loaded(*loaded);
            }
            Event::Mouse(mouse) => self.handle_mouse(mouse),
            Event::Tick => self.tick(),
            // 来自 MPRIS（桌面媒体控件）的语义动作，已经是明确意图，直接执行
            Event::Action(action) => self.handle_action(action),
        }
    }

    // ==================================================================
    // 鼠标与列表交互
    // ==================================================================

    /// 鼠标事件：点击选中、双击激活、滚轮移动选中、点击进度条跳转。
    ///
    /// 坐标→行的换算依赖 ui 层每帧回填的命中区（见 `AppState::hit_test`）。
    /// 没有命中任何区域时直接忽略，不做「就近猜测」。
    fn handle_mouse(&mut self, mouse: MouseEvent) {
        use ratatui::crossterm::event::{MouseButton, MouseEventKind};

        match mouse.kind {
            // 悬停反馈：这里只记位置，真正的命中判断交给渲染层（只有它知道行几何）
            MouseEventKind::Moved => {
                self.state.hover = Some((mouse.column, mouse.row));
            }
            MouseEventKind::ScrollDown => self.scroll_list_at(&mouse, 1),
            MouseEventKind::ScrollUp => self.scroll_list_at(&mouse, -1),
            MouseEventKind::Down(MouseButton::Left) => self.click_at(&mouse),
            MouseEventKind::Down(MouseButton::Right) => self.right_click_at(&mouse),
            _ => {}
        }
    }

    /// 滚轮：在光标所在的列表上移动选中。
    fn scroll_list_at(&mut self, mouse: &MouseEvent, delta: isize) {
        let Some(zone) = self.state.hit_test(mouse.column, mouse.row) else {
            return;
        };
        // 进度条和侧边栏不参与列表滚动
        if !matches!(
            zone.target,
            HitTarget::Entries | HitTarget::Songs | HitTarget::Queue
        ) {
            return;
        }
        self.focus_hit_target(zone.target);
        // 一次滚一行。列表是要停在某一首上的，跳着滚会越过目标再往回滚，
        // 看着快、实际更慢——精确比速度重要。
        self.move_selection(delta);
    }

    /// 左键点击。
    fn click_at(&mut self, mouse: &MouseEvent) {
        let Some(zone) = self.state.hit_test(mouse.column, mouse.row) else {
            return;
        };

        match zone.target {
            HitTarget::Tab(index) => {
                if let Some(tab) = Tab::from_sidebar_index(index) {
                    self.switch_tab(tab);
                }
            }
            HitTarget::Entries | HitTarget::Songs | HitTarget::Queue => {
                self.click_list_row(zone, mouse)
            }
            HitTarget::Progress => self.click_progress(zone, mouse),
            HitTarget::LyricLine => self.click_lyric(zone, mouse),
            HitTarget::Settings => self.click_setting(zone, mouse),
            HitTarget::VipClaim => self.claim_daily_vip(),
            HitTarget::ProfileRetry => {
                self.state.info("正在重新获取用户资料…");
                self.fetch_user_info();
            }
        }
    }

    /// 右键：在光标那一行上弹出歌曲菜单。
    ///
    /// 左键已经占了「选中 / 双击播放」，右键补的是「对这一首歌还能做什么」——
    /// 与键盘的 `;` 完全等价，见 `open_context_menu`。
    ///
    /// 菜单里的每一项都标着对应的键位，所以它**不是另一套交互**，只是把键位
    /// 集中到光标处；菜单里也不会出现没有键位对应的操作。
    fn right_click_at(&mut self, mouse: &MouseEvent) {
        let Some(zone) = self.state.hit_test(mouse.column, mouse.row) else {
            return;
        };
        let Some(index) = zone.index_at(mouse.row) else {
            return;
        };

        // 先把光标下那一行选中——菜单是「对这首歌操作」，选中的也必须是它
        match zone.target {
            HitTarget::Entries => {
                self.state.select_entry_index(index);
                self.focus_hit_target(zone.target);
            }
            HitTarget::Songs => {
                self.state.select_song_index(index);
                self.focus_hit_target(zone.target);
            }
            HitTarget::Queue => {
                self.focus_hit_target(zone.target);
                self.state.queue_cursor.select(Some(index));
            }
            HitTarget::Tab(_)
            | HitTarget::Progress
            | HitTarget::LyricLine
            | HitTarget::Settings
            | HitTarget::VipClaim
            | HitTarget::ProfileRetry => {
                return;
            }
        }

        self.open_context_menu();
    }

    /// 点击列表行：选中，双击则激活（等同 Enter）。
    fn click_list_row(&mut self, zone: HitZone, mouse: &MouseEvent) {
        let Some(index) = zone.index_at(mouse.row) else {
            return;
        };

        // 双击判定要在覆盖 last_click 之前做
        let double = self.state.is_double_click(zone.target, Some(index));

        match zone.target {
            HitTarget::Entries => self.state.select_entry_index(index),
            HitTarget::Songs => self.state.select_song_index(index),
            HitTarget::Queue => {
                self.focus_hit_target(zone.target);
                self.state.queue_cursor.select(Some(index));
            }
            HitTarget::Tab(_)
            | HitTarget::Progress
            | HitTarget::LyricLine
            | HitTarget::Settings
            | HitTarget::VipClaim
            | HitTarget::ProfileRetry => {
                return;
            }
        }

        self.focus_hit_target(zone.target);
        self.state.set_last_click(zone.target, Some(index));

        if double {
            self.activate();
        }
    }

    /// 点击进度条：按 x 比例跳转到对应时间。
    fn click_progress(&mut self, zone: HitZone, mouse: &MouseEvent) {
        let duration_ms = self.state.duration_ms;
        if duration_ms == 0 || zone.rect.width == 0 {
            return;
        }

        let ratio =
            f64::from(mouse.column.saturating_sub(zone.rect.left())) / f64::from(zone.rect.width);
        let target = (ratio * duration_ms as f64) as u64;
        let target = target.min(duration_ms.saturating_sub(1));
        self.audio.seek_to(target);
        // 乐观更新，让进度条立刻响应
        self.state.position_ms = target;
    }

    /// 点击歌词行：跳到这一句的起始时间。
    ///
    /// 命中区记的是**显示行**（译文/音译各占一行），而跳转要的是**歌词行**，
    /// 所以经 `display_line_index` 换一次下标——那个映射由渲染层每帧回填，
    /// 因为只有它知道这次滚动到了哪里。
    fn click_lyric(&mut self, zone: HitZone, mouse: &MouseEvent) {
        let Some(display) = zone.index_at(mouse.row) else {
            return;
        };
        let Some(line_index) = self.state.lyric.line_index_at_display(display) else {
            return;
        };
        let Some(line) = self.state.lyric.lyric.lines.get(line_index) else {
            return;
        };

        // **要加回歌词偏移**：当前行是按 `position - lyric_offset_ms` 算出来的，
        // 直接跳到 `line.time_ms` 会正好差一句。偏移为 0 时看不出，调过的人一眼
        // 就发现。
        self.seek_to(lyric_seek_target(
            line.time_ms,
            self.state.config.lyric_offset_ms,
        ));
    }

    /// 把焦点切到命中区对应的面板。离开搜索框时退出输入态，否则字母键会被吞掉。
    fn focus_hit_target(&mut self, target: HitTarget) {
        let focus = match target {
            HitTarget::Entries => Focus::Primary,
            HitTarget::Songs => Focus::Secondary,
            HitTarget::Queue => Focus::Queue,
            HitTarget::Settings => Focus::Primary,
            // 领取 VIP 是一行即时动作，不改变焦点——点完继续看首页
            // 歌词行同理：点它是「跳到这句」，跳完还在原处看歌词
            HitTarget::Tab(_)
            | HitTarget::Progress
            | HitTarget::LyricLine
            | HitTarget::VipClaim
            | HitTarget::ProfileRetry => return,
        };

        if self.state.focus == Focus::Primary && focus != Focus::Primary {
            self.state.search.editing = false;
        }
        self.state.focus = focus;
    }

    // ==================================================================
    // 歌曲右键菜单
    // ==================================================================

    /// 菜单打开时的按键：移动、执行、关闭。
    fn handle_menu_key(&mut self, action: Action) {
        let Some(menu) = self.state.context_menu.as_mut() else {
            return;
        };

        match action {
            Action::MoveUp => menu.move_cursor(-1),
            Action::MoveDown => menu.move_cursor(1),
            Action::MoveTop => menu.cursor = 0,
            Action::MoveBottom => menu.cursor = menu.items.len().saturating_sub(1),
            Action::Cancel | Action::Help => self.state.context_menu = None,
            Action::Submit | Action::PlayPause => {
                let Some(selected) = menu.selected() else {
                    return;
                };
                let song = menu.song.clone();
                self.state.context_menu = None;
                self.run_menu_action(selected, &song);
            }
            // 数字键直接选中并执行，和列表的 1-9 一个手感
            Action::Char(digit @ '1'..='9') => {
                let index = (digit as u8 - b'1') as usize;
                if let Some(selected) = menu.items.get(index).copied() {
                    let song = menu.song.clone();
                    self.state.context_menu = None;
                    self.run_menu_action(selected, &song);
                }
            }
            _ => {}
        }
    }

    /// 打开当前焦点歌曲的菜单。
    fn open_context_menu(&mut self) {
        let Some(song) = self.state.selected_song() else {
            self.state.info("这里没有可对它操作的歌曲");
            return;
        };
        let in_queue = self.state.focus == crate::app::state::Focus::Queue;
        self.state.context_menu = Some(crate::app::state::ContextMenu::new(song, in_queue));
    }

    /// 执行菜单项。每一项都转调已有的动作实现——菜单**不是**第二套逻辑，
    /// 只是把键位集中到光标处，这样键盘和鼠标走的是同一条代码路径。
    fn run_menu_action(&mut self, action: crate::app::state::MenuAction, song: &Song) {
        use crate::app::state::MenuAction;

        match action {
            MenuAction::Play => self.start_playback(song.clone(), 0),
            MenuAction::QueueAppend => {
                let label = describe_song(song);
                self.state.queue.append(song.clone());
                self.clamp_queue_cursor();
                self.state.success(format!("已加入队列：{label}"));
            }
            MenuAction::QueuePlayNext => {
                let label = describe_song(song);
                self.state.queue.insert_next(song.clone());
                self.clamp_queue_cursor();
                self.state.success(format!("已插入到下一首：{label}"));
            }
            // 打开菜单时焦点已经是这首歌，所以沿用「收藏焦点歌曲」那条路径
            MenuAction::AddToCloud => self.add_focused_song_to_cloud(),
            MenuAction::Download => self.open_quality_picker(song.clone()),
            MenuAction::RemoveFromQueue => self.remove_song_from_queue(song),
        }
    }

    /// 菜单里的「从队列移除」：按下标移除，而不是动当前游标。
    ///
    /// 菜单可以作用在队列里**任意**一首上，而 `x`（remove_selected_from_queue）
    /// 只认当前选中那首——直接复用的话，右键第 5 首会删掉第 2 首。
    fn remove_song_from_queue(&mut self, song: &Song) {
        let Some(index) = self
            .state
            .queue
            .items()
            .iter()
            .position(|item| item.hash == song.hash)
        else {
            self.state.warn("这首歌不在播放队列里");
            return;
        };
        if let Some(removed) = self.state.queue.remove(index) {
            self.state.info(format!("已从队列移除《{}》", removed.name));
        }
    }

    // ==================================================================
    // 下载：预取下一首、把单曲存到下载目录
    // ==================================================================

    /// 把队列里的下一首提前下载到缓存（**不播放、不打扰界面**）。
    ///
    /// 只做一件事：如果下一首还没缓存，就在后台把它下下来。这样用户按 `n` 切歌时
    /// 直接命中缓存，不用再等一次几十秒的下载（Hi-Res 那档实测 65 MB，首次播放
    /// 必然是「转圈半分钟」；预取之后切歌就是秒开）。
    ///
    /// # 为什么不复用 `request_stream`
    ///
    /// 那条路径下载完会 emit `StreamCached`，而它的处理函数会把「不是当前这首歌」
    /// 的结果当孤儿**删掉**——预取的正是「下一首」，会被自己删掉。而且它会去
    /// `audio.load()`，等于打断正在放的这首。所以这里单独走一条静默路径：
    /// 不设 `download_progress` / `busy`（那会覆盖掉当前播放的进度显示），
    /// 成功失败都只记日志。
    fn prefetch_next(&mut self) {
        let len = self.state.queue.len();
        if len < 2 {
            return;
        }
        let current = self.state.queue_cursor.selected().unwrap_or(0).min(len - 1);
        let Some(next) = self.state.queue.items().get(current + 1).cloned() else {
            return;
        };

        let quality = self.state.config.quality.clone();
        // 已缓存就别重复下了
        if self.cache.find(&next.cache_key(&quality)).is_some() {
            return;
        }

        let source = next.source;
        let api = match self.client_for(source) {
            Ok(client) => client,
            Err(_) => return, // 预取失败无所谓，不该弹错误打扰用户
        };
        let downloader = self.downloader.clone();
        let cache = self.cache.clone();

        self.runtime.spawn(async move {
            let Ok(stream) = source.song_stream_url(&api, &next, &quality).await else {
                return;
            };
            let key = next.cache_key(&quality);
            let target = cache.path_for(&key, Downloader::extension_from_url(&stream.url));
            if target.exists() {
                return;
            }
            match downloader
                .fetch_to(&stream.url, &target, &|_received, _total| {})
                .await
            {
                Ok(bytes) => tlog!(
                    crate::logger::LEVEL_DEBUG,
                    "预取《{}》完成（{} 字节）",
                    next.name,
                    bytes
                ),
                Err(error) => tlog!(
                    crate::logger::LEVEL_DEBUG,
                    "预取《{}》失败：{}",
                    next.name,
                    error.user_hint()
                ),
            }
        });
    }

    /// 把当前播放的歌曲下载到设置里的目录。
    ///
    /// 设计上的几个选择：
    ///
    /// * **同名不覆盖**：目标文件已存在就跳过并提示，让用户改名或换目录——避免
    ///   「下载中途被覆盖丢半首」。
    /// * **文件名**：`<歌手> - <歌名>.<ext>`；扩展名从直链 URL 末段推断（`.mp3`
    ///   / `.flac` / `.m4a`）。这样手动打开就能识别格式。
    /// * **异步**：下载可能几十秒，阻塞主循环会让整个 TUI 卡住，所以 spawn 出去。
    fn download_current(&mut self) {
        let Some(song) = self.state.current.clone() else {
            self.state.warn("当前没有在播放的歌曲");
            return;
        };
        self.open_quality_picker(song);
    }

    /// 打开音质选择框。选完之后走 [`Self::download_song`]。
    ///
    /// 不直接用全局音质：那个是**播放**音质（要照顾流量和缓冲），下载到
    /// 文件夹通常想要无损——为下一首歌去改全局设置太别扭。
    fn open_quality_picker(&mut self, song: Song) {
        let current = self.state.config.quality.trim().to_string();
        self.state.quality_picker = Some(QualityPicker::new(song, &current));
    }

    /// 用选中的音质下载。
    fn confirm_quality_picker(&mut self) {
        let Some(picker) = self.state.quality_picker.take() else {
            return;
        };
        let Some(quality) = picker.selected().map(str::to_string) else {
            return;
        };
        let song = picker.song.clone();
        self.download_song(song, quality);
    }

    /// 下载指定歌曲。菜单里点的歌不一定是当前在放的那首，所以这里收一首歌
    /// 而不是读 `state.current`。
    fn download_song(&mut self, song: Song, quality: String) {
        let dir =
            crate::app::settings::expand_download_dir(self.state.config.download_dir.as_deref());
        let target_dir = std::path::PathBuf::from(&dir);
        let label = format!("{} - {}", song.singer_text(), song.name);

        // 文件名清洗：去掉路径分隔符和控制字符，避免把歌名变成子目录或
        // 让 OS 拒绝写入。保留字母、数字、汉字、空格、常见标点。
        let sanitized = crate::app::settings::sanitize_filename(&label);

        let api = match self.client_for(song.source) {
            Ok(client) => client,
            Err(error) => {
                self.state
                    .error(format!("无法连接「{}」：{error}", song.source.label()));
                return;
            }
        };
        let downloader = self.downloader.clone();
        let bus = self.bus.clone();

        self.runtime.spawn(async move {
            let stream = match song.source.song_stream_url(&api, &song, &quality).await {
                Ok(stream) => stream,
                Err(error) => {
                    bus.fail(format!("获取《{}》的播放地址失败", song.name), error);
                    return;
                }
            };

            // 文件扩展名：从 URL 路径末段里取。免费试听是 `.m4a`/`.mp3`/`.flac`，
            // 直链里看得到。取不到就退到 `.mp3`——绝大多数情况是 mp3。
            let ext = std::path::Path::new(&stream.url)
                .extension()
                .and_then(|os| os.to_str())
                .filter(|ext| matches!(*ext, "mp3" | "flac" | "m4a"))
                .unwrap_or("mp3")
                .to_string();
            let file_name = format!("{sanitized}.{ext}");
            let target = target_dir.join(&file_name);

            if target.exists() {
                bus.emit(Loaded::CloudNotice(format!(
                    "《{}》已存在：{}（请换目录或改名）",
                    song.name,
                    target.display()
                )));
                return;
            }

            let dir_label = target_dir.display().to_string();
            match downloader
                .fetch_to(
                    &stream.url,
                    &target,
                    &(|received, total| {
                        let _ = total;
                        if received % (256 * 1024) == 0 {
                            crate::logger::tlog!(
                                crate::logger::LEVEL_DEBUG,
                                "下载 {file_name} {received}/{:?}",
                                total
                            );
                        }
                    }),
                )
                .await
            {
                Ok(written) => bus.emit(Loaded::CloudNotice(format!(
                    "已下载《{}》（{} MiB → {}）",
                    song.name,
                    written / 1024 / 1024,
                    dir_label
                ))),
                Err(error) => {
                    bus.fail(format!("下载《{}》失败", song.name), error);
                }
            }
        });
    }

    // ==================================================================
    // 按键分派：模态优先级 → 编辑态 → 浏览态
    // ==================================================================

    /// 处理一个按键动作。
    pub fn handle_action(&mut self, action: Action) {
        // 右键菜单是模态的：打开的期间所有按键都归它，避免菜单开着还能
        // 操作底下的列表（那样选中的歌和菜单目标的歌会不一致）
        if self.state.context_menu.is_some() {
            self.handle_menu_key(action);
            return;
        }

        // 帮助面板是模态的：只放行滚动与关闭。
        //
        // `CHEATSHEET` 有 38 条，而 34 行的终端只放得下 26 条，所以滚动必须放行。
        // 早先这里只认「关闭」，`j`/`k`/PgDn 全被吞掉，最后 12 条——也就是整块
        // 播放控制（Space / n·p / ←·→ / +·- / m / r / l / [·] / W）——永远看不到。
        if self.state.help.is_open() {
            let total = CHEATSHEET.len();
            match action {
                Action::MoveUp => self.state.help.scroll_by(-1, total),
                Action::MoveDown => self.state.help.scroll_by(1, total),
                Action::MoveTop => self.state.help.scroll_to_top(),
                Action::MoveBottom => self.state.help.scroll_to_bottom(total),
                Action::PageUp => self.state.help.scroll_page(-1, total),
                Action::PageDown => self.state.help.scroll_page(1, total),
                Action::Quit => self.state.should_quit = true,
                Action::ForceQuit => {
                    self.state.should_quit = true;
                    self.state.force_quit = true;
                }
                Action::Help | Action::Cancel => self.state.help.close(),
                // 其余按键一律吞掉：面板是模态的，不能让底下的列表跟着动
                _ => {}
            }
            return;
        }

        // 下载音质选择框是模态的：只放行移动、确认、取消与退出。
        // 和登录选择器同样的处理方式——不持有 picker 的借用再去调 self 的方法。
        if self.state.quality_picker.is_some() {
            match action {
                Action::MoveUp => {
                    if let Some(picker) = self.state.quality_picker.as_mut() {
                        picker.move_by(-1);
                    }
                }
                Action::MoveDown => {
                    if let Some(picker) = self.state.quality_picker.as_mut() {
                        picker.move_by(1);
                    }
                }
                Action::Submit => self.confirm_quality_picker(),
                Action::Cancel => {
                    self.state.quality_picker = None;
                    self.state.info("已取消下载");
                }
                Action::Quit => self.state.should_quit = true,
                Action::ForceQuit => {
                    self.state.should_quit = true;
                    self.state.force_quit = true;
                }
                _ => {}
            }
            return;
        }

        // 登录音源选择器是模态的：只放行移动、确认、取消与退出。
        // 这里不持有 picker 的借用再去调 self 的方法（会冲突），
        // 只在需要改选中时才短暂借用。
        if self.state.login_picker.is_some() {
            match action {
                Action::MoveUp => {
                    if let Some(picker) = self.state.login_picker.as_mut() {
                        picker.move_by(-1);
                    }
                }
                Action::MoveDown => {
                    if let Some(picker) = self.state.login_picker.as_mut() {
                        picker.move_by(1);
                    }
                }
                Action::Submit => self.confirm_login_source(),
                Action::Cancel => {
                    self.state.login_picker = None;
                    self.state.info("已取消登录");
                }
                Action::Quit => self.state.should_quit = true,
                Action::ForceQuit => {
                    self.state.should_quit = true;
                    self.state.force_quit = true;
                }
                _ => {}
            }
            return;
        }

        // 设置页：← → 是「改值」。这一页没有输入框也没有播放进度，
        // 让方向键去快进/快退的话，用户在这一页就只剩 Enter 能用，很别扭。
        //
        // 方向键在别处映射成 SeekForward / SeekBackward（快进快退），
        // CursorLeft / CursorRight 才是文本光标，所以两种都要接。
        if self.state.tab == Tab::Settings {
            match action {
                Action::CursorLeft | Action::SeekBackward => return self.adjust_setting(-1),
                Action::CursorRight | Action::SeekForward => return self.adjust_setting(1),
                _ => {}
            }
        }

        // 确认对话框优先于一切：有未决确认时其它按键一律拦截。
        if let Some(confirm) = self.state.pending_confirm {
            match action {
                Action::Submit => {
                    self.state.pending_confirm = None;
                    match confirm {
                        ConfirmAction::ClearQueue => self.clear_queue(),
                        ConfirmAction::DeleteCloudPlaylist => self.delete_cloud_playlist(),
                        ConfirmAction::ClearCache => self.clear_cache(),
                        ConfirmAction::Relogin => self.relogin(),
                    }
                }
                Action::Cancel | Action::Char('n') | Action::Char('N') => {
                    self.state.pending_confirm = None;
                    self.state.info("已取消");
                }
                _ => {}
            }
            return;
        }

        // 文本输入弹窗（新建歌单）：拦截所有按键，只处理文本编辑、提交与取消。
        if let Some(prompt) = self.state.prompt.as_ref() {
            let action_kind = prompt.action;
            match action {
                // 字符与编辑动作直接转给输入框。
                //
                // 这些键已经在 `resolve()` 里按 `KeyMode::TextInput` 解析（见
                // `AppState::is_editing` 把 prompt 也算作输入态），所以自定义
                // 键位不会再把字母换成动作；`resolve_text_input` 顺带给出了
                // Delete / Home / End / 光标移动，不用在这里重复实现。
                Action::Char(character) => {
                    if let Some(prompt) = self.state.prompt.as_mut() {
                        prompt.buffer.insert(character);
                    }
                }
                Action::Backspace => {
                    if let Some(prompt) = self.state.prompt.as_mut() {
                        prompt.buffer.backspace();
                    }
                }
                Action::Delete => {
                    if let Some(prompt) = self.state.prompt.as_mut() {
                        prompt.buffer.delete();
                    }
                }
                Action::CursorLeft => {
                    if let Some(prompt) = self.state.prompt.as_mut() {
                        prompt.buffer.move_left();
                    }
                }
                Action::CursorRight => {
                    if let Some(prompt) = self.state.prompt.as_mut() {
                        prompt.buffer.move_right();
                    }
                }
                Action::CursorHome => {
                    if let Some(prompt) = self.state.prompt.as_mut() {
                        prompt.buffer.move_home();
                    }
                }
                Action::CursorEnd => {
                    if let Some(prompt) = self.state.prompt.as_mut() {
                        prompt.buffer.move_end();
                    }
                }
                Action::Submit => {
                    let Some(prompt) = self.state.prompt.take() else {
                        return;
                    };
                    let name = prompt.buffer.text().trim().to_string();
                    match action_kind {
                        PromptAction::CreateCloudPlaylist => self.create_cloud_playlist(name),
                    }
                }
                Action::Cancel => {
                    self.state.prompt = None;
                    self.state.info("已取消");
                }
                Action::Quit => self.state.should_quit = true,
                Action::ForceQuit => {
                    self.state.should_quit = true;
                    self.state.force_quit = true;
                }
                _ => {}
            }
            return;
        }

        // 登录弹窗是模态的：只放行取消与退出，避免二维码显示时误触其它功能。
        if self.state.login.is_some() {
            match action {
                Action::Cancel => {
                    // 弹窗里已经有结果消息了（成功 / 失败 / 二维码过期）的话，
                    // Esc 只是关掉弹窗——别再追加一句「已取消登录」，跟原文打架。
                    // 真正「用户在二维码未完成时主动取消」是更早的状态。
                    let finished = self
                        .state
                        .login
                        .as_ref()
                        .is_some_and(|state| state.finished);
                    self.state.login = None;
                    if !finished {
                        self.state.info("已取消登录");
                    }
                }
                Action::Quit => self.state.should_quit = true,
                Action::ForceQuit => {
                    self.state.should_quit = true;
                    self.state.force_quit = true;
                }
                _ => {}
            }
            return;
        }

        if self.state.is_editing() && self.handle_editing_action(action) {
            return;
        }
        self.handle_normal_action(action);
    }

    /// 输入态按键。返回 `true` 表示已消费。
    fn handle_editing_action(&mut self, action: Action) -> bool {
        let input = &mut self.state.search.input;
        match action {
            Action::Char(character) => input.insert(character),
            Action::Backspace => input.backspace(),
            Action::Delete => input.delete(),
            Action::CursorLeft => input.move_left(),
            Action::CursorRight => input.move_right(),
            Action::CursorHome => input.move_home(),
            Action::CursorEnd => input.move_end(),
            Action::Submit => self.run_search(),
            // Esc 退出输入态但保留已输入内容，再按一次 Enter 可继续编辑
            Action::Cancel => self.state.search.editing = false,
            _ => return false,
        }
        true
    }

    /// 浏览态下的按键分发。
    fn handle_normal_action(&mut self, action: Action) {
        match action {
            Action::None => {}
            Action::Quit => self.state.should_quit = true,
            Action::ForceQuit => {
                self.state.should_quit = true;
                self.state.force_quit = true;
            }
            Action::Help => self.state.help.open(),
            Action::ContextMenu => self.open_context_menu(),
            Action::SwitchSource => self.open_sources_page(),
            Action::SetDefaultSource => self.set_default_source(),
            Action::RaiseSourcePriority => self.shift_source_priority(true),
            Action::LowerSourcePriority => self.shift_source_priority(false),
            Action::ToggleSidebar => {
                self.state.sidebar_visible = !self.state.sidebar_visible;
            }
            // 托盘菜单触发：把 TUI 从平铺布局里收起来（或放回去），音乐照常播。
            Action::ToggleWindow => self.toggle_window(),
            // 数字键：焦点在侧边栏时切标签页，在列表里时跳到第 N 项
            Action::Digit(number) => self.handle_digit(number),
            Action::FocusNext => self.state.cycle_focus(true),
            Action::FocusPrev => self.state.cycle_focus(false),

            // 首页 / 可视化没有可导航的列表，↑↓ 在这两页原本是空的——划给播放
            // 控制：这两页正是「看着歌词 / 频谱听歌」的页面，切歌是最高频的操作。
            // 有列表的页面（搜索 / 歌单 / 歌手 / 榜单 / 云端 / 队列 / 设置）↑↓
            // 依旧是列表导航，抢走它等于抢走核心交互。
            //
            // 焦点在**侧边栏**或**队列**时不能抢：那两个焦点各有自己的 ↑↓ 语义
            // （见 `move_selection`）。少了这个条件，Tab 键把焦点交给侧边栏之后
            // ↑↓ 就会切歌，而高亮还停在侧边栏上——看起来像程序没反应。
            Action::MoveDown if up_down_switches_track(self.state.tab, self.state.focus) => {
                self.next_track(true);
            }
            Action::MoveUp if up_down_switches_track(self.state.tab, self.state.focus) => {
                self.previous_track();
            }
            Action::MoveDown => self.move_selection(1),
            Action::MoveUp => self.move_selection(-1),
            Action::MoveTop => self.move_selection_edge(true),
            Action::MoveBottom => self.move_selection_edge(false),
            Action::PageDown => self.move_selection(PAGE_STEP),
            Action::PageUp => self.move_selection(-PAGE_STEP),

            Action::Submit => self.activate(),
            Action::Cancel => self.go_back(),

            Action::PlayPause => self.toggle_playback(),
            Action::Next => self.next_track(true),
            Action::Prev => self.previous_track(),
            // 焦点在侧边栏时，左右方向键用来在导航与列表之间移动焦点；
            // 只有焦点已经落在列表/队列里，它们才是快进快退。
            Action::SeekForward => {
                if self.state.focus == Focus::Sidebar {
                    self.state.cycle_focus(true);
                } else {
                    self.seek_by(SEEK_STEP_MS);
                }
            }
            Action::SeekTo(position_ms) => self.seek_to(position_ms),
            // MPRIS 的 Seek：一次带数值的相对跳转（见 keymap::Action::SeekBy）
            Action::SeekBy(delta_ms) => self.seek_by(delta_ms),
            Action::LoadMoreSearch => self.load_more_search(),
            Action::SeekBackward => {
                if self.state.focus == Focus::Sidebar {
                    self.state.cycle_focus(false);
                } else {
                    self.seek_by(-SEEK_STEP_MS);
                }
            }
            Action::VolumeUp => self.adjust_volume(VOLUME_STEP),
            Action::VolumeDown => self.adjust_volume(-VOLUME_STEP),
            Action::ToggleMute => self.toggle_mute(),
            Action::CyclePlaybackMode => {
                let mode = self.state.queue.cycle_mode();
                self.state.config.playback_mode = mode;
                self.state.info(format!("播放模式：{}", mode.label()));
            }
            // 音质只在**下一首**生效：当前这首歌的直链与缓存都是按旧档位取的，
            // 中途换档没有意义。提示里说清楚，否则用户会以为没生效。
            Action::CycleQuality => {
                let current = self.state.config.quality.as_str();
                let index = crate::config::SUPPORTED_QUALITIES
                    .iter()
                    .position(|quality| *quality == current)
                    .unwrap_or(0);
                let next = crate::config::SUPPORTED_QUALITIES
                    [(index + 1) % crate::config::SUPPORTED_QUALITIES.len()];
                self.state.config.quality = next.to_string();

                if let Err(error) = self.state.config.save() {
                    self.state
                        .warn(format!("音质已切换，但保存配置失败：{error}"));
                }
                self.state
                    .success(format!("音质：{}（下一首生效）", quality_label(next)));
            }
            Action::ToggleLyricPanel => {
                self.state.show_lyric_panel = !self.state.show_lyric_panel;
            }
            Action::LyricDelay => self.adjust_lyric_offset(-100),
            Action::LyricAdvance => self.adjust_lyric_offset(100),

            Action::OpenSearch => {
                self.switch_tab(Tab::Search);
                self.state.focus = Focus::Primary;
                self.state.search.editing = true;
            }
            Action::OpenSettings => {
                self.switch_tab(Tab::Settings);
                self.state.focus = Focus::Primary;
            }
            Action::DownloadCurrent => self.download_current(),
            Action::Reload => self.reload_current_tab(),
            Action::QueueAppend => self.queue_focused_song(false),
            Action::AddAllToQueue => self.queue_all_songs(),
            Action::QueuePlayNext => self.queue_focused_song(true),
            Action::RemoveFromQueue => self.remove_selected_from_queue(),
            // 清空队列是破坏性操作，先进确认流程，避免误按一下就把整个队列清掉。
            Action::ClearQueue => {
                self.state.pending_confirm = Some(ConfirmAction::ClearQueue);
            }
            // 删文件不可恢复，同样先确认
            Action::ClearCache => {
                self.state.pending_confirm = Some(ConfirmAction::ClearCache);
            }
            Action::ToggleSortOrder => self.toggle_sort_order(),
            Action::OpenRanks => self.switch_tab(Tab::Ranks),
            Action::OpenCloud => self.switch_tab(Tab::Cloud),
            Action::Login => self.start_login(),
            Action::ClaimVip => self.claim_daily_vip(),
            Action::CycleArtistFilter => self.cycle_artist_filter(),
            Action::SyncToCloud => self.sync_queue_to_cloud(),
            Action::AddToCloud => self.add_focused_song_to_cloud(),
            Action::RemoveFromCloud => self.remove_focused_song_from_cloud(),
            // 删除歌单是破坏性操作，先进确认流程
            Action::DeleteCloudPlaylist => {
                if self.state.sync_target.is_none() {
                    self.state.warn("请先在「云端」标签页选中一个歌单");
                    return;
                }
                self.state.pending_confirm = Some(ConfirmAction::DeleteCloudPlaylist);
            }
            Action::NewCloudPlaylist => {
                self.state.prompt = Some(PromptState::new(
                    "新建云端歌单",
                    PromptAction::CreateCloudPlaylist,
                ));
                self.state.info("输入歌单名称，Enter 创建 · Esc 取消");
            }

            // 这些动作只在输入态有意义，浏览态下忽略
            Action::Char(_)
            | Action::Backspace
            | Action::Delete
            | Action::CursorLeft
            | Action::CursorRight
            | Action::CursorHome
            | Action::CursorEnd => {}
        }
    }

    // ==================================================================
    // 输入意图分派：跨模块的入口
    //
    // `activate` 与 `startup_search` 都不是单一模块的事，所以住在分派层：
    //
    // * `activate`：Enter 键按 `(Tab, Focus)` 决定该搜索、该播放还是该打开。
    //   它原本住在 `navigation.rs` 里，结果让 navigation 同时依赖 search 与
    //   playback——一个「导航」模块去调播放，边界就说不通了。
    // * `startup_search`：`--search` 的启动胶水（切到搜索页 + 填词 + 提交），
    //   同样横跨两个模块；留在 `search.rs` 会让 search 反过来依赖 navigation。
    // ==================================================================

    /// Enter：进入下一层，或播放选中歌曲。
    fn activate(&mut self) {
        match (self.state.tab, self.state.focus) {
            // 首页与可视化页都是纯展示页，方向键与 Enter 在它们上面没有意义
            (Tab::Home | Tab::Visualizer, _) => {}
            // 队列页：Enter 播放选中的那首（与焦点在队列时一致）
            (Tab::Queue, _) => self.play_from_queue(),
            // 音源页：Enter = 启用 / 禁用
            (Tab::Sources, _) => self.toggle_source_enabled(),
            // 设置页：Enter = 把选中项往前调一档
            (Tab::Settings, _) => self.adjust_setting(1),
            // 搜索框还没进入编辑态时，Enter 先聚焦输入框
            (Tab::Search, Focus::Primary) => {
                if self.state.search.editing {
                    self.run_search();
                } else if self.state.search.input.is_empty() {
                    self.state.search.editing = true;
                } else {
                    self.run_search();
                }
            }
            (Tab::Search, Focus::Secondary) => self.play_from_focused_songs(),
            (Tab::Playlists, Focus::Primary) => self.open_selected_playlist(),
            (Tab::Playlists, Focus::Secondary) => self.play_from_focused_songs(),
            (Tab::Artists, Focus::Primary) => self.open_selected_artist(),
            (Tab::Artists, Focus::Secondary) => self.play_from_focused_songs(),
            (Tab::Ranks, Focus::Primary) => self.open_selected_rank(),
            (Tab::Ranks, Focus::Secondary) => self.play_from_focused_songs(),
            (Tab::Cloud, Focus::Primary) => self.open_selected_cloud_playlist(),
            (Tab::Cloud, Focus::Secondary) => self.play_from_focused_songs(),
            (_, Focus::Queue) => self.play_from_queue(),
            (_, Focus::Sidebar) => self.state.focus = Focus::Primary,
        }
    }

    /// 启动时带关键词直接进入搜索结果页（对应 `--search`）。
    pub fn startup_search(&mut self, keyword: &str) {
        self.switch_tab(Tab::Search);
        self.state.search.input.set(keyword);
        self.run_search();
    }

    // ==================================================================
    // 共享辅助：按音源构造 HTTP 客户端
    // ==================================================================

    /// 为**指定音源**构造 HTTP 客户端。
    ///
    /// 队列允许跨音源：播放队列里的歌时不能想当然地用「当前音源」的客户端——
    /// 那会把请求发到另一个服务上，而这首歌的 hash 在那个平台根本查不到。
    /// 地址与凭据都取自目标音源自己的档案。
    pub(super) fn client_for(
        &self,
        kind: SourceKind,
    ) -> crate::error::Result<crate::api::ApiClient> {
        // 汽水的应用签名凭证要在**造客户端时**同步到进程级槽位：它不是
        // HTTP 层能表达的东西（不是 cookie、也不是 URL 参数），
        // 而分派层的方法签名统一只收 `&ApiClient`。
        //
        // 这里顺带**无条件**同步（而不是只在 `kind == Sodam` 时）：切歌、
        // 跨音源播放都会经过这个函数，而凭证是「本机的汽水身份」，
        // 只有一个值。每次都写同一个值，开销可以忽略。
        crate::source::sodam::set_active_credentials(self.state.config.sources.sodam_app.clone());

        if kind == self.state.config.active_source_kind() {
            // 当前音源已经有现成的客户端，直接复用（省一次连接池重建）
            return Ok(self.api.clone());
        }
        let profile = self.state.config.sources.profile(kind);
        crate::api::ApiClient::new(
            &profile.api_base,
            profile.cookie_header(kind),
            self.state.config.proxy.as_deref(),
        )
    }

    // ==================================================================
    // 数据加载
    // ==================================================================

    pub fn load_plaza_playlists(&mut self) {
        let api = self.api.clone();
        let active_source = self.state.config.active_source_kind();
        let bus = self.bus.clone();
        let (category, page_size) = (self.state.playlists.category, self.state.config.page_size);

        self.state.playlists.list.load.begin();
        self.state.busy = Some("载入歌单广场".to_string());

        self.runtime.spawn(async move {
            match active_source
                .plaza_playlists(&api, category, 1, page_size)
                .await
            {
                Ok(items) => bus.emit(Loaded::Playlists {
                    category,
                    title: "歌单广场".to_string(),
                    items,
                }),
                Err(error) => bus.fail_loading(LoadingTarget::Playlists, "载入歌单广场失败", error),
            }
        });
    }

    pub fn load_artists(&mut self) {
        let api = self.api.clone();
        let active_source = self.state.config.active_source_kind();
        let bus = self.bus.clone();
        let kind = self.state.artists.kind;

        self.state.artists.list.load.begin();
        self.state.busy = Some("载入歌手列表".to_string());

        self.runtime.spawn(async move {
            match active_source
                .artist_list(&api, kind, ARTIST_LIST_SIZE)
                .await
            {
                Ok(artists) => bus.emit(Loaded::Artists { kind, artists }),
                Err(error) => bus.fail_loading(LoadingTarget::Artists, "载入歌手列表失败", error),
            }
        });
    }

    /// `f`：在歌手列表里轮换地区筛选。
    ///
    /// 取值含义见 KuGouMusicApi 文档的 `/artist/lists`：0 全部 / 1 华语 / 2 欧美 /
    /// 3 日韩 / 4 其他。切换后立刻重新拉取，并把上一次的选中项一并清掉——否则
    /// 筛选完还停在旧歌手上，用户会以为没生效。
    fn cycle_artist_filter(&mut self) {
        if self.state.tab != Tab::Artists {
            self.state
                .info("按 f 可筛选歌手地区，请先切到「歌手」标签页");
            return;
        }

        const REGIONS: [(i64, &str); 5] = [
            (0, "全部"),
            (1, "华语"),
            (2, "欧美"),
            (3, "日韩"),
            (4, "其他"),
        ];

        let current = REGIONS
            .iter()
            .position(|(kind, _)| *kind == self.state.artists.kind)
            .unwrap_or(0);
        let (kind, label) = REGIONS[(current + 1) % REGIONS.len()];

        self.state.artists.kind = kind;
        self.state.artists.list.replace(Vec::new());
        self.state.artists.songs.replace(String::new(), Vec::new());
        self.state.info(format!("歌手地区筛选：{label}"));
        self.load_artists();
    }

    pub fn load_ranks(&mut self) {
        let api = self.api.clone();
        let active_source = self.state.config.active_source_kind();
        let bus = self.bus.clone();

        self.state.ranks.list.load.begin();
        self.state.busy = Some("载入排行榜".to_string());

        self.runtime.spawn(async move {
            match active_source.rank_boards(&api).await {
                Ok(boards) => bus.emit(Loaded::RankBoards(boards)),
                Err(error) => bus.fail_loading(LoadingTarget::Ranks, "载入排行榜失败", error),
            }
        });
    }

    pub fn load_cloud_playlists(&mut self) {
        if !self.state.logged_in {
            self.state
                .warn(not_logged_in_hint(self.state.config.active_source_kind()));
            return;
        }

        let api = self.api.clone();
        let active_source = self.state.config.active_source_kind();
        let bus = self.bus.clone();

        self.state.cloud.list.load.begin();
        self.state.busy = Some("载入云端歌单".to_string());

        self.runtime.spawn(async move {
            match active_source.user_playlists(&api).await {
                Ok(items) => bus.emit(Loaded::CloudPlaylists(items)),
                Err(error) => {
                    bus.fail_loading(LoadingTarget::CloudPlaylists, "载入云端歌单失败", error)
                }
            }
        });
    }

    pub(super) fn open_selected_playlist(&mut self) {
        let Some(playlist) = self.state.playlists.list.selected().cloned() else {
            self.state.warn("请先选择一个歌单");
            return;
        };
        self.load_playlist_songs(playlist, PlaylistSource::Plaza, false);
    }

    pub(super) fn open_selected_cloud_playlist(&mut self) {
        let Some(playlist) = self.state.cloud.list.selected().cloned() else {
            self.state.warn("请先选择一个歌单");
            return;
        };
        // 选中的歌单同时作为云端同步目标
        if playlist.is_writable() {
            self.state.sync_target = Some(playlist.clone());
        }
        self.load_playlist_songs(playlist, PlaylistSource::Cloud, false);
    }

    /// 载入歌单歌曲。
    ///
    /// `source` 决定结果落到哪个歌曲面板：歌单广场和云端歌单共用一个请求路径，
    /// 但它们是两个独立的界面区域。
    /// \`fresh\` 为真时绕过服务端 2 分钟缓存（见 \`ApiClient::cache_buster\`）。
    /// 按 \`R\` 刷新、以及加歌/删歌之后重新拉取时必须为真，否则拿到的是旧列表。
    fn load_playlist_songs(&mut self, playlist: Playlist, source: PlaylistSource, fresh: bool) {
        let api = self.api.clone();
        let active_source = self.state.config.active_source_kind();
        let bus = self.bus.clone();

        match source {
            PlaylistSource::Plaza => {
                self.state.playlists.open_playlist = Some(playlist.clone());
                let pane = &mut self.state.playlists.songs;
                pane.load.begin();
                // 标题只写歌单名：面板正文已经会显示「载入中…」，标题再挂一次是重复；
                // 而失败时那个后缀会留在标题上撒谎
                pane.title = playlist.name.clone();
            }
            PlaylistSource::Cloud => {
                // 记下打开了哪个歌单：云端内容变动时要靠它判断是否该重载这里，
                // 按 s 收藏时也要以它为目标（而不是上次选的 sync_target）
                self.state.cloud.open_playlist = Some(playlist.clone());
                let pane = &mut self.state.cloud.songs;
                pane.load.begin();
                pane.title = playlist.name.clone();
            }
        }
        self.state.busy = Some(format!("载入歌单《{}》", playlist.name));

        let first_page =
            first_screen_page(self.state.sort_descending, playlist.song_count, PAGE_LIMIT);

        self.runtime.spawn(async move {
            // 自建/收藏歌单走新版接口（按数字 listid），公开歌单走 global_collection_id
            let is_own = playlist.list_id.filter(|_| playlist.is_own);

            // ---- 首屏：先只取一页，让界面立刻有内容 ----
            //
            // 大歌单（几百首）即使并发翻页也要好几秒，这段时间界面只有一个"载入中"，
            // 体验很差。学 MoeKoeMusic 的做法：先给首屏，剩下的后台继续取。
            // 它那边是滚动到底再加载；我们一次性取完，但**先让用户看到东西**。
            //
            // 取哪一页见 [`first_screen_page`]：倒序显示时取的是**最后一页**，
            // 这样首屏出现的就是最终列表的头部，后面整表到位时画面不会整体翻一次。
            //
            // 走 `active_source` 而不是 `api`：分页同样是音源相关的——酷狗那两个
            // 端点（参数是 `page` + `pagesize`）网易云根本没有，直接调 `ApiClient`
            // 会让网易云下打开歌单必然 404；而首屏失败会 return，连下面的后台
            // 补全都走不到，表现就是「歌单里的歌一直 404」。
            let target = match is_own {
                Some(list_id) => PlaylistRef::Own(list_id),
                None => PlaylistRef::Public(&playlist.id),
            };
            let mut fetched_page = first_page;
            let mut first = active_source
                .playlist_tracks_page(&api, target, fetched_page, PAGE_LIMIT, fresh)
                .await;
            // 曲数过期（歌单被删过歌）时算出来的那一页可能是空的。宁可闪一屏
            // 最老的歌，也不能让用户对着一个空列表——退回第 1 页重取。
            if fetched_page > 1 && first.as_ref().is_ok_and(|songs| songs.is_empty()) {
                fetched_page = 1;
                first = active_source
                    .playlist_tracks_page(&api, target, fetched_page, PAGE_LIMIT, fresh)
                    .await;
            }

            match first {
                Ok(songs) => {
                    let has_more = needs_full_fetch(fetched_page, songs.len(), PAGE_LIMIT);
                    bus.emit(Loaded::PlaylistTracks {
                        playlist: playlist.clone(),
                        songs,
                        source,
                    });
                    // 这一页就是全部，没必要再取
                    if !has_more {
                        return;
                    }
                }
                Err(error) => {
                    bus.fail_loading(
                        LoadingTarget::PlaylistSongs(source),
                        format!("载入歌单《{}》失败", playlist.name),
                        error,
                    );
                    return;
                }
            }

            // ---- 后台继续取全部，取到后覆盖为完整列表 ----
            //
            // 界面上会看到「30 首」变成「400 首」，是个不错的进度反馈，
            // 比干等一个"载入中"强。
            let result = match is_own {
                Some(list_id) => {
                    active_source
                        .user_playlist_tracks_all(&api, list_id, fresh)
                        .await
                }
                None => {
                    active_source
                        .playlist_tracks_all(&api, &playlist.id, fresh)
                        .await
                }
            };

            match result {
                Ok(songs) => bus.emit(Loaded::PlaylistTracks {
                    playlist,
                    songs,
                    source,
                }),
                Err(error) => bus.fail_loading(
                    LoadingTarget::PlaylistSongs(source),
                    format!("载入歌单《{}》失败", playlist.name),
                    error,
                ),
            }
        });
    }

    pub(super) fn open_selected_artist(&mut self) {
        let Some(artist) = self.state.artists.list.selected().cloned() else {
            self.state.warn("请先选择一位歌手");
            return;
        };

        let api = self.api.clone();
        let active_source = self.state.config.active_source_kind();
        let bus = self.bus.clone();

        self.state.artists.songs.load.begin();
        self.state.artists.songs.title = artist.name.clone();
        // 记下打开的是谁：结果回来时要靠它判断这条结果还该不该采纳
        self.state.artists.open_artist = Some(artist.clone());
        self.state.busy = Some(format!("载入歌手 {}", artist.name));

        self.runtime.spawn(async move {
            match active_source
                .artist_tracks_all(&api, artist.id, "hot")
                .await
            {
                Ok(songs) => bus.emit(Loaded::ArtistSongs { artist, songs }),
                Err(error) => bus.fail_loading(
                    LoadingTarget::ArtistSongs,
                    format!("载入歌手 {} 的歌曲失败", artist.name),
                    error,
                ),
            }
        });
    }

    pub(super) fn open_selected_rank(&mut self) {
        let Some(board) = self.state.ranks.list.selected().cloned() else {
            self.state.warn("请先选择一个榜单");
            return;
        };

        let api = self.api.clone();
        let active_source = self.state.config.active_source_kind();
        let bus = self.bus.clone();

        self.state.ranks.songs.load.begin();
        self.state.ranks.songs.title = board.name.clone();
        // 记下打开的是哪个榜单，理由同 `open_selected_artist`
        self.state.ranks.open_board = Some(board.clone());
        self.state.busy = Some(format!("载入榜单 {}", board.name));

        self.runtime.spawn(async move {
            match active_source.rank_tracks_all(&api, board.id).await {
                Ok(songs) => bus.emit(Loaded::RankTracks { board, songs }),
                Err(error) => bus.fail_loading(
                    LoadingTarget::RankSongs,
                    format!("载入榜单 {} 失败", board.name),
                    error,
                ),
            }
        });
    }

    fn reload_current_tab(&mut self) {
        match self.state.tab {
            Tab::Search => {
                if self.state.search.submitted.is_empty() {
                    self.state.info("还没有搜索过，按 / 开始搜索");
                } else {
                    self.run_search();
                }
            }
            // 这两个页面是「左侧列表 + 右侧歌曲」两段式：只刷左侧列表的话，右边打开的
            // 歌单内容永远是旧的——用户按 R 看到的还是刚才那些歌，会以为没刷新。
            // 所以列表和当前打开的歌单都要重载。
            Tab::Playlists => {
                self.load_plaza_playlists();
                if let Some(playlist) = self.state.playlists.open_playlist.clone() {
                    self.load_playlist_songs(playlist, PlaylistSource::Plaza, true);
                }
            }
            Tab::Artists => self.load_artists(),
            Tab::Ranks => self.load_ranks(),
            Tab::Cloud => {
                self.load_cloud_playlists();
                if let Some(playlist) = self.state.cloud.open_playlist.clone() {
                    self.load_playlist_songs(playlist, PlaylistSource::Cloud, true);
                }
            }
            Tab::Visualizer => self.state.info("可视化页面没有需要刷新的数据"),
            Tab::Sources => self.state.info("音源状态会在切换与启动时自动探测"),
            Tab::Home | Tab::Queue => self
                .state
                .info("这一页展示的是本地状态，没有需要刷新的列表"),
            Tab::Settings => self.state.info("设置改完即生效并已保存，无需刷新"),
        }
    }

    // ==================================================================
    // 队列、列表顺序与缓存
    // ==================================================================

    fn queue_focused_song(&mut self, play_next: bool) {
        let Some(song) = self.state.selected_song() else {
            self.state.warn("当前没有选中的歌曲");
            return;
        };

        let label = describe_song(&song);
        if play_next {
            self.state.queue.insert_next(song);
            self.state.success(format!("已插入到下一首：{label}"));
        } else {
            self.state.queue.append(song);
            self.state.success(format!("已加入队列：{label}"));
        }
        self.clamp_queue_cursor();
    }

    /// `x`：把播放队列里选中的歌曲移出队列。
    fn remove_selected_from_queue(&mut self) {
        if self.state.focus != Focus::Queue {
            self.state.warn("请先按 Tab 把焦点切到播放队列");
            return;
        }
        let Some(index) = self.state.queue_cursor.selected() else {
            self.state.warn("播放队列为空");
            return;
        };
        let Some(removed) = self.state.queue.remove(index) else {
            return;
        };

        self.clamp_queue_cursor();
        // 被移除的正好是当前曲目时停止播放，避免出现「在放一首已不在队列里的歌」
        let removed_current = self
            .state
            .current
            .as_ref()
            .map(|song| song.hash == removed.hash)
            .unwrap_or(false);
        if removed_current {
            self.audio.stop();
            self.state.current = None;
            self.state.playback = PlaybackState::Stopped;
            self.state.position_ms = 0;
            self.state.duration_ms = 0;
        }

        self.state
            .info(format!("已从队列移除：{}", describe_song(&removed)));
    }

    /// `A`：把当前列表**全部**加入播放队列。
    ///
    /// 一次 `append_all`，避免逐首调用（数百首时逐首添加既慢又容易在扩容时卡顿。
    fn queue_all_songs(&mut self) {
        let descending = self.state.sort_descending;
        let Some(songs) = self.state.focused_songs().map(|list| list.songs.clone()) else {
            self.state.warn("当前没有可加入队列的歌曲列表");
            return;
        };

        if songs.is_empty() {
            self.state.warn("列表为空，先按 Enter 载入歌曲");
            return;
        }

        let count = songs.len();
        let _ = self.state.queue.append_all(songs);
        self.clamp_queue_cursor();

        let order = if descending { "倒序" } else { "正序" };
        self.state
            .success(format!("已把 {count} 首歌加入队列（{order}）"));
    }

    /// `o`：切换歌曲列表的排列方向，并同步反转所有已载入的列表。
    ///
    /// 反转的是存储，所以界面顺序与播放队列顺序始终一致——这也是它比「渲染时反转」更
    /// 可靠的理由。
    fn toggle_sort_order(&mut self) {
        self.state.sort_descending = !self.state.sort_descending;
        let descending = self.state.sort_descending;

        self.state.search.results.toggle_sort();
        self.state.playlists.songs.toggle_sort();
        self.state.artists.songs.toggle_sort();
        self.state.ranks.songs.toggle_sort();
        self.state.cloud.songs.toggle_sort();

        // 排列方向变了，排序提示也跟着更新，免得用户按 o 之后界面没有任何反馈。
        let order = if descending {
            "倒序（最后一首在最上）"
        } else {
            "正序"
        };
        self.state.info(format!("列表排列：{order}"));
    }

    /// 清空音频缓存目录。
    ///
    /// 由确认弹窗触发——删文件不可恢复，不能让一次误按就清掉全部缓存。
    /// 清空后立刻重新统计占用，界面上的「已用」才会跟着变。
    fn clear_cache(&mut self) {
        match self.cache.clear() {
            Ok(report) => {
                self.refresh_cache_usage();

                let freed = crate::ui::widgets::human_bytes(report.freed_bytes);
                if report.failed > 0 {
                    self.state.warn(format!(
                        "已清理 {} 个文件（{}），{} 个删除失败（检查目录权限）",
                        report.removed_files, freed, report.failed
                    ));
                } else if report.removed_files == 0 {
                    self.state.info("缓存已经是空的");
                } else {
                    self.state.success(format!(
                        "已清理缓存：{} 个文件，释放 {}",
                        report.removed_files, freed
                    ));
                }
            }
            Err(error) => self.state.error(format!("清空缓存失败：{error}")),
        }
    }

    /// 执行「清空播放队列」。**停止当前播放**——队列都没了，继续播一首不在队列里的
    /// 歌既没意义（下一首无从查找）。若只想删掉其中一首，用 `x`，它不会打断当前播放。
    fn clear_queue(&mut self) {
        if self.state.queue.is_empty() {
            self.state.warn("播放队列已经为空");
            return;
        }

        let count = self.state.queue.len();
        self.stop_playback();
        self.state.queue.clear();
        self.state.queue_cursor.select(None);
        self.state.current = None;
        self.state.playback = PlaybackState::Stopped;
        self.state.position_ms = 0;
        self.state.duration_ms = 0;
        self.state
            .info(format!("已清空播放队列（{count} 首），已停止播放"));
    }

    pub(super) fn sync_queue_cursor(&mut self, index: usize) {
        if self.state.queue.is_empty() {
            self.state.queue_cursor.select(None);
        } else {
            self.state
                .queue_cursor
                .select(Some(index.min(self.state.queue.len() - 1)));
        }
    }

    fn clamp_queue_cursor(&mut self) {
        let len = self.state.queue.len();
        if len == 0 {
            self.state.queue_cursor.select(None);
            return;
        }
        let current = self.state.queue_cursor.selected().unwrap_or(0).min(len - 1);
        self.state.queue_cursor.select(Some(current));
    }

    // ==================================================================
    // 异步结果
    // ==================================================================

    fn handle_loaded(&mut self, loaded: Loaded) {
        match loaded {
            Loaded::Search {
                keyword,
                songs,
                append,
            } => {
                // 迟到的结果必须丢掉。用户可能已经搜了别的词：不判这一下的话，
                // 上一个词的结果会把新结果**整体覆盖**；追加时更糟——把上一个词的歌
                // 混进新列表，标题却还写着新词，用户没有任何线索能看出来。
                //
                // 这一步也要在清 `busy` **之前**：否则一条过期结果会把正在进行中的
                // 新搜索的「正在搜索…」抹掉，看起来像是已经搜完了。
                if !accepts_search_result(&self.state.search.submitted, &keyword) {
                    return;
                }
                // 认领之后收掉「加载更多」的标志：无论这一页回来了还是越界（空页），
                // 都不能让它一直挂着，把后续的 M 全挡掉。
                self.state.search.loading_more = false;
                self.state.busy = None;

                if append {
                    // 追加：保持服务端给的相关性顺序，**不重排**。
                    // 重排会把新一页的相关结果搅进旧结果里，破坏"越靠前越相关"。
                    let pane = &mut self.state.search.results;
                    let mut all = std::mem::take(&mut pane.songs);
                    let added = songs.len();
                    all.extend(songs);
                    let total = all.len();
                    pane.set_songs_sorted(format!("搜索「{keyword}」· {total} 首"), all, false);
                    if added == 0 {
                        self.state.info("没有更多结果了");
                    } else {
                        self.state
                            .success(format!("又加载了 {added} 首（共 {total} 首）"));
                    }
                } else {
                    let count = songs.len();
                    // 标题带上条数：只写「搜索「XX」」会让人以结果就列表里这几条，
                    // 实际接口 total 常有好几百，按 M 可以继续加载。
                    self.state.search.page = 1;
                    //
                    // 这里**刻意不**跟随 `sort_descending`。
                    // 那个开关是给歌单用的（新歌在最上），但搜索结果的价值全在
                    // 服务端给的相关性排序上：默认倒序会把最相关的翻到最后，
                    // 实测搜「黑色幽默」原本周杰伦排第一，倒序后前排变成
                    // 「潜水不会游」「科野」这类，等于搜不到想要的东西。
                    self.state.search.results.set_songs_sorted(
                        format!("搜索「{keyword}」· {count} 首"),
                        songs,
                        false,
                    );
                    if count == 0 {
                        self.state.warn(format!("「{keyword}」没有找到结果"));
                    } else {
                        self.state.success(format!("「{keyword}」找到 {count} 首"));
                        // 结果到手后把焦点交给列表，方便直接按 Enter 播放
                        self.state.focus = Focus::Secondary;
                    }
                }
            }

            Loaded::Playlists {
                category,
                title,
                items,
            } => {
                // 用户可能已经切了分类：这条迟到的结果直接替换列表的话，左侧选中的
                // 是「华语」，右侧却是「全部」的内容。
                //
                // 分类是常驻状态（不是 `Option`），所以这里直接比，不走
                // `accepts_open_item`——那个函数的语义是「当前打开的那个」。
                if category != self.state.playlists.category {
                    return;
                }
                self.state.busy = None;
                let count = items.len();
                self.state.playlists.list.replace(items);
                if count == 0 {
                    self.state.warn(format!("{title}暂无内容"));
                } else {
                    self.state.info(format!("{title}：{count} 个歌单"));
                }
            }

            Loaded::PlaylistTracks {
                playlist,
                songs,
                source,
            } => {
                // 用户可能已经打开了另一个歌单：这条迟到的结果直接覆盖右侧列表的话，
                // 会变成「标题写着 B、内容是 A」。只认当前打开的那个歌单的结果。
                //
                // 首屏与后台补齐是两次 `PlaylistTracks`，同一个歌单，都放行。
                let open = match source {
                    PlaylistSource::Plaza => self
                        .state
                        .playlists
                        .open_playlist
                        .as_ref()
                        .map(|open| open.id.as_str()),
                    PlaylistSource::Cloud => self
                        .state
                        .cloud
                        .open_playlist
                        .as_ref()
                        .map(|open| open.id.as_str()),
                };
                if !accepts_open_item(open, playlist.id.as_str()) {
                    return;
                }
                self.state.busy = None;
                let descending = self.state.sort_descending;
                let count = songs.len();
                let title = format!("{} · {count} 首", playlist.name);

                match source {
                    PlaylistSource::Plaza => self
                        .state
                        .playlists
                        .songs
                        .set_songs_sorted(title, songs, descending),
                    PlaylistSource::Cloud => self
                        .state
                        .cloud
                        .songs
                        .set_songs_sorted(title, songs, descending),
                }

                self.state.focus = Focus::Secondary;
                self.state
                    .info(format!("《{}》共 {count} 首", playlist.name));
            }

            Loaded::Artists { kind, artists } => {
                // 同歌单广场：切了地区筛选之后，旧筛选的结果不能顶上来
                if kind != self.state.artists.kind {
                    return;
                }
                self.state.busy = None;
                let count = artists.len();
                self.state.artists.list.replace(artists);
                if count == 0 {
                    self.state.warn("歌手列表为空");
                } else {
                    self.state.info(format!("共 {count} 位歌手"));
                }
            }

            Loaded::ArtistSongs { artist, songs } => {
                // 同歌单：用户可能已经点了下一位歌手，只认当前打开的那位
                let open = self.state.artists.open_artist.as_ref().map(|open| open.id);
                if !accepts_open_item(open, artist.id) {
                    return;
                }
                self.state.busy = None;
                let descending = self.state.sort_descending;
                let count = songs.len();
                self.state.artists.songs.set_songs_sorted(
                    format!("{} · {count} 首", artist.name),
                    songs,
                    descending,
                );
                self.state.focus = Focus::Secondary;
            }

            Loaded::RankBoards(boards) => {
                self.state.busy = None;
                let count = boards.len();
                self.state.ranks.list.replace(boards);
                if count == 0 {
                    self.state.warn("排行榜列表为空");
                } else {
                    self.state.info(format!("共 {count} 个榜单"));
                }
            }

            Loaded::RankTracks { board, songs } => {
                // 同上：只认当前打开的那个榜单
                let open = self.state.ranks.open_board.as_ref().map(|open| open.id);
                if !accepts_open_item(open, board.id) {
                    return;
                }
                self.state.busy = None;
                let descending = self.state.sort_descending;
                let count = songs.len();
                self.state.ranks.songs.set_songs_sorted(
                    format!("{} · {count} 首", board.name),
                    songs,
                    descending,
                );
                self.state.focus = Focus::Secondary;
            }

            Loaded::CloudPlaylists(items) => {
                self.state.busy = None;
                // 刻意不写状态栏：这个方法也会在「同步完成」之后被调用，而那句
                // 「已同步 N 首」比「云端歌单：N 个」重要得多，不该被它覆盖掉。
                // 列表本身的变化已经是足够的反馈。
                self.state.cloud.list.replace(items);
            }

            Loaded::Lyric { hash, lyric } => {
                // 用户可能已经切歌：`lyric.hash` 在 start_playback 时被设为当前曲目，
                // 对不上说明这是上一首的迟到结果，直接丢弃
                if self.state.lyric.hash.as_deref() != Some(hash.as_str()) {
                    return;
                }
                let empty = lyric.is_empty();
                self.state.lyric.lyric = lyric;
                self.state.lyric.load.succeed();
                // 换歌了：过渡状态一并复位，否则新歌的第一句会从上一首的某一行
                // 淡过来，看着像歌词串了。
                self.state.lyric.reset_transition();
                if empty {
                    tlog!(crate::logger::LEVEL_DEBUG, "歌曲 {hash} 没有可用歌词");
                }
            }

            Loaded::LyricFailed { hash, reason } => {
                // 和上面同理：只认当前这首的失败，迟到的结果直接丢弃
                if self.state.lyric.hash.as_deref() != Some(hash.as_str()) {
                    return;
                }
                self.state.lyric.lyric = crate::api::model::Lyric::default();
                self.state.lyric.load.fail(reason);
                self.state.lyric.reset_transition();
            }

            Loaded::StreamReady {
                song,
                url,
                start_at_ms,
                is_trial,
                reason,
            } => {
                if !self.is_current(&song) {
                    return;
                }
                // 记下来：片段播完时要提示，而不是当成正常结束直接切下一首
                self.state.current_is_trial = is_trial;
                if is_trial {
                    // 带上服务端给的原因，否则用户只能猜「是不是会员没生效」
                    let why = reason
                        .map(|why| format!("：{why}"))
                        .unwrap_or_else(|| "（完整版需要对应会员）".to_string());
                    self.state
                        .warn(format!("《{}》只有试听片段{why}", song.name));
                } else if let Some(why) = reason {
                    // 完整版拿到了，但**档位可能不是用户设的那一档**：`/song/url`
                    // 会静默降级（请求 flac 回 128 kbps mp3，`status` 仍是 1），
                    // 蝰蛇音质没权限时也会降到标准档。以前这条 `reason` 只在试听
                    // 分支被读，于是两种降级都无声无息地过去了——界面标着 flac，
                    // 耳朵听的是 128，而且永远查不出来。
                    self.state.info(format!("《{}》{why}", song.name));
                }
                // 片段落到独立的缓存键，避免把完整版的位置占住
                self.start_download(*song, url, start_at_ms, is_trial);
            }

            Loaded::StreamPrerolled {
                song,
                buffer,
                start_at_ms,
            } => {
                if !self.is_current(&song) {
                    // 用户已经切走了：这次下载没人会听，通知它收工，别再占着
                    // 带宽和磁盘；缓冲里的窗口和文件句柄也随之释放。
                    buffer.cancel();
                    return;
                }
                // 攒够开头就开播——不用等整首下完（边下边播）
                self.state.download_progress = None;
                self.state.busy = None;
                // 留一份句柄：切歌/停止时要靠它通知后台任务别再下了
                self.active_stream = Some(buffer.clone());
                self.audio
                    .load(AudioSource::Stream(buffer), start_at_ms, song.duration_ms);
            }

            Loaded::DownloadProgress { received, total } => {
                self.state.download_progress = Some((received, total));
            }

            Loaded::StreamCached {
                song,
                path,
                start_at_ms,
            } => {
                if !self.is_current(&song) {
                    // 用户已切歌，删掉刚下载的孤儿文件，避免缓存被无用数据占满
                    if let Err(error) = std::fs::remove_file(&path) {
                        tlog!(
                            crate::logger::LEVEL_WARN,
                            "清理过期缓存 {} 失败：{error}",
                            path.display()
                        );
                    }
                    return;
                }

                self.state.download_progress = None;
                self.state.busy = None;
                self.audio
                    .load(AudioSource::File(path), start_at_ms, song.duration_ms);

                self.after_download_landed();
            }

            Loaded::StreamCompleted { song, path } => {
                // 不是当前这首就别管：收尾是给"正在听的那首"做的
                if !self.is_current(&song) {
                    return;
                }
                // **这里绝不能 `audio.load()`**。歌已经在放了，缓冲里（和刚落盘
                // 的文件里）数据都是全的，重新装载只会把播放位置冲回 0。
                // 前端那句「放着放着从头开始」就是这条路径造成的。
                tlog!(
                    crate::logger::LEVEL_DEBUG,
                    "《{}》边下边播已下完：{}",
                    song.name,
                    path.display()
                );
                self.state.download_progress = None;
                self.state.busy = None;
                self.active_stream = None;
                self.after_download_landed();
            }

            Loaded::DeviceFingerprint(dfid) => {
                self.state.config.dfid = Some(dfid.clone());
                let cookie = self.state.config.cookie_header();
                self.api.set_cookie(cookie);
                tlog!(crate::logger::LEVEL_INFO, "已获取设备指纹 dfid={dfid}");
                // 立刻落盘，下次启动就不用再请求
                if let Err(error) = self.state.config.save() {
                    tlog!(crate::logger::LEVEL_WARN, "保存 dfid 到配置失败：{error}");
                }
            }

            Loaded::CacheUsage(bytes) => {
                self.state.cache_bytes = bytes;
            }

            Loaded::CloudNotice(message) => {
                self.state.busy = None;
                self.state.success(message);
                // 刷新云端歌单，让新加入的歌曲数量反映出来
                self.load_cloud_playlists();
            }

            Loaded::CloudPlaylistChanged { playlist } => {
                // 歌单列表**再**刷一次。
                //
                // `CloudNotice` 里已经刷过一次，但那是「立刻」刷的——服务端歌单同步
                // 有延迟（实测约 5 秒），那一次读到的还是旧歌曲数。这里是在延迟之后
                // 才发出的，再刷一次把旧值覆盖掉。少了这步的表现就是：列表里的
                // 「我喜欢 413」纹丝不动，而服务端其实已经变成 414 了。
                self.load_cloud_playlists();

                // 补上**歌曲列表**——收藏/移除成功后当前歌单还显示旧内容，
                // 用户会以为没生效。
                //
                // 只在「云端页打开的就是这个歌单」时重载：否则用户正在看另一个歌单，
                // 重载会把它的内容盖掉（数据没错，但界面莫名其妙跳到别的歌单了）。
                let list_id = playlist.list_id;
                let opened = self
                    .state
                    .cloud
                    .open_playlist
                    .as_ref()
                    .and_then(|open| open.list_id);
                if list_id.is_some() && opened == list_id {
                    self.load_playlist_songs(*playlist, PlaylistSource::Cloud, true);
                }
            }

            Loaded::LoginQr { key, content } => {
                let qr = crate::ui::widgets::qr_lines(&content, self.state.config.qr_aspect)
                    .unwrap_or_default();
                let login = self.state.login.get_or_insert_with(LoginState::default);
                login.qr = qr;
                login.key = key;
                login.message = format!(
                    "用 {} App 扫码登录",
                    self.state.config.active_source_kind().scan_app()
                );
                login.finished = false;
            }

            Loaded::LoginStatus { message } => {
                if let Some(login) = self.state.login.as_mut()
                    && !login.finished
                {
                    login.message = message;
                }
            }

            Loaded::LoginSucceeded {
                token,
                userid,
                cookie,
            } => {
                match (token, userid) {
                    (Some(token), Some(userid)) => self.apply_login(token, userid),
                    // 网易云：登录凭证就是服务端给的那个 cookie，必须存下来，
                    // 否则「登录成功」只是个谎言——之后的请求没有任何身份。
                    _ => match cookie {
                        Some(cookie) if !cookie.trim().is_empty() => {
                            self.apply_server_cookie(cookie)
                        }
                        _ => self.finish_server_side_login(),
                    },
                }
            }

            Loaded::LoginFailed { message } => {
                self.finish_login(false, message);
            }

            Loaded::CoverReady { hash, image } => {
                // 结果回来时用户可能已经切歌，只认当前这首
                if self
                    .state
                    .current
                    .as_ref()
                    .is_some_and(|song| song.hash == hash)
                {
                    // 只存解码后的原图，**不在这里建图片协议**：协议要按目标区域的
                    // 像素尺寸编码，而区域只有渲染时才知道（还会随窗口大小变）。
                    // 交给 `CoverArt::fit_to` 按需建、按需重编。
                    let aspect = image_aspect(&image);
                    self.state.cover.set_image(hash, image, aspect);
                }
            }

            Loaded::UserInfo(info) => {
                // 头像地址在资料里，拿到就去取图
                if let Some(url) = info.pic.clone()
                    && !self.state.config.lite_mode
                {
                    self.load_avatar(url);
                }
                self.state.user_info = Some(*info);
                self.state.user_info_load.succeed();
            }

            Loaded::AvatarReady { image } => {
                // 协议必须在主线程建：`Picker` 探测过终端能力，不是 `Send`
                self.state.avatar.protocol = self
                    .state
                    .picker
                    .as_ref()
                    .map(|picker| picker.new_resize_protocol(image));
            }

            Loaded::VipStatus(info) => {
                self.state.vip_info = Some(*info);
            }

            Loaded::VipClaimed {
                day,
                outcome,
                manual,
            } => {
                self.state.vip_claiming = false;
                match outcome {
                    VipClaimOutcome::Claimed => {
                        // 只有真的领到才记进会话。失败不该占掉「今天已经领过」的
                        // 名额，否则用户想重试都没有机会。
                        self.state.vip_claimed_day = Some(day);
                        self.state.success("已领取今日概念版 VIP");
                        // 会员摘要跟着变了，重新取一次，让「我的资料」显示到账后的状态
                        self.fetch_vip_status();
                    }
                    VipClaimOutcome::AlreadyClaimed => {
                        // 服务端说今天领过了（可能是手机或别的机器上领的），
                        // 记下来，省掉今天剩下的所有重复查询。
                        self.state.vip_claimed_day = Some(day);
                        if manual {
                            self.state.info("今日 VIP 已经领过了，无需重复领取");
                        }
                        self.fetch_vip_status();
                    }
                    VipClaimOutcome::Failed(message) => {
                        // 上游对失败原因常常只给一个码。这里不再补「请到手机端领取」
                        // 那种猜测——现在能区分「已经领过」和「真失败」了，
                        // 剩下的失败补一句猜测只会误导。
                        self.state.warn(format!("领取 VIP 失败：{message}"));
                    }
                }
            }

            Loaded::Failed {
                context,
                error,
                target,
            } => {
                self.state.busy = None;
                self.state.download_progress = None;
                tlog!(crate::logger::LEVEL_ERROR, "{context}：{error}");
                // 凭据被服务端作废后，界面若还挂着「登录 是」就是谎言——用户会
                // 以为登录态是好的、转而去怀疑别处。标记失效后，按 L 直接进扫码
                // （不再弹「已登录，是否覆盖」的确认）。
                if error.is_login_expired() {
                    self.state.logged_in = false;
                }
                let hint = error.user_hint();
                // 失败必须落到**面板**上，不能只写状态栏：状态栏那一行会被后续
                // 消息覆盖，而面板要是留在「载入中…」就成了永久转圈。
                if let Some(target) = target {
                    self.mark_load_failed(target, &hint);
                }
                self.state.error(format!("{context}：{hint}"));
            }
        }
    }

    /// 按真实请求的结果更新「连通性」。
    ///
    /// 只在状态**变化**时写状态栏——每来一个成功响应都刷一句「已连接」会把有用的
    /// 消息冲掉。失败的详细原因由 `handle_loaded` 写，这里不重复。
    ///
    /// # 为什么失败只认带 `target` 的那些
    ///
    /// `target` 非空表示这是一次**载入类**请求，而载入类请求全部打向
    /// KuGouMusicApi 本身。反过来，下载 / 取直链失败打的是 CDN（`imge.kugou.com`
    /// 那一类），把它的传输层失败算成「API 未连通」会把用户引到完全错误的排查方向。
    /// 宁可漏记（下一次载入请求会补上），也不要记错。
    fn note_connection(&mut self, loaded: &Loaded) {
        let next = match loaded {
            Loaded::Failed {
                error,
                target: Some(_),
                ..
            } => {
                if error.is_connectivity() {
                    Connection::Unreachable
                } else {
                    // 业务错误码（需要登录、页码越界…）恰恰说明服务是通的
                    Connection::Connected
                }
            }
            _ if loaded.is_api_response() => Connection::Connected,
            // 本地产生的事件（缓存占用、下载进度、封面解码…）不参与判断：
            // 它们本机就能产生，算进去的话接口挂着也会被标成「已连通」
            _ => return,
        };
        if self.state.connection == next {
            return;
        }
        self.state.connection = next;
        if next == Connection::Connected {
            self.state.info(format!("已连接 {}", self.api.base()));
        }
    }

    /// 把一次载入失败落到对应的面板上：收掉「载入中」，留下原因。
    fn mark_load_failed(&mut self, target: LoadingTarget, reason: &str) {
        match target {
            LoadingTarget::SearchResults => {
                // 失败的可能是「加载更多」那一页，标志得收掉——否则 M 会被永久挡住
                self.state.search.loading_more = false;
                self.state.search.results.load.fail(reason)
            }
            LoadingTarget::Playlists => self.state.playlists.list.load.fail(reason),
            LoadingTarget::Artists => self.state.artists.list.load.fail(reason),
            LoadingTarget::Ranks => self.state.ranks.list.load.fail(reason),
            LoadingTarget::CloudPlaylists => self.state.cloud.list.load.fail(reason),
            LoadingTarget::PlaylistSongs(source) => match source {
                PlaylistSource::Plaza => self.state.playlists.songs.load.fail(reason),
                PlaylistSource::Cloud => self.state.cloud.songs.load.fail(reason),
            },
            LoadingTarget::ArtistSongs => self.state.artists.songs.load.fail(reason),
            LoadingTarget::RankSongs => self.state.ranks.songs.load.fail(reason),
            LoadingTarget::UserInfo => self.state.user_info_load.fail(reason),
        }
    }

    fn start_download(&mut self, song: Song, url: String, start_at_ms: u64, is_trial: bool) {
        let key = if is_trial {
            song.trial_cache_key(&self.state.config.quality)
        } else {
            song.cache_key(&self.state.config.quality)
        };
        let extension = Downloader::extension_from_url(&url);
        let target = self.cache.path_for(&key, extension);

        let downloader = self.downloader.clone();
        let bus = self.bus.clone();
        let label = describe_song(&song);

        self.state.download_progress = Some((0, None));
        self.state.busy = Some(format!("缓冲《{label}》"));

        self.runtime.spawn(async move {
            // 节流用原子量而不是 Cell：闭包需要 Send + Sync
            let last_reported = AtomicU64::new(0);
            let progress = |received: u64, total: Option<u64>| {
                let finished = total.is_some_and(|total| received >= total);
                if !finished
                    && received.saturating_sub(last_reported.load(Ordering::Relaxed))
                        < PROGRESS_STEP_BYTES
                {
                    return;
                }
                last_reported.store(received, Ordering::Relaxed);
                bus.emit(Loaded::DownloadProgress { received, total });
            };

            // 要跳到中间（续播上次的位置）时**不能**走流式：缓冲里只有开头那点
            // 数据，seek 到几百秒的位置会阻塞等下载、超时失败，结果从头播
            // ——用户实测「边听边下载会直接从最开始听」就是这个。
            //
            // 汽水（`file://`）也走这条：它整首已经下完并解密了，流式缓冲
            // 那套「边下边播」对它没有意义——文件就在本地，直接复制进缓存。
            if start_at_ms > 0 || crate::audio::download::is_local_url(&url) {
                match downloader.fetch_to(&url, &target, &progress).await {
                    Ok(_) => bus.emit(Loaded::StreamCached {
                        song: Box::new(song),
                        path: target,
                        start_at_ms,
                    }),
                    Err(error) => bus.fail(format!("下载《{label}》失败"), error),
                }
                return;
            }

            // 边下边播：先起流式下载（立即返回缓冲），攒够开头就开播，
            // 剩下的在后台继续下并落盘到缓存。
            //
            // 之前是 `fetch_to` 下完整个文件才 `load`——一首 Hi-Res 几十 MB，
            // 等待时间全押在下载上。现在只等开头那 128 KB。
            let bus_for_done = bus.clone();
            let song_for_done = song.clone();
            let target_for_done = target.clone();
            let label_for_done = label.clone();

            let buffer =
                match downloader.start_streaming(&url, target, move |outcome| match outcome {
                    // 下完了只做收尾。**不能**在这里 `audio.load` 换成本地文件：
                    // 这首已经在放了，再装载一次会把位置冲回 0——用户听到的就是
                    // 「放着放着从头开始」（`StreamCompleted` 的注释里写了缘由）。
                    StreamOutcome::Completed => bus_for_done.emit(Loaded::StreamCompleted {
                        song: Box::new(song_for_done),
                        path: target_for_done,
                    }),
                    StreamOutcome::Failed(message) => bus_for_done.fail(
                        format!("下载《{label_for_done}》失败"),
                        AppError::Audio(message),
                    ),
                    // 用户已经切歌/停止，安静收尾：不报错、不提示
                    StreamOutcome::Cancelled => {}
                }) {
                    Ok(buffer) => buffer,
                    Err(error) => {
                        bus.fail(format!("下载《{label}》失败"), error);
                        return;
                    }
                };

            // 等攒够开头再开播。轮询而不是阻塞等——这是 async 任务，
            // 阻塞会把 runtime 的线程占住。
            let preroll = buffer.clone();
            loop {
                let got = preroll.buffered_bytes();
                // 流式拿不到总长度，只能报已收到的字节——进度条照常工作
                progress(got, None);
                // 用 `is_finished` 而不是 `is_complete`：下载**失败**也是收工，
                // 失败时再等下去只会白转（而这正是"下不到就不开播"该有的样子）
                if got >= PREROLL_BYTES || preroll.is_finished() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }

            // 一点都没下到（比如直链立刻失败）就别发了，等下载完成那条报错
            if preroll.buffered_bytes() > 0 {
                bus.emit(Loaded::StreamPrerolled {
                    song: Box::new(song),
                    buffer,
                    start_at_ms,
                });
            }
        });
    }

    /// 一首歌下载落盘之后的收尾：预取下一首 + 回收缓存。
    ///
    /// 两条下载路径共用它（续播的 `StreamCached`、边下边播的 `StreamCompleted`）
    /// ——"文件已经在盘上"这件事对两者是一样的，区别只在要不要动播放器。
    fn after_download_landed(&mut self) {
        // 当前这首已经在放了——趁这会儿把**下一首**悄悄下下来。
        //
        // 高音质（Hi-Res 那档实测 65 MB）首次播放要等完整下载，几十秒起步；
        // 切歌时再下就是「每首都等一遍」。预取之后切到下一首直接命中缓存，
        // 体验上的差别是「秒开」和「转圈半分钟」。
        self.prefetch_next();

        // 缓存回收是同步目录扫描，扔到阻塞线程池，别卡住 UI
        let cache = self.cache.clone();
        self.runtime
            .spawn_blocking(move || match cache.enforce_limit() {
                Ok(report) if report.removed_files > 0 => tlog!(
                    crate::logger::LEVEL_INFO,
                    "缓存回收：删除 {} 个文件，释放 {} 字节",
                    report.removed_files,
                    report.freed_bytes
                ),
                Ok(_) => {}
                Err(error) => {
                    tlog!(crate::logger::LEVEL_WARN, "缓存回收失败：{error}")
                }
            });
    }

    /// 事件里的歌曲是否仍是当前播放的那首。
    fn is_current(&self, song: &Song) -> bool {
        self.state
            .current
            .as_ref()
            .map(|current| current.hash == song.hash)
            .unwrap_or(false)
    }

    // ==================================================================
    // 封面与歌词：只负责发起请求，结果在 handle_loaded 里处理
    // ==================================================================

    pub(super) fn request_lyric(&mut self, song: Song) {
        // 歌词同样按歌曲自己的来源取：跨音源时 hash 只在该平台的接口里有意义
        let source = song.source;
        let bus = self.bus.clone();
        let api = match self.client_for(source) {
            Ok(client) => client,
            Err(error) => {
                crate::logger::tlog!(
                    crate::logger::LEVEL_WARN,
                    "无法连接「{}」取歌词：{error}",
                    source.label()
                );
                // 面板也得知道——不然它会停在上一次的歌词上，或者显示
                // 「暂无歌词」，两种都不对
                bus.emit(Loaded::LyricFailed {
                    hash: song.hash.clone(),
                    reason: error.user_hint(),
                });
                return;
            }
        };

        self.state.lyric.load.begin();
        self.runtime.spawn(async move {
            match source.fetch_lyric(&api, &song).await {
                Ok(lyric) => bus.emit(Loaded::Lyric {
                    hash: song.hash.clone(),
                    lyric,
                }),
                Err(error) => {
                    tlog!(
                        crate::logger::LEVEL_WARN,
                        "获取《{}》的歌词失败：{error}",
                        song.name
                    );
                    // 走 LyricFailed 而不是塞一份空歌词：空歌词会被面板渲染成
                    // 「暂无歌词」，等于替这首歌断言「它本来就没有歌词」。
                    // 也不走 Loaded::Failed——那会往状态栏写错误，而歌词失败
                    // 不影响播放，每切一首歌闪一条太吵。
                    bus.emit(Loaded::LyricFailed {
                        hash: song.hash.clone(),
                        reason: error.user_hint(),
                    });
                }
            }
        });
    }

    /// 为当前歌曲取封面（异步）：下载 → 解码 → 生成字符画。
    ///
    /// 失败只记日志：封面是锦上添花，不能因为它让播放流程报错。
    pub(super) fn load_cover(&mut self, song: &Song) {
        // 简易模式：封面是最占内存的一块（图片解码 + 图形协议），直接不取。
        // 与其取完再扔，不如从源头省掉这次下载与解码。
        if self.state.config.lite_mode {
            if !self
                .state
                .cover
                .hash
                .as_deref()
                .unwrap_or_default()
                .is_empty()
            {
                self.state.cover = CoverArt::default();
            }
            return;
        }
        // 已经有这张封面就不用重复取
        if self.state.cover.belongs_to(&song.hash) && self.state.cover.is_drawable() {
            return;
        }
        if song.hash.is_empty() {
            self.state.cover = CoverArt::default();
            return;
        }

        let song = song.clone();
        let hash = song.hash.clone();
        // 封面同样按歌曲自己的来源取：网易云要额外查 /song/detail，
        // 拿当前音源的客户端去问是问不到的
        let active_source = song.source;
        let api = match self.client_for(active_source) {
            Ok(client) => client,
            Err(error) => {
                crate::logger::tlog!(
                    crate::logger::LEVEL_WARN,
                    "无法连接「{}」取封面：{error}",
                    active_source.label()
                );
                return;
            }
        };
        let downloader = self.downloader.clone();
        let bus = self.bus.clone();

        self.runtime.spawn(async move {
            // 封面地址由音源自己解析：多数音源在搜索结果里直接带 URL，
            // 网易云只有 picId，要再查一次 /song/detail。
            let url = match active_source.cover_url(&api, &song).await {
                // 展开 {size}：酷狗的封面模板 URL 不替换就是个 404，
                // 封面会永远加载不出来（这正是之前一直显示占位的原因）
                Ok(Some(url)) => expand_cover_size(&url, COVER_PIXEL_SIZE),
                Ok(None) => return,
                Err(error) => {
                    crate::logger::tlog!(crate::logger::LEVEL_WARN, "取封面地址失败：{error}");
                    return;
                }
            };
            let bytes = match downloader.fetch_bytes(&url).await {
                Ok(bytes) => bytes,
                Err(error) => {
                    crate::logger::tlog!(crate::logger::LEVEL_WARN, "下载封面失败 {url}：{error}");
                    return;
                }
            };
            let image = match image::load_from_memory(&bytes) {
                Ok(image) => image,
                Err(error) => {
                    crate::logger::tlog!(crate::logger::LEVEL_WARN, "解码封面失败 {url}：{error}");
                    return;
                }
            };
            // 只传解码结果：图片协议按目标区域编码，所以这里不预先转字符画——
            // 那是条永远走不到的兜底路径（`Picker::halfblocks()` 不会失败）
            bus.emit(Loaded::CoverReady { hash, image });
        });
    }

    // ==================================================================
    // 音频事件与心跳
    // ==================================================================

    fn handle_audio_event(&mut self, event: AudioEvent) {
        match event {
            AudioEvent::Ready { duration_ms } => {
                if duration_ms > 0 {
                    self.state.duration_ms = duration_ms;
                }
                self.state.busy = None;
                self.state.download_progress = None;
                self.state.playback = PlaybackState::Playing;
                // 曲目真的起来了，之前记的"续播点"已经兑现（或者早就过期了），
                // 留着只会让下次按 Space 莫名回到某个中间位置
                self.state.resume = None;
                let title = self
                    .state
                    .current
                    .as_ref()
                    .map(describe_song)
                    .unwrap_or_default();
                self.state.success(format!("正在播放：{title}"));
            }
            AudioEvent::DeviceOpened { name } => {
                // 第一次是启动时上报，只记下来不打扰；之后的切换才提示
                let is_startup = self.state.output_device.is_empty();
                self.state.output_device = name.clone();

                if let Some((song, position)) = self.pending_device_resume.take() {
                    self.start_playback(song, position);
                    return;
                }
                if !is_startup {
                    self.state.info(format!("输出设备 → {name}"));
                }
            }
            AudioEvent::TrackFinished => {
                self.state.position_ms = 0;
                // **先 cancel 再丢**。直接置 `None` 只放下了我们手里的那一份 Arc：
                // 后台下载任务自己还持有一份，会继续把整首往内存窗口和 `.part`
                // 文件里灌，谁也回收不了。切歌与主动停止两条路都调了 `cancel()`
                // （`playback.rs`），唯独「自然播完」漏了——它恰恰是最常见的一条。
                if let Some(stream) = self.active_stream.take() {
                    stream.cancel();
                }
                // 一首歌结束是天然的释放边界：这里把「已 free 但没还给 OS」的页
                // 交还，而不是等到下一首 `load` 时才做——那时新的分配已经发生，
                // 刚还回去的页又被占回来了。
                crate::audio::engine::trim_heap();

                // 试听片段播完 ≠ 整首播完。这里不能走自动切歌：用户会以为是
                // 「会员没生效、听了几十秒就跳歌」，而实际是没拿到完整版。
                // 停在当前曲目并说清楚原因，把选择权交还给用户。
                if self.state.current_is_trial {
                    self.state.current_is_trial = false;
                    self.state.playback = PlaybackState::Stopped;
                    self.state
                        .warn("试听片段已播完（完整版需要对应会员），按 n 跳下一首");
                    return;
                }

                // 自然播完：顺序模式到底就停，单曲循环原地重播
                self.next_track(false);
            }
            // 边下边播时数据没跟上（读超时），歌**没放完**。
            //
            // 这里唯一不能做的事就是"当成播完了"：那会触发切歌，单曲循环下
            // 就是从头再放一遍——用户看到的就是「进度回到开头」。正确做法是
            // 留住位置，然后从这个位置把这首重新拾起来（`start_at_ms > 0`
            // 会自动走"整首下完再播"那条路，不再依赖流式缓冲）。
            AudioEvent::StreamInterrupted { position_ms } => {
                // 落一条日志：这个现象在界面上只是"卡了一下"，而排查时最需要的
                // 恰恰是「几点断的、断在哪」，光看现场看不出来
                tlog!(
                    crate::logger::LEVEL_INFO,
                    "边下边播缓冲中断，停在 {} ms，准备从该位置续播",
                    position_ms
                );
                self.state.busy = None;
                self.state.download_progress = None;
                self.state.playback = PlaybackState::Stopped;
                // 位置必须留着：界面停在断流的地方，按 Space 也能从这儿继续
                self.state.position_ms = position_ms;

                let Some(song) = self.state.current.clone() else {
                    self.state.warn("缓冲中断");
                    return;
                };

                // 每首歌只自动兜一次：网络真断了的话，反复重试只会刷屏。
                //
                // 但自动兜底退出之后**必须把断点交给手动路径**，而且只认 Space：
                // 之前这里只打一句「按 Space 或 Enter 重试」就返回，而 Enter 在浏览态
                // 走 `activate()`（按 Tab/Focus 分派成「播放选中歌曲」「打开歌单」……），
                // 与重试毫无关系；Space 走 `toggle_playback()`，位置取自 `state.resume`
                // （会话恢复用的，此时为 None），结果是**从头播**。提示说重试、实际是重来。
                if self.stream_retried.as_deref() == Some(song.hash.as_str()) {
                    self.pending_stream_retry = Some((song.clone(), position_ms));
                    self.state.warn(format!(
                        "《{}》缓冲中断在 {}，按 Space 从断点重试",
                        song.name,
                        format_duration_ms(position_ms)
                    ));
                    return;
                }

                self.stream_retried = Some(song.hash.clone());
                self.state.warn(format!(
                    "《{}》缓冲中断，正从 {} 继续…",
                    song.name,
                    format_duration_ms(position_ms)
                ));
                self.start_playback(song, position_ms);
            }
            // 换设备失败：旧设备照旧在播，只提示，不动播放状态
            AudioEvent::DeviceSwitchFailed(message) => {
                self.pending_device_resume = None;
                self.state.warn(message);
            }
            AudioEvent::Failed(message) => {
                self.state.busy = None;
                self.state.download_progress = None;
                // 换设备失败时旧设备还活着，刚才那首也还在播，别再排队续播了
                self.pending_device_resume = None;
                // 这次失败可能就来自这条流，句柄留着没用了
                self.active_stream = None;
                self.state.playback = PlaybackState::Stopped;
                // 位置留着：用户按 Space 能从停住的地方再来一次，而不是从头
                self.mark_resume_point();
                self.state.error(message);
            }
        }
    }

    /// 定时心跳：同步播放状态、推进歌词、低频测量缓存占用。
    fn tick(&mut self) {
        self.state.ticks = self.state.ticks.wrapping_add(1);
        self.state.playback = self.audio.state();

        // 定期落盘会话。只在正常退出时存是不够的——关机、断电、进程被杀时
        // `shutdown()` 根本不会执行，上次进度就丢了（用户实测「重启就没了」）。
        // 这里每 30 秒存一次兜底，最坏情况丢半分钟进度。
        if self.last_session_save.elapsed() >= std::time::Duration::from_secs(30) {
            self.persist_session();
            self.last_session_save = std::time::Instant::now();
        }

        // 内存追踪（`KUGOU_TUI_MEM_TRACE=1`）：每 5 秒把 RSS 与当前曲目记进日志。
        //
        // 为什么挂在 tick 上而不是另起一个线程：日志的时间戳要与播放事件对齐，
        // 同一个写入者才能保证顺序——排查「每首歌涨多少」全靠这个顺序。开销是
        // 5 秒一次读 `/proc` 加一行 format，关掉时连读都不做。
        if crate::logger::mem_trace_enabled()
            && self.last_mem_trace.elapsed() >= std::time::Duration::from_secs(5)
        {
            self.last_mem_trace = std::time::Instant::now();
            let current = self
                .state
                .current
                .as_ref()
                .map(describe_song)
                .unwrap_or_else(|| "（无）".to_string());
            // 除了 RSS，把「可能累积的东西」的计数一起打出来。
            //
            // 光有 RSS 只能看出「涨了」，看不出「谁在涨」——这几个计数才是把范围
            // 收窄到某一条路径的东西：
            //   streams    流式下载登记表（正常 0 或 1，涨了就是任务没摘除）
            //   queue      播放队列（跨音源播放时可能被追加）
            //   hit_zones  每帧回填的命中区（容量应当很快稳定）
            tlog!(
                crate::logger::LEVEL_INFO,
                "[mem] RSS {} KiB，流式任务 {}，队列 {}，命中区 {}，曲目 {}",
                crate::logger::rss_kib(),
                self.downloader.active_streams(),
                self.state.queue.len(),
                self.state.hit_zones.capacity(),
                current
            );
        }
        // 只在音频引擎真的持有曲目时才用它上报的位置。
        //
        // 否则（Stopped）`audio.position_ms()` 返回 0，会把会话恢复出来的进度
        // 冲掉——表现是：启动瞬间能看到上次的进度，等别的请求回来刷了一帧就
        // 变回 00:00。按 Space 又回到正确位置，因为那时才重新用 resume 赋值。
        if matches!(
            self.state.playback,
            PlaybackState::Playing | PlaybackState::Paused
        ) {
            self.state.position_ms = self.audio.position_ms();
        }

        let duration = self.audio.duration_ms();
        if duration > 0 {
            self.state.duration_ms = duration;
        }
        // 算出真实经过时长：动画要按时间缓动，不能按帧数，否则帧率一变观感就变
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_frame_at);
        self.last_frame_at = now;

        // 电平每帧刷新（audio 那一侧只是读原子量，开销可忽略）
        self.state.levels = self.audio.levels();
        // 频谱只在可视化页且真的在播时算：一次 2048 点 FFT 只要零点几毫秒，
        // 但为所有页面每帧都付这份钱没必要——别的页面根本不显示它。
        let wants_spectrum = !self.state.config.lite_mode
            && self.state.tab == Tab::Visualizer
            && self.state.playback == PlaybackState::Playing;
        if wants_spectrum {
            self.state.spectrum = self.audio.spectrum(BAND_COUNT);
        } else {
            self.state.spectrum.clear();
        }
        self.state.advance_visualizer(elapsed);
        // 歌词换行的过渡时钟。放在 `update_active_lyric` **之前**：这样本帧刚换的行
        // 从 t = 0 开始渲染，不会先闪一帧稳态再开始淡。
        self.state.lyric.advance_transition(elapsed);

        // 桌面集成只在 Unix 上存在（MPRIS / StatusNotifierItem 都是 D-Bus 接口）
        #[cfg(unix)]
        {
            self.sync_mpris();
            self.sync_tray();
        }

        if self.state.playback == PlaybackState::Playing {
            self.update_active_lyric();
        }

        // 登录中：每约 2 秒轮询一次扫码状态（tick 默认 200ms，10 拍 = 2s）
        if self.state.login.is_some() && self.state.ticks.is_multiple_of(10) {
            self.poll_login();
        }

        if self.state.ticks.is_multiple_of(CACHE_MEASURE_TICKS) {
            self.refresh_cache_usage();
        }
    }

    /// 测量缓存占用。目录扫描是同步 IO，扔到阻塞线程池执行。
    pub fn refresh_cache_usage(&mut self) {
        let cache = self.cache.clone();
        let bus = self.bus.clone();
        self.runtime.spawn_blocking(move || {
            bus.emit(Loaded::CacheUsage(cache.total_bytes()));
        });
    }

    fn update_active_lyric(&mut self) {
        // 正值表示歌词提前，所以要从播放位置里减掉偏移
        let position = (self.state.position_ms as i64 - self.state.config.lyric_offset_ms).max(0);
        let index = self.state.lyric.lyric.index_at(position as u64);
        if index == self.state.lyric.active_line {
            return;
        }
        // 换行了：记下旧锚点、算这次该用多长的过渡。
        //
        // 时长在这里算而不是在渲染层：它要用到「行距」与「跨了几行」，都是
        // 状态层的知识；渲染层只管拿一个进度 t 去插值。
        let total =
            self.state
                .lyric
                .transition_ms_for(&self.state.config, self.state.duration_ms, index);
        self.state.lyric.retarget(index, total);
    }

    /// 取一条事件，最多处理 [`MAX_EVENTS_PER_FRAME`] 条，防止事件洪水饿死渲染。
    pub fn drain_events(&mut self) {
        for _ in 0..MAX_EVENTS_PER_FRAME {
            let Ok(event) = self.receiver.try_recv() else {
                return;
            };
            self.handle_event(event);
            if self.state.should_quit {
                return;
            }
        }
    }
}

/// 点击歌词行时该跳到的播放位置。
///
/// 抽成自由函数是为了能直接测：`+ lyric_offset_ms` 这一步漏了**不会报错**，
/// 只会「跳过去正好差一句」——偏移为 0 时完全看不出来，而调过偏移的人一眼就发现。
/// 这正是那种只能靠测试兜住的错。
pub fn lyric_seek_target(line_time_ms: u64, lyric_offset_ms: i64) -> u64 {
    line_time_ms.saturating_add_signed(lyric_offset_ms)
}

/// `歌手 - 歌名`，用于状态栏与提示。
pub(super) fn describe_song(song: &Song) -> String {
    let singers = song.singer_text();
    if singers == "未知歌手" {
        song.name.clone()
    } else {
        format!("{singers} - {}", song.name)
    }
}

/// 歌手的副标题。
///
/// 酷狗的歌手列表接口不填 `songcount`（实测恒为 0 或缺失），所以只在真的有数字时
/// 才显示「N 首」——否则整页都是误导性的「0 首」。
pub fn artist_subtitle(artist: &Artist) -> String {
    let mut parts = Vec::new();
    if let Some(songs) = artist.song_count.filter(|count| *count > 0) {
        parts.push(format!("{songs} 首"));
    }
    if let Some(fans) = artist.follower_count.filter(|count| *count > 0) {
        parts.push(format!("{fans} 粉丝"));
    }
    parts.join(" · ")
}

/// 歌单的副标题。
pub fn playlist_subtitle(playlist: &Playlist) -> String {
    let mut parts = Vec::new();
    if playlist.song_count > 0 {
        parts.push(format!("{} 首", playlist.song_count));
    }
    if let Some(creator) = playlist.creator.as_deref().filter(|name| !name.is_empty()) {
        parts.push(format!("by {creator}"));
    }
    if playlist.is_writable() {
        parts.push("可写".to_string());
    }
    parts.join(" · ")
}

/// 榜单的副标题。
pub fn rank_subtitle(board: &RankBoard) -> String {
    board
        .update_frequency
        .clone()
        .unwrap_or_else(|| format!("#{}", board.id))
}

/// 打开歌单时，首屏先取哪一页。
///
/// 列表**倒序**显示时（默认如此，`o` 键可切），出现在最上面的是歌单的**最后一页**。
/// 所以首屏也要从末尾取：否则用户一进歌单先看到的是一屏最老的歌，几秒后整表到位、
/// 画面整体翻一次，才变成他心里的"第一首"。实测就是这么反馈的——进「我喜欢」先看到
/// 歌单开头的歌，而最上面最终变成的是最后加进去的那首。
///
/// 总页数由歌单元数据里的曲数算；曲数拿不到（有的接口不给）就算不出来，只能退回
/// 第 1 页（老行为）。
fn first_screen_page(sort_descending: bool, song_count: u32, page_limit: u32) -> u32 {
    if sort_descending && song_count > page_limit {
        song_count.div_ceil(page_limit)
    } else {
        1
    }
}

/// 首屏拿到那一页之后，还要不要继续把整表取回来。
///
/// 「这一页不满 ⇒ 这就是全部」**只在第 1 页成立**。首屏改从末尾取之后（见
/// [`first_screen_page`]），末页不满页是常态——414 首 = 13×30 + 24，末页就是 24 首。
/// 照搬老判据会让「整表补齐」永不出门，界面从此只显示那 24 首。
fn needs_full_fetch(fetched_page: u32, received: usize, page_limit: u32) -> bool {
    fetched_page > 1 || received >= page_limit as usize
}

// ======================================================================
// 迟到的异步结果
//
// 每个请求都是 `tokio::spawn` 出去的，回来的顺序不保证。用户在这中间完全可以
// 再搜一次、再开一个歌单、再点一位歌手——那条旧结果如果照单全收，界面就会变成
// 「输入框写着 B、列表是 A」这类**看起来就是数据错了**的状态。
//
// 所以每个结果回来时都要先回答一个问题：**它还是「现在」要的吗？**
//
// 这里用「身份比对」而不是给每次请求编号（generation / request id）：
// 播放那一侧早就这么做（`App::is_current`、歌词按 hash 比对），沿用同一套判据
// 只需读一个字段，而计数器要一路穿过 event、状态与所有发出点，收益一样、改动面大得多。
// 判据本身抽成下面的自由函数，好让它们能单独测——`App` 构造需要音频引擎与运行时，
// 单测里搭不出来。
// ======================================================================

/// 一条搜索结果该不该采纳。
///
/// **只认当前提交的那个关键词**。不判的话：用户搜了 B 之后，A 的结果迟到会把它
/// 整体覆盖；追加（`M` 加载更多）时更糟——上一个词的歌混进新列表，而标题还是新词。
fn accepts_search_result(current_keyword: &str, arriving_keyword: &str) -> bool {
    !arriving_keyword.is_empty() && arriving_keyword == current_keyword
}

/// 一条列表结果该不该采纳：**只认当前打开的那一个**。
///
/// 歌单 / 歌手 / 榜单三处共用这条判据——失败模式完全一样：用户已经打开了 B，
/// A 的结果迟到，直接覆盖右侧就是「标题写着 B、内容是 A」。
///
/// 比较的是 id 而不是整个结构（取 `Option<K>` 而不是 `Option<&T>`）：三个模型的
/// id 类型不同（歌单是 `String`，歌手与榜单是 `i64`），而且歌名/封面这类字段
/// 本来就可能随时间变，拿它们当身份反而会误判。
fn accepts_open_item<K: PartialEq>(open: Option<K>, arriving: K) -> bool {
    open.is_some_and(|current| current == arriving)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 点歌词行跳转必须**加回歌词偏移**。
    ///
    /// 漏掉这一步不会报错，只会「跳过去正好差一句」：偏移为 0 时完全看不出来，
    /// 调过偏移的人一眼就发现。三档偏移各验一次。
    #[test]
    fn lyric_seek_adds_the_offset_back() {
        // 偏移为 0：跳过去就是这一句
        assert_eq!(lyric_seek_target(12_000, 0), 12_000);
        // 正值（歌词提前显示）：这一句实际出现在 12_000 + 300
        assert_eq!(lyric_seek_target(12_000, 300), 12_300);
        // 负值（歌词延后）：实际出现在 12_000 - 300
        assert_eq!(lyric_seek_target(12_000, -300), 11_700);
    }

    /// 偏移大到把目标压到 0 以下时不能下溢成天文数字。
    #[test]
    fn lyric_seek_saturates_at_zero() {
        assert_eq!(lyric_seek_target(100, -5_000), 0);
    }

    /// 倒序（默认）时首屏取最后一页——这是「进歌单先看到最老的歌」那个问题的正解。
    #[test]
    fn descending_playlist_opens_at_the_last_page() {
        // 414 首、每页 30：最后一页是第 14 页（391–414）
        assert_eq!(first_screen_page(true, 414, 30), 14);
        // 正好整页时不要多取一页
        assert_eq!(first_screen_page(true, 60, 30), 2);
    }

    /// 正序显示时首屏仍然是第 1 页（这时它才真的是列表头部）。
    #[test]
    fn ascending_playlist_opens_at_the_first_page() {
        assert_eq!(first_screen_page(false, 414, 30), 1);
    }

    /// 一页装得下的歌单（以及曲数未知的）照旧取第 1 页：
    /// 只有一页时"最后一页"就是第 1 页，多绕一步没意义。
    #[test]
    fn small_or_unknown_playlists_stay_on_page_one() {
        assert_eq!(first_screen_page(true, 30, 30), 1);
        assert_eq!(first_screen_page(true, 4, 30), 1);
        assert_eq!(first_screen_page(true, 0, 30), 1);
    }

    /// ↑↓ 只在「无列表的展示页 + 焦点在主区/歌曲列表」时才表示切歌。
    ///
    /// 回归：`Tab` 把焦点交给侧边栏后，在导航里按 ↑↓ 曾经会**切歌**，
    /// 而侧边栏高亮不动——因为判据只看了标签页、没看焦点。
    #[test]
    fn up_down_switches_track_only_where_there_is_no_list() {
        // 首页 / 可视化页 + 主区焦点 → 切歌（这两页没有任何列表）
        for tab in [Tab::Home, Tab::Visualizer] {
            assert!(
                up_down_switches_track(tab, Focus::Primary),
                "{tab:?} 主区应切歌"
            );
            assert!(
                up_down_switches_track(tab, Focus::Secondary),
                "{tab:?} 歌曲列表应切歌"
            );
            // 侧边栏要切栏目、队列要移动队列项，都不能被切歌抢走
            assert!(
                !up_down_switches_track(tab, Focus::Sidebar),
                "{tab:?} 侧边栏焦点下不能切歌"
            );
            assert!(
                !up_down_switches_track(tab, Focus::Queue),
                "{tab:?} 队列焦点下不能切歌"
            );
        }

        // 有列表的页面一律走列表导航
        for tab in [
            Tab::Search,
            Tab::Playlists,
            Tab::Artists,
            Tab::Ranks,
            Tab::Cloud,
            Tab::Queue,
            Tab::Settings,
            Tab::Sources,
        ] {
            assert!(
                !up_down_switches_track(tab, Focus::Primary),
                "{tab:?} 有列表，不能被切歌抢走"
            );
        }
    }

    /// 首屏取的是末页（不满页）时，**仍然**要把整表取回来。
    ///
    /// 这条钉的是一次差点踩进去的坑：老的「不满页 ⇒ 没有更多」判据搬到末页上，
    /// 会让 414 首的歌单永远只显示最后那 24 首。
    #[test]
    fn an_underfull_last_page_still_needs_the_full_fetch() {
        assert!(needs_full_fetch(14, 24, 30), "末页不满也要补齐整表");
        assert!(needs_full_fetch(2, 30, 30), "末页正好满也要");
    }

    /// 第 1 页时的老判据保持不变：不满页就是全部。
    #[test]
    fn a_short_first_page_is_the_whole_playlist() {
        assert!(!needs_full_fetch(1, 12, 30));
        assert!(needs_full_fetch(1, 30, 30));
    }

    /// 迟到的搜索结果必须丢掉——这是「搜了 B 却看到 A 的歌」的守卫。
    ///
    /// 场景：搜「A」→ 结果还在路上 → 用户改搜「B」→ A 的结果先到。
    /// 不判这一下的话它会**整体覆盖** B 的结果（`append` 时则把 A 的歌混进 B 的列表，
    /// 而标题还写着 B）。
    #[test]
    fn a_late_search_result_is_dropped() {
        assert!(
            accepts_search_result("海阔天空", "海阔天空"),
            "当前词的该采纳"
        );
        assert!(
            !accepts_search_result("海阔天空", "光辉岁月"),
            "上一个词的结果必须丢掉"
        );
        // `submitted` 为空 = 还没搜过。空关键词的结果不该冒出来抢屏
        assert!(!accepts_search_result("", ""));
        assert!(!accepts_search_result("海阔天空", ""));
    }

    /// 列表结果只认当前打开的那一个。
    ///
    /// 场景：打开歌单 A → 还在加载 → 用户打开 B → A 的结果先到。照单全收就是
    /// 「标题写着 B、内容是 A」，而且没有任何提示。
    #[test]
    fn only_the_currently_open_item_is_accepted() {
        // 什么都没打开时，任何结果都不认（避免启动期的迟到结果抢屏）
        assert!(!accepts_open_item::<i64>(None, 7), "没打开任何东西时不认");
        assert!(!accepts_open_item(Some(7), 8), "打开的已经换成别的了");
        assert!(accepts_open_item(Some(7), 7));

        // 歌单走的是字符串 id（公开歌单是 global_collection_id，自建是 listid）
        assert!(accepts_open_item(Some("3_abc"), "3_abc"));
        assert!(!accepts_open_item(Some("3_abc"), "3_def"));
        assert!(!accepts_open_item::<&str>(None, "3_abc"));
    }
}
