//! 事件 → 状态变更。
//!
//! 这是整个程序的「大脑」：所有输入（按键、异步结果、音频事件、定时心跳）都在
//! 这里被翻译成对 [`AppState`] 的修改。它**不**渲染、不阻塞、不直接做 IO——
//! 需要 IO 时一律 `runtime.spawn` 一个任务，结果通过事件总线回来。
//!
//! 这条纪律带来的直接好处：`handle_*` 全是纯同步函数，可以逐个单元测试，
//! 也不会因为某个网络请求卡住而冻结界面。

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use ratatui::crossterm::event::MouseEvent;

use crate::api::catalog::PAGE_LIMIT;
use crate::api::cloud::QrStatus;
use crate::api::model::{Artist, Playlist, RankBoard, Song, format_duration_ms};
use crate::app::App;
use crate::app::state::{
    ConfirmAction, Connection, CoverArt, EntryList, Focus, HitTarget, HitZone, LoginPicker,
    LoginState, PromptAction, PromptState, QualityPicker, Tab, move_selection, select_first,
    select_last,
};
use crate::audio::cache::AudioCache;
use crate::audio::engine::AudioSource;
use crate::config::{Config, SUPPORTED_QUALITIES};
use crate::error::AppError;
use crate::source::{PlaylistRef, SourceKind};

use crate::audio::download::{Downloader, PREROLL_BYTES, StreamOutcome};
use crate::audio::engine::{AudioEvent, PlaybackState, SEEK_STEP_MS, VOLUME_STEP};
use crate::audio::spectrum::BAND_COUNT;
use crate::event::{Event, Loaded, LoadingTarget, PlaylistSource, VipClaimOutcome};
use crate::keymap::{Action, CHEATSHEET, KeyMode};
use crate::logger::tlog;
use crate::ui::theme::ThemeName;

/// 翻页时跳过的行数。
const PAGE_STEP: isize = 10;

/// 下载进度上报的字节间隔。太小会让事件通道被高频消息淹没。
const PROGRESS_STEP_BYTES: u64 = 256 * 1024;

/// 「上一首」在播放超过这个时长后，先回到本曲开头而不是切歌。
const RESTART_THRESHOLD_MS: u64 = 3_000;

/// 每帧最多处理的事件数，防止事件洪水导致画面完全停止刷新。
const MAX_EVENTS_PER_FRAME: usize = 64;

/// 每隔多少拍测量一次缓存占用。
///
/// 默认 200ms 一拍，50 拍约 10 秒。目录扫描放在阻塞线程池里，
/// 因此这个频率不会影响界面流畅度。
const CACHE_MEASURE_TICKS: u64 = 50;

/// 搜索接口的最大页数。
///
/// 实测 `page` 超过 16 会返回 `error_code: 149`（Out Page Range）。分页是服务端
/// 硬限，不是约定，所以提在这里并注明：改这个数之前要重新打一次接口确认。
const SEARCH_MAX_PAGES: u32 = 16;

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

/// 桌面组件（MPRIS）用的封面像素尺寸。控件显示得不大，没必要拉原图。
const MPRIS_COVER_SIZE: u32 = 400;

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
    }
}

/// 领取当日概念版 VIP：**先领一天，再升级成畅听 VIP**。
///
/// 两步是连着的——上游要求先领一天才能升级，中间隔 500ms 让服务端状态落库。
/// MoeKoeMusic 的 `getVip()` 也是这两步加同一个间隔。
async fn claim_and_upgrade(api: &crate::api::ApiClient, day: &str) -> crate::error::Result<()> {
    api.claim_day_vip(day).await?;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    api.upgrade_day_vip().await?;
    Ok(())
}

/// 从错误里取出**该给用户看**的那句话。
///
/// `AppError::Api` 的 Display 是「接口 /youth/day/vip 返回错误：code=30201 今日已领取」。
/// 路径那半句对用户是纯噪音（他只有一个可能的操作），而且会把状态栏挤爆、
/// 把真正有用的原因截掉——实测过，112 列的终端上「今日已领取」正好被切没。
fn readable_reason(error: &AppError) -> String {
    match error {
        AppError::Api { message, .. } => message.clone(),
        other => other.to_string(),
    }
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
    // 鼠标
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
            | HitTarget::Settings
            | HitTarget::VipClaim
            | HitTarget::ProfileRetry => {
                return;
            }
        }

        self.open_context_menu();
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

    /// 设置页上下移动选中项。到头就停住，不回绕——设置项一共十来个，
    /// 绕回去反而容易改错项。
    fn move_settings(&mut self, delta: isize) {
        let len = crate::app::settings::Setting::ALL.len() as isize;
        let next = (self.state.settings_cursor as isize + delta).clamp(0, len - 1);
        self.state.settings_cursor = next as usize;
    }

    /// 点击设置项：点一下选中，**再点一下**改值。
    ///
    /// 之所以不做「点一次就改」：设置行横跨整屏，用户更多时候只是想选中它，
    /// 误触就改值会很难受。两次点击的语义和列表的「双击激活」一致。
    fn click_setting(&mut self, zone: HitZone, mouse: &MouseEvent) {
        let Some(index) = zone.index_at(mouse.row) else {
            return;
        };
        let already_selected = self.state.settings_cursor == index;
        self.state.settings_cursor = index;

        let double = self.state.is_double_click(zone.target, Some(index));
        self.state.set_last_click(zone.target, Some(index));
        if already_selected || double {
            self.adjust_setting(1);
        }
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

    /// 下载指定歌曲。菜单里点的歌不一定是当前在放的那首，所以这里收一首歌
    /// 而不是读 `state.current`。
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

    /// 把设置页选中的那一项按 `delta` 调整一档（左为 -1、右为 +1），并落盘。
    ///
    /// 改完立刻 `save()`：设置页的价值就在于「改了就是改了」，退出时再保存
    /// 的话，崩溃或强制退出会丢掉刚才那几下调整，用户会觉得没生效。
    fn adjust_setting(&mut self, delta: isize) {
        use crate::app::settings as s;

        let Some(setting) = s::Setting::ALL.get(self.state.settings_cursor).copied() else {
            return;
        };
        let config = &mut self.state.config;

        match setting {
            s::Setting::Theme => {
                if let Some(next) = s::cycle(&ThemeName::ALL, config.theme, delta) {
                    config.theme = next;
                }
            }
            s::Setting::Quality => {
                if let Some(next) = s::cycle_str(SUPPORTED_QUALITIES, &config.quality, delta) {
                    config.quality = next.to_string();
                }
            }
            s::Setting::PlaybackMode => {
                if let Some(next) = s::cycle(&s::PLAYBACK_MODES, config.playback_mode, delta) {
                    config.playback_mode = next;
                }
            }
            s::Setting::RefreshMs => {
                if let Some(next) = s::cycle(&s::REFRESH_MS_OPTIONS, config.tick_ms, delta) {
                    config.tick_ms = next;
                }
            }
            s::Setting::LyricOffsetMs => {
                let next = config.lyric_offset_ms + delta as i64 * s::LYRIC_OFFSET_STEP;
                config.lyric_offset_ms = next.clamp(-s::LYRIC_OFFSET_LIMIT, s::LYRIC_OFFSET_LIMIT);
            }
            s::Setting::PageSize => {
                if let Some(next) = s::cycle(&s::PAGE_SIZE_OPTIONS, config.page_size, delta) {
                    config.page_size = next;
                }
            }
            s::Setting::CacheLimitMib => {
                if let Some(next) = s::cycle(&s::CACHE_LIMIT_OPTIONS, config.cache_limit_mib, delta)
                {
                    config.cache_limit_mib = next;
                    // AudioCache 只认构造时传进来的上限，改了要重建。
                    // 它没有别的状态（就是目录 + 上限字节数），重建是安全的。
                    self.cache = AudioCache::new(config.cache_dir.clone(), next);
                    self.refresh_cache_usage();
                }
            }
            // 这三项只影响界面，不进配置文件：它们是「这次会话想不想看」
            // 而不是「以后都要这样」，持久化反而会在下次启动时让人困惑
            s::Setting::BasicColor => {
                if delta != 0 {
                    config.basic_color = !config.basic_color;
                }
            }
            s::Setting::LyricPanel => {
                if delta != 0 {
                    self.state.show_lyric_panel = !self.state.show_lyric_panel;
                }
            }
            s::Setting::Sidebar => {
                if delta != 0 {
                    self.state.sidebar_visible = !self.state.sidebar_visible;
                }
            }
            // 简易模式进配置文件：它是「这台机器要不要省资源」的长期选择，
            // 不像歌词面板那样只关乎这一次会话。
            s::Setting::LiteMode => {
                if delta != 0 {
                    config.lite_mode = !config.lite_mode;
                    // 关掉封面就把已解码的那张也扔了，否则内存不会降
                    if config.lite_mode {
                        self.state.cover = crate::app::state::CoverArt::default();
                    }
                }
            }
            // 路径用 Cycle 在几个预设之间切，配置文件里空着也行
            // （首次启动按 `~/Music` 走，不会坏在「路径不存在」上）。
            s::Setting::DownloadDir => {
                let current = config.download_dir.as_deref().unwrap_or("~/Music");
                if let Some(next) = s::cycle(&s::DOWNLOAD_DIR_OPTIONS, current, delta) {
                    config.download_dir = Some(next.to_string());
                }
            }
            // 进配置文件：这是「这块封面以后都这么铺」的长期选择，不是一次性开关。
            // 改了下一帧就生效——`cover_fill` 是每帧从 config 读的，封面协议会
            // 因为铺满方式变了而重新裁一次图。
            s::Setting::CoverFill => {
                if let Some(next) = s::cycle(&s::COVER_FILLS, config.cover_fill, delta) {
                    config.cover_fill = next;
                }
            }
            // 换一张声卡。设备是在音频线程里重建的，当前这首会停一下，
            // 等新设备就绪（DeviceOpened）再按原位置续播，所以这里单独提示并返回。
            s::Setting::AudioDevice => {
                let options = s::device_options(&self.state.audio_devices);
                let Some(next) = s::cycle_device(&options, config.audio_device.as_deref(), delta)
                else {
                    return;
                };
                let target = next
                    .clone()
                    .unwrap_or_else(|| s::DEFAULT_DEVICE_LABEL.to_string());
                config.audio_device = next.clone();
                // 只在真的有声音在放时才续播：暂停状态下换设备不该自作主张开始播
                self.pending_device_resume = match self.state.playback {
                    PlaybackState::Playing => self
                        .state
                        .current
                        .clone()
                        .map(|song| (song, self.state.position_ms)),
                    _ => None,
                };
                self.audio.use_device(next);
                self.state.info(crate::ui::views::settings::change_notice(
                    setting.label(),
                    &target,
                ));
                if let Err(error) = self.state.config.save() {
                    self.state
                        .error(format!("保存设置失败：{}", error.user_hint()));
                }
                return;
            }
        }

        let label = setting.label();
        let value = s::value_text(setting, &self.state);
        // 这两项只改本次会话的界面，不进配置文件：它们是「现在想不想看」
        // 而不是「以后都这样」，持久化反而会在下次启动时让人困惑
        if matches!(setting, s::Setting::LyricPanel | s::Setting::Sidebar) {
            self.state
                .info(crate::ui::views::settings::change_notice(label, &value));
            return;
        }

        match self.state.config.save() {
            Ok(()) => self
                .state
                .info(crate::ui::views::settings::change_notice(label, &value)),
            Err(error) => self
                .state
                .error(format!("保存设置失败：{}", error.user_hint())),
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

    /// 把焦点切到命中区对应的面板。离开搜索框时退出输入态，否则字母键会被吞掉。
    fn focus_hit_target(&mut self, target: HitTarget) {
        let focus = match target {
            HitTarget::Entries => Focus::Primary,
            HitTarget::Songs => Focus::Secondary,
            HitTarget::Queue => Focus::Queue,
            HitTarget::Settings => Focus::Primary,
            // 领取 VIP 是一行即时动作，不改变焦点——点完继续看首页
            HitTarget::Tab(_)
            | HitTarget::Progress
            | HitTarget::VipClaim
            | HitTarget::ProfileRetry => return,
        };

        if self.state.focus == Focus::Primary && focus != Focus::Primary {
            self.state.search.editing = false;
        }
        self.state.focus = focus;
    }

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
                if self.state.playlists.list.is_empty() && !self.state.playlists.list.load.is_loading() =>
            {
                self.load_plaza_playlists();
            }
            Tab::Artists
                if self.state.artists.list.is_empty() && !self.state.artists.list.load.is_loading() =>
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
    fn move_selection(&mut self, delta: isize) {
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
    fn move_selection_edge(&mut self, to_first: bool) {
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

    /// Esc：回到上一层。
    fn go_back(&mut self) {
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
    // 播放控制
    // ==================================================================

    /// 用一批歌曲替换播放队列并起播第 `index` 首。
    pub fn play_from(&mut self, songs: Vec<Song>, index: usize) {
        let Some(song) = self.state.queue.replace_with(songs, index).cloned() else {
            self.state.warn("列表为空，无法播放");
            return;
        };
        self.sync_queue_cursor(index);
        self.start_playback(song, 0);
    }

    fn play_from_focused_songs(&mut self) {
        let selection = self.state.focused_songs().and_then(|songs| {
            songs
                .selected_index()
                .map(|index| (songs.songs.clone(), index))
        });

        match selection {
            Some((songs, index)) => self.play_from(songs, index),
            None => self.state.warn("当前没有可播放的歌曲"),
        }
    }

    fn play_from_queue(&mut self) {
        let Some(index) = self.state.queue_cursor.selected() else {
            self.state.warn("播放队列为空");
            return;
        };
        let Some(song) = self.state.queue.jump_to(index).cloned() else {
            self.state.warn("播放队列为空");
            return;
        };
        self.start_playback(song, 0);
    }

    fn toggle_playback(&mut self) {
        match self.state.playback {
            // 正在缓冲时忽略，避免连按导致状态错乱
            PlaybackState::Loading => {}
            PlaybackState::Playing | PlaybackState::Paused => self.audio.toggle(),
            PlaybackState::Stopped => {
                if let Some(song) = self.state.queue.current().cloned() {
                    // 会话恢复的那首：从上次的位置续播，而不是从头。
                    // hash 对不上就说明用户换了歌，这个位置作废。
                    let resume = match self.state.resume.take() {
                        Some((hash, ms)) if hash == song.hash => ms,
                        _ => 0,
                    };
                    if resume > 0 {
                        self.state.info(format!(
                            "接着上次播：{}",
                            crate::api::model::format_duration_ms(resume)
                        ));
                    }
                    self.start_playback(song, resume);
                } else {
                    self.play_from_focused_songs();
                }
            }
        }
    }

    fn next_track(&mut self, triggered_by_user: bool) {
        let Some(index) = self.state.queue.advance(triggered_by_user) else {
            // 顺序播放到底：停止而不是循环
            self.audio.stop();
            self.state.playback = PlaybackState::Stopped;
            self.state.position_ms = 0;
            self.state.info("播放队列已到末尾");
            return;
        };
        self.sync_queue_cursor(index);
        if let Some(song) = self.state.queue.current().cloned() {
            self.start_playback(song, 0);
        }
    }

    fn previous_track(&mut self) {
        // 播放超过 3 秒时先回到本曲开头，这是主流播放器的通用行为
        if self.state.position_ms > RESTART_THRESHOLD_MS {
            self.audio.seek_to(0);
            self.state.position_ms = 0;
            return;
        }

        let Some(index) = self.state.queue.retreat() else {
            self.state.warn("播放队列为空");
            return;
        };
        self.sync_queue_cursor(index);
        if let Some(song) = self.state.queue.current().cloned() {
            self.start_playback(song, 0);
        }
    }

    /// 把当前播放信息推给 MPRIS，供桌面组件显示。
    ///
    /// 每次 tick 调一次。开销就是一次互斥锁写入，可忽略；桌面组件的轮询
    /// 频率远低于此，没必要更高频。
    fn sync_mpris(&mut self) {
        // D-Bus 注册是异步的，尚未成功（或压根没有 session bus）时没必要每帧
        // 构造一份快照——那只是白白做几次字符串克隆。
        let Some(handle) = self.mpris.as_ref().filter(|handle| handle.is_connected()) else {
            return;
        };

        let info = match self.state.current.as_ref() {
            Some(song) => crate::mpris::TrackInfo {
                title: song.name.clone(),
                artists: song
                    .singers
                    .iter()
                    .map(|singer| singer.name.clone())
                    .collect(),
                album: song.album_name.clone(),
                // 在这里展开 {size}：MprisSnapshot 只存最终可用的地址，
                // 展开规则收敛到 Song::cover_url，避免各调用点各写一份
                art_url: song.cover_url(MPRIS_COVER_SIZE),
                position_us: (self.state.position_ms as i64) * 1_000,
                duration_us: (self.state.duration_ms as i64) * 1_000,
                status: self.state.playback,
            },
            None => crate::mpris::TrackInfo {
                status: self.state.playback,
                ..Default::default()
            },
        };

        handle.update(info);
    }

    /// 把当前播放信息推给系统托盘，让 ToolTip 和状态图标跟着变。
    ///
    /// 设计取舍和 [`Self::sync_mpris`] 一致：只把数据写到快照里，DBus 的属性刷新
    /// 和 `New*` 信号由托盘线程自己轮询+发，避免反向调用主线程。
    fn sync_tray(&mut self) {
        // 没注册成功时跳过——还没连上 watcher 的进程每帧构造一次快照是白干。
        let Some(handle) = self.tray.as_ref().filter(|handle| handle.is_connected()) else {
            return;
        };

        let info = match self.state.current.as_ref() {
            Some(song) => crate::tray::TrayInfo {
                title: song.name.clone(),
                artists: song
                    .singers
                    .iter()
                    .map(|singer| singer.name.clone())
                    .collect(),
                status: self.state.playback,
            },
            None => crate::tray::TrayInfo {
                status: self.state.playback,
                ..Default::default()
            },
        };

        handle.update(info);
    }

    /// 绝对定位到 `position_ms`。
    ///
    /// 与 [`Self::seek_by`] 的区别：那个是相对步进，这个是"跳到某处"。
    /// MPRIS 的 SetPosition（桌面组件拖进度条）需要后者。
    fn seek_to(&mut self, position_ms: u64) {
        if self.state.current.is_none() {
            return;
        }
        // 夹在时长范围内，避免拖到尽头后位置越界
        let target = if self.state.duration_ms > 0 {
            position_ms.min(self.state.duration_ms.saturating_sub(1))
        } else {
            position_ms
        };

        self.audio.seek_to(target);
        // 乐观更新，让进度条立刻响应；音频线程随后给出真实位置
        self.state.position_ms = target;
    }

    /// 搜索结果「加载更多」：追加下一页。
    ///
    /// 之所以分页而不是一次取全：酷狗搜索只有第 1 页是精确匹配，
    /// 深页塞的是兜底内容（实测搜「黑色幽默」第 2 页起变成有声书）。
    /// 全量合并会把相关结果淹没，所以让用户主动一页页要看。
    fn load_more_search(&mut self) {
        let keyword = self.state.search.submitted.clone();
        if keyword.is_empty() {
            self.state.warn("请先搜索");
            return;
        }
        if self.state.search.results.songs.is_empty() {
            self.state.warn("当前没有搜索结果");
            return;
        }

        let next_page = self.state.search.page + 1;
        // 实测上限 16 页，再往后会返回 code=149 Out Page Range
        if next_page > SEARCH_MAX_PAGES {
            self.state.info("已经到最后一页了");
            return;
        }

        let api = self.api.clone();
        let active_source = self.state.config.active_source_kind();
        let bus = self.bus.clone();
        let page_size = self.state.config.page_size;
        self.state.busy = Some(format!("加载「{keyword}」第 {next_page} 页"));

        self.state.search.page = next_page;

        self.runtime.spawn(async move {
            match active_source
                .search_songs(&api, &keyword, next_page, page_size)
                .await
            {
                Ok(songs) => bus.emit(Loaded::Search {
                    keyword,
                    songs,
                    append: true,
                }),
                Err(error) => {
                    // 越界就是没有更多了，不是故障
                    if error.is_page_out_of_range() {
                        bus.emit(Loaded::Search {
                            keyword,
                            songs: Vec::new(),
                            append: true,
                        });
                    } else {
                        bus.fail_loading(
                            LoadingTarget::SearchResults,
                            format!("加载「{keyword}」更多结果失败"),
                            error,
                        );
                    }
                }
            }
        });
    }

    fn seek_by(&mut self, delta_ms: i64) {
        if self.state.current.is_none() {
            return;
        }
        self.audio.seek_by(delta_ms);
        // 乐观更新，让进度条立刻响应；音频线程随后会给出真实位置
        let next = (self.state.position_ms as i64 + delta_ms).max(0) as u64;
        self.state.position_ms = match self.state.duration_ms {
            0 => next,
            duration => next.min(duration),
        };
    }

    fn adjust_volume(&mut self, delta: f32) {
        // 静音状态下按音量键，先解除静音再调整
        let base = self
            .state
            .volume_before_mute
            .take()
            .unwrap_or(self.state.volume);
        let next = (base + delta).clamp(0.0, 1.0);

        self.state.volume = next;
        self.audio.set_volume(next);
        self.state.info(format!("音量 {:.0}%", next * 100.0));
    }

    /// 切换窗口的最小化状态（仅 niri，目前由托盘菜单触发）。
    ///
    /// 把 TUI 从平铺布局里收起来但**音乐照常播**——收起来之后键盘就够不着了，
    /// 这时托盘菜单与 MPRIS 是唯一的控制入口。
    ///
    /// 失败只提示一句 + 记日志：窗口操作不成功不该影响播放。
    fn toggle_window(&mut self) {
        if !crate::window::available() {
            self.state.warn("当前环境不支持窗口控制（需要 niri）");
            return;
        }
        if crate::window::toggle_minimized() {
            self.state.info("已切换窗口最小化");
        } else {
            self.state.warn("窗口操作失败（详见日志）");
        }
    }

    fn toggle_mute(&mut self) {
        match self.state.volume_before_mute.take() {
            Some(previous) => {
                self.state.volume = previous;
                self.audio.set_volume(previous);
                self.state
                    .info(format!("取消静音，音量 {:.0}%", previous * 100.0));
            }
            None => {
                self.state.volume_before_mute = Some(self.state.volume);
                self.state.volume = 0.0;
                self.audio.set_volume(0.0);
                self.state.info("已静音");
            }
        }
    }

    fn adjust_lyric_offset(&mut self, delta_ms: i64) {
        let offset = (self.state.config.lyric_offset_ms + delta_ms).clamp(-10_000, 10_000);
        self.state.config.lyric_offset_ms = offset;
        self.state.info(format!("歌词偏移 {offset:+} ms"));
    }

    /// 把「当前这首听到哪儿了」记进 `state.resume`，供非正常收场之后续播。
    ///
    /// 断流、下载失败都会让歌停下来，但听到的位置是有效的：用户按播放
    /// （Space）就该从这儿接着听，而不是从头。`state.resume` 本来就只对
    /// hash 相同的曲目生效、换歌即作废，语义正好，不必再造一套。
    fn mark_resume_point(&mut self) {
        let Some(song) = self.state.current.as_ref() else {
            return;
        };
        if self.state.position_ms == 0 {
            return;
        }
        self.state.resume = Some((song.hash.clone(), self.state.position_ms));
    }

    /// 停止播放，并把还在下的那条流一起叫停。
    ///
    /// 光调 `audio.stop()` 会漏掉流式下载：音频线程停了，后台任务还在把整首
    /// 往缓冲和 `.part` 文件里灌，谁也不回收它。
    fn stop_playback(&mut self) {
        if let Some(stream) = self.active_stream.take() {
            stream.cancel();
        }
        self.stream_retried = None;
        self.audio.stop();
    }

    /// 起播一首歌：先查缓存，未命中则解析直链 → 下载 → 播放。
    fn start_playback(&mut self, song: Song, start_at_ms: u64) {
        // 上一首如果还在边下边播，通知后台任务收工：任务退出、内存窗口和
        // 文件句柄一起释放。频繁切歌时这一步很关键——否则每个被丢下的下载
        // 任务都会继续把数据往它的缓冲里堆，谁也回收不了。
        if let Some(stream) = self.active_stream.take() {
            stream.cancel();
        }
        // 「已自动兜过一次」的记号按曲目算：换了歌就清掉，同一次断流不重复兜。
        if self.stream_retried.as_deref() != Some(song.hash.as_str()) {
            self.stream_retried = None;
        }

        self.state.current = Some(song.clone());
        self.state.duration_ms = song.duration_ms;
        self.state.position_ms = start_at_ms;
        self.state.download_progress = None;
        self.state.lyric = crate::app::state::LyricPane {
            hash: Some(song.hash.clone()),
            ..Default::default()
        };
        // 切歌后先把旧封面清掉，否则会短暂显示上一张
        if !self.state.cover.belongs_to(&song.hash) {
            self.state.cover = CoverArt::default();
        }
        self.load_cover(&song);

        self.audio.mark_loading();
        self.state.playback = PlaybackState::Loading;

        // privilege 不含可播标记时提前提示，省得用户对着「缓冲中」干等
        if song.looks_playable() {
            self.state
                .info(format!("正在解析播放地址：{}", describe_song(&song)));
        } else {
            self.state.warn(format!(
                "《{}》可能受版权限制（VIP/已下架），若取链失败请换一首",
                song.name
            ));
        }

        self.request_lyric(song.clone());
        self.request_stream(song, start_at_ms);
    }

    fn request_stream(&mut self, song: Song, start_at_ms: u64) {
        let key = song.cache_key(&self.state.config.quality);

        // 缓存命中直接播，零网络开销
        if let Some(path) = self.cache.find(&key) {
            tlog!(crate::logger::LEVEL_DEBUG, "缓存命中：{}", path.display());
            self.audio
                .load(AudioSource::File(path), start_at_ms, song.duration_ms);
            return;
        }

        // 用**这首歌自己的来源**，不是当前音源：队列可以跨音源，
        // 切过音源之后队列里的旧歌仍要能播。
        let source = song.source;
        let api = match self.client_for(source) {
            Ok(client) => client,
            Err(error) => {
                self.state
                    .error(format!("无法连接「{}」：{error}", source.label()));
                return;
            }
        };
        let bus = self.bus.clone();
        let quality = self.state.config.quality.clone();

        self.runtime.spawn(async move {
            match source.song_stream_url(&api, &song, &quality).await {
                Ok(stream) => bus.emit(Loaded::StreamReady {
                    song: Box::new(song),
                    url: stream.url,
                    start_at_ms,
                    is_trial: stream.is_trial,
                    reason: stream.reason,
                }),
                Err(error) => bus.fail(format!("获取《{}》的播放地址失败", song.name), error),
            }
        });
    }

    /// 为**指定音源**构造 HTTP 客户端。
    ///
    /// 队列允许跨音源：播放队列里的歌时不能想当然地用「当前音源」的客户端——
    /// 那会把请求发到另一个服务上，而这首歌的 hash 在那个平台根本查不到。
    /// 地址与凭据都取自目标音源自己的档案。
    fn client_for(&self, kind: SourceKind) -> crate::error::Result<crate::api::ApiClient> {
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

    fn request_lyric(&mut self, song: Song) {
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

    // ==================================================================
    // 数据加载
    // ==================================================================

    /// 启动时带关键词直接进入搜索结果页（对应 `--search`）。
    pub fn startup_search(&mut self, keyword: &str) {
        self.switch_tab(Tab::Search);
        self.state.search.input.set(keyword);
        self.run_search();
    }

    pub fn run_search(&mut self) {
        let keyword = self.state.search.input.text().trim().to_string();
        if keyword.is_empty() {
            self.state.warn("请输入搜索关键词");
            return;
        }

        self.state.search.submitted = keyword.clone();
        self.state.search.editing = false;
        self.state.search.results.load.begin();
        self.state.search.results.title = format!("搜索「{keyword}」");
        self.state.busy = Some(format!("搜索 {keyword}"));

        let api = self.api.clone();
        let active_source = self.state.config.active_source_kind();
        let bus = self.bus.clone();
        let page_size = self.state.config.page_size;

        self.runtime.spawn(async move {
            // 刻意**只取第一页**，不做全量翻页。
            //
            // 实测：酷狗搜索只有第 1 页是精确匹配，深页塞的是兜底内容——
            // 搜「黑色幽默」翻到第 2 页往后全是「卖花的惹不起」这类有声书，
            // 全量合并会把相关结果淹没在垃圾里。MoeKoeMusic 也是分页浏览
            // （`searchResults.value = response.data.lists` 只放当前页）。
            // 想看更多按 `M` 一页页追加，顺序保持服务端的相关性。
            match active_source
                .search_songs(&api, &keyword, 1, page_size)
                .await
            {
                Ok(songs) => bus.emit(Loaded::Search {
                    keyword,
                    songs,
                    append: false,
                }),
                Err(error) => bus.fail_loading(
                    LoadingTarget::SearchResults,
                    format!("搜索「{keyword}」失败"),
                    error,
                ),
            }
        });
    }

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
                    title: "歌单广场".to_string(),
                    items,
                }),
                Err(error) => {
                    bus.fail_loading(LoadingTarget::Playlists, "载入歌单广场失败", error)
                }
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
                Ok(artists) => bus.emit(Loaded::Artists(artists)),
                Err(error) => {
                    bus.fail_loading(LoadingTarget::Artists, "载入歌手列表失败", error)
                }
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
                Err(error) => {
                    bus.fail_loading(LoadingTarget::Ranks, "载入排行榜失败", error)
                }
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

    fn open_selected_playlist(&mut self) {
        let Some(playlist) = self.state.playlists.list.selected().cloned() else {
            self.state.warn("请先选择一个歌单");
            return;
        };
        self.load_playlist_songs(playlist, PlaylistSource::Plaza, false);
    }

    fn open_selected_cloud_playlist(&mut self) {
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

        self.runtime.spawn(async move {
            // 自建/收藏歌单走新版接口（按数字 listid），公开歌单走 global_collection_id
            let is_own = playlist.list_id.filter(|_| playlist.is_own);

            // ---- 首屏：先只取第一页，让界面立刻有内容 ----
            //
            // 大歌单（几百首）即使并发翻页也要好几秒，这段时间界面只有一个"载入中"，
            // 体验很差。学 MoeKoeMusic 的做法：先给首屏，剩下的后台继续取。
            // 它那边是滚动到底再加载；我们一次性取完，但**先让用户看到东西**。
            let first = {
                // 走 `active_source` 而不是 `api`：分页同样是音源相关的——酷狗那两个
                // 端点（参数是 `page` + `pagesize`）网易云根本没有，直接调 `ApiClient`
                // 会让网易云下打开歌单必然 404；而首屏失败会 return，连下面的后台
                // 补全都走不到，表现就是「歌单里的歌一直 404」。
                let target = match is_own {
                    Some(list_id) => PlaylistRef::Own(list_id),
                    None => PlaylistRef::Public(&playlist.id),
                };
                active_source
                    .playlist_tracks_page(&api, target, 1, PAGE_LIMIT, fresh)
                    .await
            };

            match first {
                Ok(songs) => {
                    let has_more = songs.len() >= PAGE_LIMIT as usize;
                    bus.emit(Loaded::PlaylistTracks {
                        playlist: playlist.clone(),
                        songs,
                        source,
                    });
                    // 不足一页说明这首页就是全部，没必要再取
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

    fn open_selected_artist(&mut self) {
        let Some(artist) = self.state.artists.list.selected().cloned() else {
            self.state.warn("请先选择一位歌手");
            return;
        };

        let api = self.api.clone();
        let active_source = self.state.config.active_source_kind();
        let bus = self.bus.clone();

        self.state.artists.songs.load.begin();
        self.state.artists.songs.title = artist.name.clone();
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

    fn open_selected_rank(&mut self) {
        let Some(board) = self.state.ranks.list.selected().cloned() else {
            self.state.warn("请先选择一个榜单");
            return;
        };

        let api = self.api.clone();
        let active_source = self.state.config.active_source_kind();
        let bus = self.bus.clone();

        self.state.ranks.songs.load.begin();
        self.state.ranks.songs.title = board.name.clone();
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
    // 队列与云端
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

    // ==================================================================
    // 扫码登录
    // ==================================================================

    /// `L`：在应用内开始扫码登录。
    ///
    /// 流程是 `/login/qr/key` → `/login/qr/create` → 轮询 `/login/qr/check`。
    /// 二维码直接渲染在界面上，不用切终端，也不用额外依赖 `qrencode` 之类的命令行工具。
    fn start_login(&mut self) {
        // 先决定「给哪个音源登录」：登录态按音源分开存，登错了地方等于没登。
        self.open_login_picker();
    }

    /// 弹出音源选择器。只有一个候选时直接跳过，不多一次交互。
    fn open_login_picker(&mut self) {
        let candidates: Vec<SourceKind> = SourceKind::ALL
            .iter()
            .copied()
            .filter(|kind| kind.capability().login)
            .collect();

        match candidates.len() {
            0 => self.state.warn("当前没有任何音源支持登录"),
            1 => self.begin_login_for(candidates[0]),
            _ => {
                let mut picker = LoginPicker {
                    candidates,
                    ..Default::default()
                };
                // 默认停在当前音源上：多数情况下用户就是想登这个
                let current = self.state.config.active_source_kind();
                if let Some(index) = picker.candidates.iter().position(|kind| *kind == current) {
                    picker.cursor.select(Some(index));
                } else {
                    picker.cursor.select(Some(0));
                }
                self.state.login_picker = Some(picker);
            }
        }
    }

    /// 选定了音源：必要时先切过去（会重建 HTTP 客户端），再走扫码。
    fn confirm_login_source(&mut self) {
        let Some(picker) = self.state.login_picker.take() else {
            return;
        };
        let Some(kind) = picker.selected() else {
            return;
        };
        self.begin_login_for(kind);
    }

    /// 对指定音源开始扫码登录。
    fn begin_login_for(&mut self, kind: SourceKind) {
        // 音源不同就先切过去：登录请求要发到那个服务上，凭据也要存进它的档案。
        if kind != self.state.config.active_source_kind() {
            self.switch_source_to(kind);
        }

        // 已登录时**不能**就此挡住。
        //
        // `logged_in` 只看 cookie 里有没有 `token=` 字段，判断不出 token 是否已经
        // 被服务端作废（实测 `/user/playlist` 会返回 20017）。若在这里直接返回，
        // 用户就会陷入死局：云端功能全部报登录失效，可按 `L` 只回一句「已登录」，
        // 没有任何途径重新扫码。
        //
        // 但仍要确认一次：凭据有效时误按 `L` 会把它冲掉。
        if self.state.logged_in {
            self.state.pending_confirm = Some(ConfirmAction::Relogin);
            return;
        }

        self.begin_login();
    }

    /// 确认后重新登录：清掉旧凭据（保留 dfid，它是设备指纹不是登录态）再扫码。
    fn relogin(&mut self) {
        self.state.config.cookie = None;
        self.state.logged_in = false;
        // cookie 清空后 cookie_header() 只会剩下 dfid，正是想要的效果
        self.api.set_cookie(self.state.config.cookie_header());
        self.begin_login();
    }

    /// 真正发起扫码请求。与「要不要扫」的判断分开，两条入口共用。
    fn begin_login(&mut self) {
        if let Some(login) = self.state.login.as_ref() {
            if !login.finished {
                self.state.info("登录已在进行中，扫码或按 Esc 取消");
                return;
            }
        }

        // 节流：每次取 key 都是向服务端申请一个新的登录会话，短时间内反复申请
        // 会被判「登录频繁」，手机端就扫不了了（实测用户就是这么被限的）。
        // 宁可让用户多等几秒，也别把账号搞限流。
        const QR_KEY_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(30);
        if let Some(last) = self.state.last_qr_key_at {
            let waited = last.elapsed();
            if waited < QR_KEY_COOLDOWN {
                let left = (QR_KEY_COOLDOWN - waited).as_secs() + 1;
                self.state.warn(format!(
                    "取二维码太频繁了，请 {left} 秒后再按 L（频繁申请会被服务端限流）"
                ));
                return;
            }
        }
        self.state.last_qr_key_at = Some(Instant::now());

        self.state.login = Some(LoginState {
            message: "正在获取二维码…".to_string(),
            ..Default::default()
        });

        let api = self.api.clone();
        let bus = self.bus.clone();
        let active_source = self.state.config.active_source_kind();

        self.runtime.spawn(async move {
            let key = match active_source.login_qr_key(&api).await {
                Ok(key) => key,
                Err(error) => {
                    bus.fail("获取登录二维码失败", error);
                    return;
                }
            };

            match active_source.login_qr_create(&api, &key).await {
                Ok(content) => bus.emit(Loaded::LoginQr { key, content }),
                Err(error) => bus.fail("生成登录二维码失败", error),
            }
        });
    }

    /// 轮询扫码状态。由 tick 每约 2 秒调用一次。
    fn poll_login(&mut self) {
        let Some(login) = self.state.login.as_ref() else {
            return;
        };
        if login.finished || login.key.is_empty() {
            return;
        }

        let key = login.key.clone();
        let api = self.api.clone();
        let bus = self.bus.clone();
        let active_source = self.state.config.active_source_kind();

        self.runtime.spawn(async move {
            let check = match active_source.login_qr_check(&api, &key).await {
                Ok(check) => check,
                Err(error) => {
                    bus.fail("查询扫码状态失败", error);
                    return;
                }
            };

            match check.status {
                QrStatus::Expired => bus.emit(Loaded::LoginFailed {
                    message: "二维码已过期，请按 Esc 后重新按 L".to_string(),
                }),
                QrStatus::Waiting => bus.emit(Loaded::LoginStatus {
                    message: "等待扫码…".to_string(),
                }),
                QrStatus::Pending => bus.emit(Loaded::LoginStatus {
                    message: "已扫码，请在手机上确认".to_string(),
                }),
                QrStatus::Success => {
                    // 登录态由服务端持有的音源（如网易云）拿不到 token，
                    // 成功就是成功，不该报「未拿到 token」。
                    if !active_source.capability().client_token {
                        bus.emit(Loaded::LoginSucceeded {
                            token: None,
                            userid: None,
                            cookie: check.cookie.clone(),
                        })
                    } else {
                        match (check.token, check.userid) {
                            (Some(token), Some(userid)) => bus.emit(Loaded::LoginSucceeded {
                                token: Some(token),
                                userid: Some(userid),
                                cookie: None,
                            }),
                            _ => bus.emit(Loaded::LoginFailed {
                                message: "扫码已授权，但未拿到 token".to_string(),
                            }),
                        }
                    }
                }
            }
        });
    }

    /// 结束登录（成功或失败）。
    fn finish_login(&mut self, succeeded: bool, message: String) {
        self.state.login = Some(LoginState {
            finished: true,
            succeeded,
            message,
            ..Default::default()
        });
    }

    /// 兜底：服务端下发的 cookie 为空时（理论上不会，但万一）的收尾路径。
    ///
    /// 正常路径走 `apply_server_cookie`——把含 `MUSIC_U` 的 cookie 写进
    /// 配置，热更新 ApiClient 并存盘。这里**只**在服务端 cookie 为空字符串
    /// 时被兜住，只更新界面状态，没有任何凭据可存。
    fn finish_server_side_login(&mut self) {
        let kind = self.state.config.active_source_kind();
        self.state.logged_in = true;
        self.finish_login(
            true,
            format!(
                "「{}」已在服务端完成登录（凭据由 {} 保管，未写入本地配置）",
                kind.label(),
                kind.service_name()
            ),
        );
    }

    /// 写入登录凭据并热更新 ApiClient 的 cookie。
    fn apply_login(&mut self, token: String, userid: String) {
        let cookie = format!("token={token}; userid={userid}");

        self.state.config.cookie = Some(cookie);
        // 必须走 `cookie_header()`：它会把 dfid 拼进去。直接用裸 cookie 会把
        // 已有的 dfid 冲掉，本次会话取播放直链就会报「本次请求需要验证」。
        self.api.set_cookie(self.state.config.cookie_header());

        let config_path = Config::path();
        match self.state.config.save() {
            Ok(()) => {
                self.state.logged_in = true;
                // 刻意**不回显 cookie**：它等价于账号密码，显示在界面上会被旁观者看到。
                // 只告诉用户写到了哪里。
                self.finish_login(
                    true,
                    format!(
                        "登录成功（userid={userid}），凭据已写入 {}",
                        config_path.display()
                    ),
                );
                self.state.success("登录成功，按 Esc 关闭");
                // 顺带取一次会员信息，界面上能直接看到服务端认定的会员形态
                self.fetch_vip_status();
                self.fetch_user_info();
                // 概念版账号刚登录就把当天的 VIP 领了——这就是它的机制，
                // 「登录就是 VIP」。今天已经领过的话内部会直接返回。
                self.maybe_claim_daily_vip();
            }
            Err(error) => {
                self.finish_login(false, format!("保存登录凭据失败：{error}"));
            }
        }
    }

    /// 登录凭证由**服务端下发**时的收尾（网易云走这条路）。
    ///
    /// 网易云的登录态**就是** `login/qr/check` 响应里那个 cookie（含 `MUSIC_U`）。
    /// 不接住并写进配置，之后的请求不带任何身份：界面上写着「登录成功」，可
    /// `/user/playlist` 拿不到 uid、云端歌单照旧报「尚未登录」——成功只是个谎言。
    ///
    /// 与 [`Self::apply_login`] 的差别只是凭据从哪来：那边自己拼 `token=; userid=`，
    /// 这边直接用服务端给的一整串。
    fn apply_server_cookie(&mut self, cookie: String) {
        self.state.config.cookie = Some(cookie);
        self.api.set_cookie(self.state.config.cookie_header());

        let config_path = Config::path();
        match self.state.config.save() {
            Ok(()) => {
                self.state.logged_in = true;
                // 同样**不回显** cookie：它等价于账号密码。
                self.finish_login(
                    true,
                    format!(
                        "「{}」登录成功，凭据已写入 {}",
                        self.state.config.active_source_kind().label(),
                        config_path.display()
                    ),
                );
                self.state.success("登录成功，按 Esc 关闭");
                self.fetch_vip_status();
                self.fetch_user_info();
                self.maybe_claim_daily_vip();
            }
            Err(error) => {
                self.finish_login(false, format!("保存登录凭据失败：{error}"));
            }
        }
    }

    /// `v`：切换到下一个音源（酷狗 ↔ 酷狗概念版）。
    ///
    /// 只换「去哪儿请求 + 带什么身份」，**不碰播放队列、不打断当前曲目**——
    /// 正在放的音频已经在本地缓存里，换音源没有理由把它停掉。
    /// 数字键 1-9 与 0：切换到对应的标签页。
    ///
    /// 早先这里按焦点区分语义（侧边栏里切页、列表里跳到第 N 项）。但「同一个键
    /// 两种行为」并不直观——用户按下去之前得先想「现在焦点在哪」。列表内定位
    /// 有 `g`/`G` 与 `PgUp`/`PgDn` 已经够用，数字键留给导航更清晰。
    fn handle_digit(&mut self, number: u8) {
        if let Some(tab) = Tab::from_number(number) {
            self.switch_tab(tab);
        }
    }

    /// `v`：打开音源管理页。
    ///
    /// 以前这个键是「循环切到下一个音源」，但音源多了之后循环切很盲目——用户
    /// 不知道下一个是谁，切错了还得再切一圈回来。改为打开管理页，把选择摆出来。
    /// 真正的切换动作（`switch_source_to`）由页面里的操作触发。
    fn open_sources_page(&mut self) {
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

    /// 当前在音源页选中的是哪个音源。
    fn selected_source(&self) -> Option<SourceKind> {
        let kinds = self.state.config.sources.ordered();
        let index = self.state.sources_cursor.selected().unwrap_or(0);
        kinds.get(index).copied()
    }

    /// 启用 / 禁用选中的音源。
    fn toggle_source_enabled(&mut self) {
        let Some(kind) = self.selected_source() else {
            return;
        };
        let profile = self.state.config.sources.profile_mut(kind);
        profile.enabled = !profile.enabled;
        let enabled = profile.enabled;

        // 不能把当前正在用的音源关掉：那样界面会处于「有音源但没选中」的状态。
        if !enabled && self.state.config.active_source_kind() == kind {
            if let Some(fallback) = self.state.config.sources.enabled().first().copied() {
                self.state.config.sync_active_source();
                self.state.config.switch_source(fallback);
                self.state.warn(format!(
                    "已禁用「{}」，当前音源切到「{}」",
                    kind.label(),
                    fallback.label()
                ));
            } else {
                self.state.config.sources.profile_mut(kind).enabled = true;
                self.state.error("至少要保留一个启用的音源");
                return;
            }
        } else {
            self.state.success(format!(
                "「{}」已{}",
                kind.label(),
                if enabled { "启用" } else { "禁用" }
            ));
        }

        self.persist_source_config();
    }

    /// 把选中的音源设为默认（即当前音源）。
    fn set_default_source(&mut self) {
        let Some(kind) = self.selected_source() else {
            return;
        };
        if self.state.config.active_source_kind() == kind {
            self.state
                .info(format!("「{}」已经是当前音源", kind.label()));
            return;
        }
        if !self.state.config.sources.profile(kind).enabled {
            self.state
                .warn(format!("「{}」已禁用，先按 Enter 启用", kind.label()));
            return;
        }
        self.switch_source_to(kind);
    }

    /// 调整选中音源的优先级（`raise = true` 表示往前排）。
    ///
    /// 直接交换相邻两项的 priority：比「整体重排」改动小，也更符合直觉。
    fn shift_source_priority(&mut self, raise: bool) {
        let Some(kind) = self.selected_source() else {
            return;
        };
        let mut kinds = self.state.config.sources.ordered();
        let Some(position) = kinds.iter().position(|candidate| *candidate == kind) else {
            return;
        };
        let target = if raise {
            position.checked_sub(1)
        } else {
            (position + 1 < kinds.len()).then_some(position + 1)
        };
        let Some(target) = target else {
            self.state.info(if raise {
                "已经是第一个"
            } else {
                "已经是最后一个"
            });
            return;
        };
        kinds.swap(position, target);

        // 按新顺序重排 priority，间隔 10 方便以后往中间插
        for (index, kind) in kinds.iter().enumerate() {
            self.state.config.sources.profile_mut(*kind).priority = (index as u32 + 1) * 10;
        }
        self.state.success(format!(
            "「{}」优先级已{}（第 {} 位）",
            kind.label(),
            if raise { "上调" } else { "下调" },
            target + 1
        ));
        self.persist_source_config();
    }

    /// 音源配置改动后落盘。失败只提示，不阻断操作。
    fn persist_source_config(&mut self) {
        if let Err(error) = self.state.config.save() {
            self.state.error(format!("保存音源配置失败：{error}"));
        }
    }

    /// 切换到指定音源，并重建 HTTP 客户端。
    pub fn switch_source_to(&mut self, kind: SourceKind) {
        let previous = self.state.config.active_source_kind();
        if previous == kind {
            return;
        }

        // 先把当前身份存回档案，否则切走再切回来时登录态和 dfid 就丢了
        self.state.config.sync_active_source();
        self.state.config.switch_source(kind);
        // 登录态是按音源分开的：切过去之后要按**新音源**的凭据重新判断，
        // 否则会沿用上一个音源的 logged_in，把「未登录」误判成「已登录」，
        // 于是登录流程被「是否覆盖已有凭据」的确认挡住，进不了扫码。
        self.state.logged_in = self.state.config.is_logged_in();

        if let Err(error) = self.state.config.save() {
            self.state
                .warn(format!("音源已切换，但保存配置失败：{error}"));
        }

        match crate::api::ApiClient::new(
            &self.state.config.api_base,
            self.state.config.cookie_header(),
            self.state.config.proxy.as_deref(),
        ) {
            Ok(client) => {
                self.api = client;
                self.state.vip_info = None;
                self.fetch_vip_status();
                self.fetch_user_info();
                self.ensure_device_fingerprint();
                // 切到概念版就把当天 VIP 领了。放在这里而不只在启动时做：
                // 常用标准版的人切换过来时，启动那次早就过去了，否则永远等不到
                // 自动领取。今天已经领过的话内部会直接返回，不会重复打接口。
                self.maybe_claim_daily_vip();
                let capability = kind.capability();
                if !capability.catalog {
                    self.state.warn(format!(
                        "已切换到「{}」，该音源仅支持搜索与播放（歌单/榜单/云端歌单不可用）",
                        kind.label()
                    ));
                } else if self.state.config.cookie.is_none() {
                    self.state.warn(format!(
                        "已切换到「{}」，但该音源还没登录——按 L 重新扫码（两个平台账号不通用）",
                        kind.label()
                    ));
                } else {
                    self.state.success(format!(
                        "已切换到「{}」音源，当前播放不受影响",
                        kind.label()
                    ));
                }
            }
            Err(error) => {
                self.state.error(format!(
                    "切换到「{}」失败：{error}（需要 {} 服务运行在 {}）",
                    kind.label(),
                    kind.service_name(),
                    self.state.config.api_base
                ));
            }
        }
    }

    /// 拉一次会员信息，把摘要显示在侧边栏。
    ///
    /// 这一步是排查「明明有会员却只能试听」的关键：界面上能直接看到服务端认定的
    /// 会员形态与到期时间，不用去翻日志或 curl。
    /// 取当前登录用户的资料（昵称 / 头像 / 等级 / 听歌时长）。
    ///
    /// 跟会员信息一样，取不到不影响听歌，静默降级。
    pub fn fetch_user_info(&mut self) {
        if !self.state.logged_in {
            self.state.user_info = None;
            self.state.user_info_load.succeed();
            return;
        }

        let api = self.api.clone();
        let bus = self.bus.clone();
        // 走音源分派：网易云的资料在 `/user/detail?uid=` 里，而且得先问出 uid；
        // 酷狗的是同一个端点但不带参数。写死哪一个都会让另一边报错。
        let source = self.state.config.active_source_kind();

        self.state.user_info_load.begin();
        self.runtime.spawn(async move {
            match source.user_detail(&api).await {
                Ok(info) => bus.emit(Loaded::UserInfo(Box::new(info))),
                // 早先这里只写日志，于是接口挂掉时首页永远显示「加载中…」——
                // 用户分不清是失败还是慢。失败也得走事件，面板才有出口。
                Err(error) => {
                    bus.fail_loading(LoadingTarget::UserInfo, "获取用户资料失败", error)
                }
            }
        });
    }

    /// 下载并解码头像。
    ///
    /// 走和封面同一条管线，但**不复用** \`state.cover\`——那是当前歌曲的专辑图，
    /// 会被切歌换掉；头像得单独存一份。
    fn load_avatar(&mut self, url: String) {
        if self.state.config.lite_mode {
            return;
        }
        let downloader = self.downloader.clone();
        let bus = self.bus.clone();

        self.runtime.spawn(async move {
            let bytes = match downloader.fetch_bytes(&url).await {
                Ok(bytes) => bytes,
                Err(error) => {
                    tlog!(crate::logger::LEVEL_WARN, "下载头像失败 {url}：{error}");
                    return;
                }
            };
            match image::load_from_memory(&bytes) {
                Ok(image) => bus.emit(Loaded::AvatarReady { image }),
                Err(error) => tlog!(crate::logger::LEVEL_WARN, "解码头像失败 {url}：{error}"),
            }
        });
    }

    pub fn fetch_vip_status(&mut self) {
        // 会员接口只有酷狗有。网易云没有对应端点（`/user/vip/detail` 是 404），
        // 去请求只会白打一次接口，所以先问能力再决定。
        if !self.state.logged_in
            || !self.state.config.active_source_kind().capability().vip
        {
            self.state.vip_info = None;
            return;
        }

        let api = self.api.clone();
        let bus = self.bus.clone();

        self.runtime.spawn(async move {
            match api.user_vip_detail().await {
                Ok(info) => bus.emit(Loaded::VipStatus(Box::new(info))),
                // 取不到会员信息不影响听歌，静默降级即可
                Err(error) => tlog!(crate::logger::LEVEL_WARN, "获取会员信息失败：{error}"),
            }
        });
    }

    /// 启动 / 登录 / 切到概念版时**自动**同步当天的概念版 VIP。
    ///
    /// 与手动触发（[`Self::claim_daily_vip`]）的唯一区别：本地已经记着今天领过时
    /// **直接返回，不打网络**。上游文档写着「尽量别频繁调用」，这接口还带风控。
    pub fn maybe_claim_daily_vip(&mut self) {
        if !self.state.logged_in
            || self.state.config.active_source_kind() != SourceKind::KugouConcept
        {
            return;
        }
        let Some(day) = crate::util::today_local() else {
            // 取不到本地日期就不自动领；手动按键时会给出明确提示
            return;
        };
        if self.state.vip_claimed_day.as_deref() == Some(day.as_str()) {
            return;
        }
        self.start_vip_sync(day, false);
    }

    /// 手动同步当天的概念版 VIP（快捷键 `V`，或点「我的资料」里那一行）。
    ///
    /// 手动时不看本地日期，一律问服务端——本地那个日期只记「**这台机器**领过」，
    /// 你在手机上领过它是不知道的。
    pub fn claim_daily_vip(&mut self) {
        if self.state.vip_claiming {
            return;
        }
        if !self.state.logged_in {
            self.state.warn("领取 VIP 需要先登录（按 L 扫码）");
            return;
        }
        if self.state.config.active_source_kind() != SourceKind::KugouConcept {
            self.state
                .warn("领取 VIP 是概念版专属功能，先按 v 切到「酷狗概念版」");
            return;
        }
        let Some(day) = crate::util::today_local() else {
            // 宁可放弃也不猜：猜错就是白领一天已经过去的 VIP
            self.state
                .warn("取不到本地日期，无法领取 VIP（可到手机端领取）");
            return;
        };
        self.start_vip_sync(day, true);
    }

    /// 同步当日 VIP 的实际动作：**先问服务端今天领过没，没领过才去领**。
    ///
    /// # 为什么必须先查记录
    ///
    /// 领取接口对「今天已经领过」**只回一个 `error_code`、不给描述**（实测如此），
    /// 界面只能显示「服务端未提供错误描述」，看着像程序坏了；更糟的是原来的文案
    /// 会补一句「反复失败请到手机端领取」，而事实恰恰相反——手机上领过了才是原因。
    ///
    /// 只读的 `/youth/month/vip/record` 能直接回答这个问题，而且比本地记的日期可靠：
    /// 领取可能发生在手机或另一台机器上。查到已经领过就不打写请求了，既省一次调用
    /// （接口带风控），也不会报一个与事实相反的错。
    fn start_vip_sync(&mut self, day: String, manual: bool) {
        let api = match self.client_for(SourceKind::KugouConcept) {
            Ok(client) => client,
            Err(error) => {
                self.state.error(format!("无法连接概念版接口：{error}"));
                return;
            }
        };
        let bus = self.bus.clone();

        self.state.vip_claiming = true;
        self.state.info("正在同步今日 VIP…");

        self.runtime.spawn(async move {
            // 查不到记录就当没领过，照常尝试——不能因为一个只读接口失败就放弃领取
            let already = match api.claimed_vip_days().await {
                Ok(days) => days.iter().any(|claimed| claimed == &day),
                Err(error) => {
                    tlog!(crate::logger::LEVEL_WARN, "查询 VIP 领取记录失败：{error}");
                    false
                }
            };

            let outcome = if already {
                VipClaimOutcome::AlreadyClaimed
            } else {
                match claim_and_upgrade(&api, &day).await {
                    Ok(()) => VipClaimOutcome::Claimed,
                    Err(error) => {
                        tlog!(crate::logger::LEVEL_WARN, "领取 VIP 失败：{error}");
                        VipClaimOutcome::Failed(readable_reason(&error))
                    }
                }
            };

            bus.emit(Loaded::VipClaimed {
                day,
                outcome,
                manual,
            });
        });
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

    /// `d`：把选中歌曲从当前云端歌单移除。
    ///
    /// 用的是歌单条目的 `fileid` 而不是 hash —— 歌单里同一首歌的 fileid 才是它在歌单里的位置标识。
    pub fn remove_focused_song_from_cloud(&mut self) {
        // 取 owned 而不是借用：成功后要把它 move 进异步任务里去发刷新事件，
        // 借用既不能 move 进 `'static` 闭包，也会和下面的 `self.state.warn` 抢借用。
        let Some(target) = self.state.sync_target.clone() else {
            self.state.warn("请先在「云端」标签页选中一个歌单");
            return;
        };

        let Some(list_id) = target.list_id else {
            self.state.error("该歌单不可写（缺少 listid）");
            return;
        };

        if !self.state.logged_in {
            self.state.warn("需要登录才能修改云端歌单，按 L 扫码登录");
            return;
        }

        let Some(song) = self.state.selected_song() else {
            self.state.warn("当前没有选中的歌曲");
            return;
        };

        // 酷狗靠**歌单条目的 fileid** 定位，网易云靠**歌曲 id**（`Song::hash`），
        // 所以只有前者需要 fileid——用一个统一的检查把网易云的歌也拦掉是错的。
        let source = self.state.config.active_source_kind();
        if !matches!(source, SourceKind::Netease) && song.file_id.is_none() {
            // 只有歌单接口才给 fileid；搜索结果没有，无法定位酷狗歌单内的条目。
            self.state
                .warn("这首歌不在歌单里（缺少 fileid），无法从歌单移除");
            return;
        }

        let label = describe_song(&song);
        let name = target.name.clone();
        let api = self.api.clone();
        let bus = self.bus.clone();

        self.state.busy = Some(format!("从《{name}》移除歌曲"));

        self.runtime.spawn(async move {
            match api
                .remove_tracks_from_playlist(source, list_id, std::slice::from_ref(&song))
                .await
            {
                Ok(count) => {
                    bus.emit(Loaded::CloudNotice(format!(
                        "已从《{name}》移除 {count} 首歌"
                    )));
                    // 同上
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                    bus.emit(Loaded::CloudPlaylistChanged {
                        playlist: Box::new(target.clone()),
                    });
                }
                Err(error) => bus.fail(format!("从《{}》移除《{}》失败", name, label), error),
            }
        });
    }

    /// 执行「删除云端歌单」。
    fn delete_cloud_playlist(&mut self) {
        let Some(target) = self.state.sync_target.take() else {
            self.state.warn("没有选中的云端歌单");
            return;
        };

        let Some(list_id) = target.list_id else {
            self.state.error("该歌单不可写（缺少 listid）");
            return;
        };

        if !self.state.logged_in {
            self.state.warn("需要登录才能删除云端歌单，按 L 扫码登录");
            return;
        }

        let name = target.name.clone();
        let source = self.state.config.active_source_kind();
        let api = self.api.clone();
        let bus = self.bus.clone();

        self.state.busy = Some(format!("删除歌单《{name}》"));

        self.runtime.spawn(async move {
            match api.delete_playlist(source, list_id).await {
                Ok(()) => bus.emit(Loaded::CloudNotice(format!("已删除歌单《{}》", name))),
                Err(error) => bus.fail(format!("删除歌单《{}》失败", name), error),
            }
        });
    }

    /// 用输入的名字新建云端歌单。
    fn create_cloud_playlist(&mut self, name: String) {
        if name.trim().is_empty() {
            self.state.warn("歌单名称不能为空");
            return;
        }

        if !self.state.logged_in {
            self.state.warn("需要登录才能新建云端歌单，按 L 扫码登录");
            return;
        }

        let source = self.state.config.active_source_kind();
        let api = self.api.clone();
        let bus = self.bus.clone();
        self.state.busy = Some("新建云端歌单".to_string());

        self.runtime.spawn(async move {
            match api.create_playlist(source, &name).await {
                Ok(list_id) => bus.emit(Loaded::CloudNotice(match list_id {
                    Some(id) => format!("已新建歌单《{name}》（listid={id}）"),
                    None => format!("已新建歌单《{name}》"),
                })),
                Err(error) => bus.fail(format!("新建歌单《{}》失败", name), error),
            }
        });
    }

    fn add_focused_song_to_cloud(&mut self) {
        if !self.state.logged_in {
            self.state.warn("云端歌单需要登录，请配置 cookie");
            return;
        }
        // 优先用**当前打开的**歌单：用户眼前就是这个歌单，加到这里才符合直觉。
        // 原来只用 sync_target（上次选的那个），它未必等于眼前这个，于是出现
        // 「在《我喜欢》里按 s，歌加到别的歌单去了，眼前这个不刷新」。
        let Some(target) = self
            .state
            .cloud
            .open_playlist
            .clone()
            .filter(|playlist| playlist.is_writable())
            .or_else(|| self.state.sync_target.clone())
        else {
            self.state.warn("请先在「云端」标签页选中一个歌单作为目标");
            return;
        };
        let Some(list_id) = target.list_id else {
            self.state.error("该歌单不可写（缺少 listid）");
            return;
        };
        let Some(song) = self.state.selected_song() else {
            self.state.warn("当前没有选中的歌曲");
            return;
        };

        let label = describe_song(&song);
        let playlist_name = target.name.clone();
        let source = self.state.config.active_source_kind();
        let api = self.api.clone();
        let bus = self.bus.clone();

        self.state.busy = Some(format!("收藏《{label}》"));

        self.runtime.spawn(async move {
            match api
                .add_tracks_to_playlist(source, list_id, std::slice::from_ref(&song))
                .await
            {
                Ok(_) => {
                    bus.emit(Loaded::CloudNotice(format!(
                        "已把《{label}》收藏到《{playlist_name}》，正在刷新列表"
                    )));
                    // 先给提示，稍等一下再拉列表。真正让「刷新看不到新歌」的是服务端
                    // 2 分钟缓存（已由 fresh=true 绕开），这里只留一点余量给写操作落定。
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                    bus.emit(Loaded::CloudPlaylistChanged {
                        playlist: Box::new(target.clone()),
                    });
                }
                Err(error) => bus.fail(format!("收藏《{label}》失败"), error),
            }
        });
    }

    fn sync_queue_to_cloud(&mut self) {
        if !self.state.logged_in {
            self.state.warn("云端歌单需要登录，请配置 cookie");
            return;
        }
        let Some(target) = self.state.sync_target.clone() else {
            self.state
                .warn("请先在「云端」标签页选中一个歌单作为同步目标");
            return;
        };
        let Some(list_id) = target.list_id else {
            self.state.error("该歌单不可写（缺少 listid）");
            return;
        };
        if self.state.queue.is_empty() {
            self.state.warn("播放队列为空，没有可同步的内容");
            return;
        }

        let songs = self.state.queue.items().to_vec();
        let count = songs.len();
        let playlist_name = target.name.clone();
        let source = self.state.config.active_source_kind();
        let api = self.api.clone();
        let bus = self.bus.clone();

        self.state.busy = Some(format!("同步 {count} 首到《{playlist_name}》"));

        self.runtime.spawn(async move {
            match api.add_tracks_to_playlist(source, list_id, &songs).await {
                Ok(written) => bus.emit(Loaded::CloudNotice(format!(
                    "已把 {written} 首歌同步到《{playlist_name}》"
                ))),
                Err(error) => bus.fail(format!("同步到《{playlist_name}》失败"), error),
            }
        });
    }

    fn sync_queue_cursor(&mut self, index: usize) {
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

            Loaded::Playlists { title, items } => {
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

            Loaded::Artists(artists) => {
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
                self.state.lyric.active_line = None;
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
                self.state.lyric.active_line = None;
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
                if let Some(login) = self.state.login.as_mut() {
                    if !login.finished {
                        login.message = message;
                    }
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
                if let Some(url) = info.pic.clone() {
                    if !self.state.config.lite_mode {
                        self.load_avatar(url);
                    }
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
            LoadingTarget::SearchResults => self.state.search.results.load.fail(reason),
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
            if start_at_ms > 0 {
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

    /// 为当前歌曲取封面（异步）：下载 → 解码 → 生成字符画。
    ///
    /// 失败只记日志：封面是锦上添花，不能因为它让播放流程报错。
    fn load_cover(&mut self, song: &Song) {
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

    /// 事件里的歌曲是否仍是当前播放的那首。
    fn is_current(&self, song: &Song) -> bool {
        self.state
            .current
            .as_ref()
            .map(|current| current.hash == song.hash)
            .unwrap_or(false)
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
                self.active_stream = None;

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

                // 每首歌只自动兜一次：网络真断了的话，反复重试只会刷屏
                if self.stream_retried.as_deref() == Some(song.hash.as_str()) {
                    self.state.warn(format!(
                        "《{}》缓冲中断在 {}，按 Space 或 Enter 重试",
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

        self.sync_mpris();
        self.sync_tray();

        if self.state.playback == PlaybackState::Playing {
            self.update_active_lyric();
        }

        // 登录中：每约 2 秒轮询一次扫码状态（tick 默认 200ms，10 拍 = 2s）
        if self.state.login.is_some() && self.state.ticks % 10 == 0 {
            self.poll_login();
        }

        if self.state.ticks % CACHE_MEASURE_TICKS == 0 {
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
        if index != self.state.lyric.active_line {
            self.state.lyric.active_line = index;
        }
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

/// `歌手 - 歌名`，用于状态栏与提示。
/// 把音质档位翻成人话。
fn quality_label(quality: &str) -> String {
    match quality {
        "128" => "标准 128kbps".to_string(),
        "320" => "较高 320kbps".to_string(),
        "flac" => "无损 FLAC".to_string(),
        "high" => "高品".to_string(),
        "super" => "超高".to_string(),
        "viper_clear" => "蝰蛇母带".to_string(),
        other => other.to_string(),
    }
}

fn describe_song(song: &Song) -> String {
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
