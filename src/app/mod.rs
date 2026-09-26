//! 应用编排层。
//!
//! [`App`] 把三个独立的部分接在一起，并驱动主循环：
//!
//! ```text
//!  ┌────────────┐   Event    ┌──────────────────────────────────┐
//!  │ 输入线程    │ ─────────▶ │                                  │
//!  ├────────────┤            │            EventBus              │
//!  │ 音频线程    │ ─────────▶ │        (crossbeam channel)       │
//!  ├────────────┤            │                                  │
//!  │ Tokio 任务  │ ─────────▶ │                                  │
//!  └────────────┘            └───────────────┬──────────────────┘
//!                                            │ recv_timeout(tick)
//!                                            ▼
//!                             ┌──────────────────────────────┐
//!                             │  主循环（本线程）              │
//!                             │  1. terminal.draw(ui::render) │
//!                             │  2. handle_event(...)         │
//!                             │  3. drain_events()            │
//!                             └──────────────────────────────┘
//! ```
//!
//! 主循环是**唯一**修改 [`AppState`] 的地方，因此不需要任何锁来保护 UI 状态。
//! 跨线程共享的只有「音频位置/音量」这类原子量与一条无锁通道。

/// 猜终端支持哪种图形协议。
///
/// **刻意不用** `Picker::from_query_stdio()`。它向终端发一串查询序列再阻塞读
/// stdin，而那个读**没有自己的超时**：终端一旦不回应（tmux、部分终端、某些 SSH
/// 组合），读线程就永远卡在 `stdin().read()` 上，之后用户按的每一个键都被它
/// 吞掉——实测表现是整个键盘失灵，`q` 都退不出去。它顺手开关的那次 raw mode
/// 还会和 `ratatui::init()` 打架。
///
/// 改成只看环境变量：kitty 与 iTerm2 都会留下明确痕迹，猜不出就退回半块字符。
/// 代价是自动检测不到 sixel（这类终端很少），换来的是绝不会抢走输入、也绝不会
/// 让启动多等两秒。
fn detect_image_picker() -> ratatui_image::picker::Picker {
    use ratatui_image::picker::{Picker, ProtocolType};

    let mut picker = Picker::halfblocks();
    let is_kitty = std::env::var_os("KITTY_WINDOW_ID").is_some()
        || std::env::var("TERM").is_ok_and(|term| term.contains("kitty"));
    let is_iterm2 = std::env::var("TERM_PROGRAM").is_ok_and(|value| value == "iTerm.app");

    if is_kitty {
        picker.set_protocol_type(ProtocolType::Kitty);
    } else if is_iterm2 {
        picker.set_protocol_type(ProtocolType::Iterm2);
    }

    // 记一条供排查：封面显示不对时先看这里。
    //
    // 两条信息都有用：`protocol_type` 不是 kitty/iTerm2 就说明**探测没命中**
    // （alacritty / konsole 等即使支持图形协议也认不出来），封面会退到半块
    // 字符画——那时像素尺寸、裁剪比例全都不参与显示，怎么调都是白调。
    // `font_size` 则是裁剪换算像素时用的唯一依据，而它**永远是硬编码的 10x20**：
    // `Picker::halfblocks()` 从不查询终端真实的单元格尺寸（`from_query_stdio`
    // 才能查，但它会在没有超时的读上卡死键盘，见本函数文档）。
    let font = picker.font_size();
    tlog!(
        crate::logger::LEVEL_INFO,
        "终端图形协议 {:?}，单元格像素 {}x{}",
        picker.protocol_type(),
        font.width,
        font.height
    );
    picker
}

pub mod queue;
pub mod session;
pub mod settings;
pub mod state;
pub mod update;

use std::time::{Duration, Instant};

use anyhow::Context;
use crossbeam_channel::{Receiver, RecvTimeoutError};

use crate::api::ApiClient;
use crate::app::state::Tab;
use crate::audio::engine::PlaybackState;
use crate::audio::{AudioCache, AudioHandle, Downloader};
use crate::config::Config;
use ratatui::crossterm::execute;

use crate::event::{Event, EventBus, Loaded};
use crate::logger::tlog;

pub use state::AppState;

/// 主循环每帧最多处理的事件数在 [`update`] 里定义。
pub struct App {
    pub state: AppState,

    /// 事件总线的发送端。音频线程、输入线程、网络任务各持有一份克隆。
    bus: EventBus,
    /// 接收端只有主循环持有。
    receiver: Receiver<Event>,

    api: ApiClient,
    audio: AudioHandle,
    cache: AudioCache,
    downloader: Downloader,

    /// 网络运行时。只在需要发起请求时 `spawn`，主线程不 `block_on`。
    runtime: tokio::runtime::Runtime,

    /// 上一帧的时间戳，用来算出真实经过时长（dt），供动画做时间无关的缓动。
    last_frame_at: Instant,

    /// 上次把会话写盘的时间。关机/被杀时 `shutdown()` 不会执行，靠 tick 里
    /// 的定期保存兜底——只在退出时存的话，非正常退出就丢进度了。
    last_session_save: Instant,

    /// 切换输出设备后要续播的曲目与位置。
    ///
    /// 换设备是在音频线程里重建设备，正在播的那首会停。这里记下「刚才在放什么、
    /// 放到哪儿」，等 [`crate::audio::engine::AudioEvent::DeviceOpened`] 到达时
    /// 按原位置重新装载——用户侧看不出中断。
    pending_device_resume: Option<(crate::api::model::Song, u64)>,

    /// 当前边下边播的流。切歌 / 停止 / 退出时用它通知后台下载任务收工。
    ///
    /// 不握住它也能播放（音频线程自己有一份），但那样就没人能叫停下载了：
    /// 用户连着切几首，好几个任务的缓冲会一起留在内存里。
    active_stream: Option<crate::audio::streaming::StreamingBuffer>,

    /// 已经自动兜过续播的曲目 hash。
    ///
    /// 边下边播断流时我们会从断点重来一次；这个记号保证同一首只自动兜一次，
    /// 免得网络真的断了还反复重试。按曲目存放：换了歌就重新允许。
    stream_retried: Option<String>,

    /// MPRIS 句柄。没有 D-Bus 时为 `None`（不影响播放，只是桌面集成不可用）。
    mpris: Option<crate::mpris::MprisHandle>,

    /// 托盘句柄。`config.tray == false` 或环境探测失败时为 `None`。
    tray: Option<crate::tray::TrayHandle>,
}

/// 动画帧间隔（约 30fps）。终端里再往上（60fps）看不出差别，但重绘成本是线性的，
/// 白白吃掉「低资源占用」这个卖点，所以取这个折中值。
const ANIMATED_TICK_MS: u64 = 33;

/// 简易模式的刷新间隔下限（5fps）。再慢界面就拖了。
const LITE_TICK_MS: u64 = 200;

/// 这一帧该等多久。
///
/// 抽成自由函数是为了能直接测：这里的取舍（省电 ↔ 顺滑）很容易被无意改坏，
/// 而为了测它去构造一个完整的 `App`（要起 tokio 运行时、网络客户端、音频设备）
/// 代价太大，实际上就不会有人测。
///
/// * `visualizer_animating` —— 可视化页正在播放，频谱柱要连续起落；
/// * `lyric_visible` —— 歌词真的显示在屏幕上，逐字推进要连续。
fn frame_delay(config: &Config, visualizer_animating: bool, lyric_visible: bool) -> Duration {
    let base = config.tick_ms;

    // 简易模式：不做动画提速，并且把刷新压到 5fps（200ms）——省下的都是
    // CPU 与重绘，听歌不受影响。
    if config.lite_mode {
        return Duration::from_millis(base.max(LITE_TICK_MS));
    }
    if (visualizer_animating || lyric_visible) && base > ANIMATED_TICK_MS {
        return Duration::from_millis(ANIMATED_TICK_MS);
    }
    Duration::from_millis(base)
}

impl App {
    /// 装配所有子系统。
    pub fn new(config: Config) -> anyhow::Result<Self> {
        config.ensure_cache_dir().context("创建音频缓存目录失败")?;

        let runtime = tokio::runtime::Builder::new_multi_thread()
            // 2 个 worker 足够：并发上限就是「搜索 + 歌词 + 取链 + 下载」这几条链
            .worker_threads(2)
            .thread_name("kugou-net")
            .enable_all()
            .build()
            .context("创建网络运行时失败")?;

        let (bus, receiver) = EventBus::new();

        let api = ApiClient::new(
            &config.api_base,
            config.cookie_header(),
            config.proxy.as_deref(),
        )
        .context("初始化 API 客户端失败")?;

        // 自定义键位要在第一次读键之前装好，否则首个按键会落到默认键表。
        // 装了多少条只在日志里记，不打扰界面。
        let _custom_keys = crate::keymap::install_custom(&config.keymap);

        let audio = AudioHandle::spawn(bus.clone(), config.volume, config.audio_device.clone());
        let cache = AudioCache::new(config.cache_dir.clone(), config.cache_limit_mib);
        let downloader = Downloader::new(config.proxy.as_deref()).context("初始化下载器失败")?;

        // 先取一份克隆给 MPRIS：bus 随后会被 move 进 App，之后就借不到了
        let mpris = crate::mpris::spawn(bus.clone());

        // 托盘与 MPRIS 共享同一个 bus（用于派发点击动作），但只读 config 一次——
        // 关掉时干脆不 spawn，省掉那条 DBus 连接。
        let tray_handle = if config.tray {
            crate::tray::spawn(bus.clone())
        } else {
            None
        };

        let state = AppState::new(config);

        let mut app = Self {
            state,
            bus,
            receiver,
            api,
            audio,
            cache,
            downloader,
            runtime,
            last_frame_at: Instant::now(),
            last_session_save: Instant::now(),
            pending_device_resume: None,
            active_stream: None,
            stream_retried: None,
            mpris,
            tray: tray_handle,
        };

        app.state.picker = Some(detect_image_picker());
        // 设备列表给设置页用。放在这里而不是 AppState::new 里：枚举设备会加载
        // 音频后端，那是音频线程的地盘，主线程只取一次名字就走。
        app.state.audio_devices = crate::audio::list_output_devices();

        app.restore_session();
        app.announce_readiness();
        // 放在 announce_readiness 之后：这种故障比「未登录」严重，提示不能被覆盖
        if app.audio.spawn_failed() {
            app.state.error("音频线程启动失败，播放不可用（详见日志）");
        }
        app.ensure_device_fingerprint();
        app.refresh_cache_usage();
        app.fetch_vip_status();
        app.fetch_user_info();
        // 放在 restore_session 之后：去重要用到会话里记的「上次领取日期」
        app.maybe_claim_daily_vip();

        let tab = app.state.tab;
        app.ensure_tab_loaded(tab);

        Ok(app)
    }

    /// 启动主循环，返回后终端已恢复。
    pub fn run(&mut self) -> anyhow::Result<()> {
        // stderr 已经在 main 里接到日志上了（见 logger::redirect_stderr_to_log 的
        // 调用点）——那里比这里早，能连音频初始化阶段的报错一起收走。
        let mut terminal = ratatui::init();

        // 设终端标题：窗口列表里认得出来，`window.rs` 也靠它找回自己的窗口
        // （niri 给的 pid 是终端模拟器的，匹配不上）。必须在 init 之后——
        // 部分终端会在切到 alternate screen 时把标题重置回去。
        crate::window::set_terminal_title();

        // 开启鼠标捕获：点击列表、滚轮翻页、点击进度条都要它。失败不影响键盘使用，
        // 有些终端/远程会话不支持，忽略即可。
        let _ = execute!(
            std::io::stdout(),
            ratatui::crossterm::event::EnableMouseCapture
        );

        // 终端进入 raw 模式后再启动输入线程，避免首个按键被行缓冲吃掉
        spawn_input_thread(self.bus.clone());

        let loop_result = self.event_loop(&mut terminal);

        // 先关鼠标捕获再恢复终端，否则有些终端会残留鼠标上报
        let _ = execute!(
            std::io::stdout(),
            ratatui::crossterm::event::DisableMouseCapture
        );
        ratatui::restore();
        self.shutdown();

        loop_result
    }

    /// 主循环。
    ///
    /// 一帧的顺序是「先渲染再取事件」：这样 `recv_timeout` 的等待时间被用来
    /// 呈现上一帧的结果，用户感知到的按键延迟就等于一次渲染加一次唤醒，
    /// 而不是「等满一个 tick 才响应」。
    /// 当前这一帧该等多久。
    ///
    /// 平时用配置的 `tick_ms`（默认 200ms，省电）。但有两处需要连续重绘：
    /// 可视化页的频谱柱，以及**歌词真的显示在屏幕上**时的逐字推进——5fps 下
    /// 再精细的插值也是五格一跳。两处都临时提到 ~30fps（33ms）。
    ///
    /// 为什么不到 60fps：终端一次重绘是整屏 diff，16ms 与 33ms 的观感差别很小，
    /// 代价却是翻倍的 CPU。这里按「看起来顺」而不是「数字好看」取值。
    fn frame_interval(&self) -> Duration {
        let playing = self.state.playback == PlaybackState::Playing;
        let visualizer = playing && self.state.tab == Tab::Visualizer;
        // 歌词可见**且**在播才提速：暂停时那一格不会动，没必要重绘；
        // 不显示歌词时更是白费 CPU——这是本项目「低占用」卖点的一部分。
        let lyric = playing && self.state.lyric_visible;

        frame_delay(&self.state.config, visualizer, lyric)
    }

    fn event_loop(&mut self, terminal: &mut ratatui::DefaultTerminal) -> anyhow::Result<()> {
        loop {
            terminal
                .draw(|frame| crate::ui::render(frame, &mut self.state))
                .context("渲染失败")?;

            match self.receiver.recv_timeout(self.frame_interval()) {
                Ok(event) => self.handle_event(event),
                // 没有事件时用一次心跳推进进度条与歌词
                Err(RecvTimeoutError::Timeout) => self.handle_event(Event::Tick),
                Err(RecvTimeoutError::Disconnected) => break,
            }

            // 把同一帧内积压的事件一并消化，快速连按时画面才不会滞后
            self.drain_events();

            if self.state.should_quit {
                break;
            }
        }

        Ok(())
    }

    /// 启动时的一行提示。
    ///
    /// **只说配置与登录态，不断言连通性。** 这个方法在启动路径上跑，那一刻程序
    /// 一个请求都还没发过；早先它写的是「已连接 {base}」，于是接口全挂也照样这么
    /// 说——用户看到「已连接」就把网络问题排除掉了，然后往别处找原因。
    /// 连通性由真实请求的结果驱动（`App::note_connection`），连上了才会改口。
    fn announce_readiness(&mut self) {
        let base = self.api.base().to_string();
        let source = self.state.config.active_source_kind().label();
        if self.state.logged_in {
            self.state.info(format!("音源 {source} · {base}（已登录）"));
        } else {
            self.state
                .warn(format!("音源 {source} · {base}（未登录，云端歌单不可用）"));
        }
    }

    /// 首次运行时自动探测设备指纹。
    ///
    /// `dfid` 是 `/song/url` 的必需参数，缺失时酷狗会返回「本次请求需要验证」。
    /// 探测失败不阻塞任何功能——只是取播放链接时会更依赖登录态。
    fn ensure_device_fingerprint(&mut self) {
        // `dfid` 是酷狗独有的设备指纹（`/register/dev`），网易云、QQ 音乐没有
        // 这个概念。对它们发这个请求只会白跑一趟，还会在配置里留下一个
        // 语义不明的 device_id，看着像是登录凭据。
        if !self
            .state
            .config
            .active_source_kind()
            .uses_device_fingerprint()
        {
            return;
        }
        if self.state.config.dfid.is_some() {
            return;
        }

        let api = self.api.clone();
        let bus = self.bus.clone();

        self.runtime.spawn(async move {
            match api.fetch_device_fingerprint().await {
                Ok(dfid) => bus.emit(Loaded::DeviceFingerprint(dfid)),
                Err(error) => tlog!(
                    crate::logger::LEVEL_WARN,
                    "获取设备指纹失败（不影响搜索与浏览）：{error}"
                ),
            }
        });
    }

    /// 收尾：保存配置、停掉音频线程。
    fn shutdown(&mut self) {
        if self.state.force_quit {
            tlog!(crate::logger::LEVEL_INFO, "强制退出，跳过配置保存");
        } else {
            self.persist_config();
            self.persist_session();
        }
        // 先把还在下的那条流叫停，再关音频线程：否则音频线程会在
        // `Player::clear()` 里等那个阻塞在 read() 里的解码器（最长 15 秒）
        if let Some(stream) = self.active_stream.take() {
            stream.cancel();
        }
        self.audio.shutdown();
    }

    /// 把「正在听什么」存下来，下次启动原样恢复。
    ///
    /// 队列 + 游标 + 播放位置。关掉终端再打开，不该从零开始。
    fn persist_session(&mut self) {
        let session = crate::app::session::Session {
            queue: self.state.queue.items().to_vec(),
            cursor: self.state.queue.cursor(),
            position_ms: self.state.position_ms,
            vip_claimed_day: self.state.vip_claimed_day.clone(),
        };
        // **队列为空也要写**：`vip_claimed_day` 还得落盘。原先这里 `is_empty()`
        // 就 return，于是从没排过队的用户「今天已经领过 VIP」这个事实永远存不下来，
        // 下次启动又会去领一次——正撞在上游「尽量别频繁调用」和风控上。
        session.save();
        tlog!(
            crate::logger::LEVEL_INFO,
            "会话已保存：{} 首，位置 {} ms",
            session.queue.len(),
            session.position_ms
        );
    }

    /// 恢复上次的会话。
    ///
    /// **刻意不自动播放**：一开程序就出声很吓人，而且可能是在不该出声的场合。
    /// 只把队列和游标填回去，让用户自己按 Space。
    fn restore_session(&mut self) {
        let Some(session) = crate::app::session::Session::load() else {
            return;
        };

        // 先恢复「上次领取 VIP 的日期」——它和队列无关，而下面队列为空会提前
        // return。漏在这里的话，从没排过队的用户每次启动都会重领一次 VIP，
        // 正好踩在上游「尽量别频繁调用」和风控上。
        self.state.vip_claimed_day = session.vip_claimed_day.clone();

        if session.is_empty() {
            return;
        }

        let count = session.queue.len();
        let mode = self.state.config.playback_mode;
        // replace_with 会把游标定位到 cursor，并把 current 设成那首歌
        if let Some(_song) = self
            .state
            .queue
            .replace_with(session.queue, session.cursor.unwrap_or(0))
        {
            self.state.queue.set_mode(mode);
            self.state.current = self.state.queue.current().cloned();
            // 时长也要恢复：进度条是 position / duration 算的，只设 position
            // 而 duration 还是 0 的话，进度条就是空的、时间也显示 00:00 / 00:00——
            // 看着像没恢复，其实位置已经在了。
            self.state.duration_ms = self
                .state
                .current
                .as_ref()
                .map(|song| song.duration_ms)
                .unwrap_or(0);
            self.state.position_ms = session.position_ms;
            // 记住「这首歌播到哪了」，按 Space 时从这里续（见 toggle_playback）。
            // 位置为 0 就不记，免得续播逻辑白白多一条分支。
            if session.position_ms > 0 {
                if let Some(song) = self.state.current.as_ref() {
                    self.state.resume = Some((song.hash.clone(), session.position_ms));
                }
            }
            self.state
                .info(format!("已恢复上次会话：{count} 首 · 按 Space 继续播放"));
            tlog!(
                crate::logger::LEVEL_INFO,
                "会话已恢复：{count} 首，位置 {} ms",
                session.position_ms
            );
        }
    }

    fn persist_config(&mut self) {
        self.state.config.volume = self.state.volume;
        self.state.config.playback_mode = self.state.queue.mode();
        self.state.config.cache_dir = self.cache.root().to_path_buf();
        // `--api-base` 只覆盖本次会话，不能落盘。
        //
        // 地址属于音源自己（见 `Config::sync_active_source` 的注释）。把临时值写进
        // 配置文件，就会出现「选中音源是概念版 :3001、顶层却写着 :3000」——启动器
        // 照顶层值去探活 / 拉服务就会找错端口，程序直接起不来。
        let active = self.state.config.active_source_kind();
        self.state.config.api_base = self.state.config.sources.profile(active).api_base.clone();

        match self.state.config.save() {
            Ok(()) => tlog!(
                crate::logger::LEVEL_INFO,
                "配置已保存到 {}",
                Config::path().display()
            ),
            Err(error) => tlog!(crate::logger::LEVEL_ERROR, "保存配置失败：{error}"),
        }
    }
}

/// 启动终端输入线程。
///
/// 线程阻塞在 `event::read()` 上，退出时不需要显式回收——`main` 返回会让整个
/// 进程结束，阻塞中的线程随之消失。
fn spawn_input_thread(bus: EventBus) {
    let result = std::thread::Builder::new()
        .name("kugou-input".to_string())
        .spawn(move || input_loop(bus));

    if let Err(error) = result {
        tlog!(
            crate::logger::LEVEL_ERROR,
            "启动输入线程失败，键盘将无响应：{error}"
        );
    }
}

fn input_loop(bus: EventBus) {
    use ratatui::crossterm::event::{self, Event as TerminalEvent, KeyEventKind};

    loop {
        match event::read() {
            // 只处理 Press：部分平台还会派发 Release/Repeat，全部处理会让按键翻倍
            Ok(TerminalEvent::Key(key)) if key.kind == KeyEventKind::Press => {
                bus.send(Event::Key(key));
            }
            Ok(TerminalEvent::Resize(_, _)) => {
                bus.send(Event::Resize);
            }
            Ok(TerminalEvent::Mouse(mouse)) => {
                bus.send(Event::Mouse(mouse));
            }
            // 焦点变化、鼠标等事件暂时不关心
            Ok(_) => {}
            Err(error) => {
                tlog!(crate::logger::LEVEL_ERROR, "读取终端输入失败：{error}");
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(tick_ms: u64, lite_mode: bool) -> Config {
        Config {
            tick_ms,
            lite_mode,
            ..Config::default()
        }
    }

    fn millis(delay: Duration) -> u64 {
        delay.as_millis() as u64
    }

    /// 什么都不动时按用户配的间隔走——不能悄悄提速，那会白吃 CPU。
    #[test]
    fn idle_keeps_the_configured_interval() {
        assert_eq!(millis(frame_delay(&config(200, false), false, false)), 200);
        assert_eq!(millis(frame_delay(&config(500, false), false, false)), 500);
    }

    /// 两处该提速的场景：可视化页在播、以及**歌词可见**在播。
    #[test]
    fn animating_raises_the_rate_to_thirty_fps() {
        let cfg = config(200, false);
        assert_eq!(millis(frame_delay(&cfg, true, false)), ANIMATED_TICK_MS);
        assert_eq!(millis(frame_delay(&cfg, false, true)), ANIMATED_TICK_MS);
        assert_eq!(millis(frame_delay(&cfg, true, true)), ANIMATED_TICK_MS);
    }

    /// 用户已经把间隔调得比 33ms 还快时不要反向拖慢——那是他自己要的。
    #[test]
    fn animating_never_slows_a_faster_configured_tick() {
        assert_eq!(millis(frame_delay(&config(16, false), true, false)), 16);
        assert_eq!(millis(frame_delay(&config(16, false), false, true)), 16);
    }

    /// 简易模式的承诺是「不做动画提速」，所以歌词可见也不能把它拉回 30fps。
    /// 这条是那个卖点的守卫：改了这里，低配机器上的占用会悄悄翻几倍。
    #[test]
    fn lite_mode_ignores_every_animation() {
        let cfg = config(50, true);
        assert_eq!(millis(frame_delay(&cfg, true, true)), LITE_TICK_MS);
        assert_eq!(millis(frame_delay(&cfg, false, false)), LITE_TICK_MS);
        // 用户配得比下限还慢时以用户为准
        assert_eq!(millis(frame_delay(&config(1000, true), true, true)), 1000);
    }
}
