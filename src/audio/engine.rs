//! 播放引擎。
//!
//! # 线程模型
//!
//! 音频设备独占一个名为 `kugou-audio` 的线程。原因是 Linux 上 cpal 的
//! `Stream`（`rodio::MixerDeviceSink` 持有它）不是 `Send`，无法跨线程传递，
//! 所以「创建设备 → 创建 Player → 消费命令」必须都发生在同一个线程里。
//!
//! ```text
//!        主线程                          kugou-audio 线程
//!   ┌──────────────┐   AudioCmd       ┌──────────────────────┐
//!   │ App / UI     │ ───────────────▶ │ MixerDeviceSink      │
//!   │              │  (crossbeam)     │ Player (rodio)       │
//!   │              │                  │                      │
//!   │              │ ◀─────────────── │ Event::Audio(..)     │
//!   └──────────────┘   EventBus       └──────────────────────┘
//!          ▲                                    │
//!          │ 读原子量（位置/时长/音量/状态）        │ 写原子量
//!          └────────────────────────────────────┘
//! ```
//!
//! # 为什么位置信息走原子量而不是 channel
//!
//! UI 每帧（默认 200ms）都要读一次播放位置。如果走 channel，音频线程要按
//! 「音频帧率」或至少「UI 帧率」投递消息，主循环的消息队列会被位置更新淹没，
//! 白白增加分配与调度开销。用 4 个原子量共享，读写都是几条指令，零分配。
//!
//! 只有**离散事件**（装载完成、播放结束、出错）才走 channel。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, unbounded};
use rodio::Source;

use crate::audio::levels::{AudioLevels, LevelMeter};
use crate::event::{Event, EventBus};
use crate::logger::tlog;

/// 音频线程的轮询间隔。
///
/// 200ms 让进度条视觉上连续，同时把空转开销压到可忽略：一次轮询只是读几个
/// 原子量加两次 `Mutex` 短锁，5 次/秒的代价约等于零。
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// 快进/快退步长（毫秒）。
pub const SEEK_STEP_MS: i64 = 5_000;

/// 音量调节步长。
pub const VOLUME_STEP: f32 = 0.05;

// ============================================================================
// 对外类型
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlaybackState {
    /// 没有装载任何音源。
    #[default]
    Stopped,
    /// 已发出播放请求，正在下载或解码。
    Loading,
    Playing,
    Paused,
}

impl PlaybackState {
    fn code(self) -> u8 {
        match self {
            Self::Stopped => 0,
            Self::Loading => 1,
            Self::Playing => 2,
            Self::Paused => 3,
        }
    }

    fn from_code(code: u8) -> Self {
        match code {
            1 => Self::Loading,
            2 => Self::Playing,
            3 => Self::Paused,
            _ => Self::Stopped,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Stopped => "已停止",
            Self::Loading => "缓冲中",
            Self::Playing => "播放中",
            Self::Paused => "已暂停",
        }
    }
}

/// 音频线程上报的离散事件。
#[derive(Debug)]
pub enum AudioEvent {
    /// 音源已装载，`duration_ms` 是解码器给出的真实时长（可能为 0）。
    Ready { duration_ms: u64 },
    /// 输出设备已打开（含切换到另一张卡）。
    ///
    /// `name` 是设备名，界面上直接显示它。这是诊断「播放中却没声音」的关键
    /// 信息：设备名对不上自己听的那张卡，一眼就能看出来，不用去翻系统配置。
    DeviceOpened { name: String },
    /// 当前曲目自然播放结束，主循环据此切下一首。
    TrackFinished,
    /// 边下边播的**流断了**：读数据超时（下载跟不上 / 网络抖了一下），
    /// 播放器因此空掉。
    ///
    /// **绝不能当成 [`Self::TrackFinished`]**——歌并没放完。混为一谈的后果很
    /// 具体：单曲循环（或队列里只有一首）下会"从头再放一遍"，用户看到的就是
    /// 「播放进度回到开头」。所以单独一个变体，并且带着断流时的位置，
    /// 让主线程能从这个位置续播，而不是从 0 重来。
    StreamInterrupted {
        /// 断流那一刻的播放位置（毫秒）。0 说明还没真正开始播。
        position_ms: u64,
    },
    /// 换输出设备没换成。**旧设备还活着、还在播**，所以它不是 [`Self::Failed`]：
    /// 只提示一句，不把播放状态改成「已停止」。
    DeviceSwitchFailed(String),
    /// 打开设备或解码失败。
    Failed(String),
}

/// 播放来源：缓存文件，或边下边播的流式缓冲。
#[derive(Debug)]
pub enum AudioSource {
    /// 缓存里已经下完的文件。可以随便 seek。
    File(PathBuf),
    /// 正在下载的缓冲。读指针跑在下载前面时会阻塞等数据。
    Stream(crate::audio::streaming::StreamingBuffer),
}

/// 主线程 → 音频线程的命令。
#[derive(Debug)]
enum AudioCmd {
    /// 装载一段音频。`start_at_ms` 用于「恢复上次播放位置」。
    ///
    /// 两种来源：缓存里已有的本地文件，或正在下载的流式缓冲（边下边播）。
    Load {
        source: AudioSource,
        start_at_ms: u64,
        /// 解码器报不出时长时的兜底，来自列表数据。
        expected_duration_ms: u64,
    },
    Toggle,
    Stop,
    SeekTo(u64),
    SeekBy(i64),
    SetVolume(f32),
    /// 换一张声卡输出。`None` 表示回到系统默认。
    UseDevice(Option<String>),
    Shutdown,
}

// ============================================================================
// 共享快照
// ============================================================================

/// 音频线程与主线程共享的只读快照。
#[derive(Debug)]
struct Shared {
    state: AtomicU8,
    position_ms: AtomicU64,
    duration_ms: AtomicU64,
    /// `f32` 的位模式。
    volume_bits: AtomicU32,
}

impl Shared {
    fn new(volume: f32) -> Self {
        Self {
            state: AtomicU8::new(PlaybackState::Stopped.code()),
            position_ms: AtomicU64::new(0),
            duration_ms: AtomicU64::new(0),
            volume_bits: AtomicU32::new(volume.clamp(0.0, 1.0).to_bits()),
        }
    }

    fn state(&self) -> PlaybackState {
        PlaybackState::from_code(self.state.load(Ordering::Relaxed))
    }

    fn set_state(&self, state: PlaybackState) {
        self.state.store(state.code(), Ordering::Relaxed);
    }

    fn position_ms(&self) -> u64 {
        self.position_ms.load(Ordering::Relaxed)
    }

    fn duration_ms(&self) -> u64 {
        self.duration_ms.load(Ordering::Relaxed)
    }

    fn volume(&self) -> f32 {
        f32::from_bits(self.volume_bits.load(Ordering::Relaxed))
    }

    fn set_volume(&self, volume: f32) {
        self.volume_bits
            .store(volume.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }
}

// ============================================================================
// 句柄
// ============================================================================

/// 播放引擎句柄。可克隆地发送命令，读取状态则直接走原子量。
pub struct AudioHandle {
    command_tx: Sender<AudioCmd>,
    shared: Arc<Shared>,
    /// 播放电平。音频线程写，主线程读。
    levels: AudioLevels,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for AudioHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AudioHandle")
            .field("state", &self.state())
            .field("position_ms", &self.position_ms())
            .field("duration_ms", &self.duration_ms())
            .field("volume", &self.volume())
            .finish()
    }
}

impl AudioHandle {
    /// 启动音频线程。
    ///
    /// 设备打开失败不会让进程退出：错误通过 [`AudioEvent::Failed`] 上报，
    /// 界面照常可用（用户可以继续浏览、搜索、管理歌单）。
    pub fn spawn(bus: EventBus, initial_volume: f32, device: Option<String>) -> Self {
        let (command_tx, command_rx) = unbounded();
        let shared = Arc::new(Shared::new(initial_volume));
        let thread_shared = Arc::clone(&shared);

        // 先建好再分身：句柄和音频线程必须是**同一个** AudioLevels，
        // 否则主线程读到的永远是零，柱子不会动。
        let levels = AudioLevels::new();
        let thread_levels = levels.clone();

        let thread = thread::Builder::new()
            .name("kugou-audio".to_string())
            .spawn(move || run(command_rx, bus, thread_shared, thread_levels, device))
            .map_err(|error| {
                tlog!(crate::logger::LEVEL_ERROR, "启动音频线程失败：{error}");
                error
            })
            .ok();

        Self {
            command_tx,
            shared,
            levels,
            thread,
        }
    }

    /// 音频线程是否连启动都没成功（`thread::Builder::spawn` 失败）。
    ///
    /// 注意它只能反映**线程创建**这一步。设备打不开、解码失败这类问题发生在线程
    /// 内部，会通过 [`AudioEvent::Failed`] 上报到界面，不走这里。
    pub fn spawn_failed(&self) -> bool {
        self.thread.is_none()
    }

    /// 当前播放电平（0.0 ~ 1.0 的一串格子，旧的在前）。
    pub fn levels(&self) -> Vec<f32> {
        self.levels.snapshot()
    }

    /// 当前这段声音的频谱（`bands` 个 0.0 ~ 1.0 的能量值）。
    ///
    /// FFT 在这里（主线程）算，不在音频线程：它是一次纯计算，放进音频线程
    /// 会拖住采样供给，表现出来就是爆音。
    pub fn spectrum(&self, bands: usize) -> Vec<f32> {
        self.levels.spectrum(bands)
    }

    /// 标记为「缓冲中」。UI 在发起下载前调用，让用户立刻看到反馈。
    pub fn mark_loading(&self) {
        self.shared.set_state(PlaybackState::Loading);
        self.shared.position_ms.store(0, Ordering::Relaxed);
    }

    /// 装载并播放一段音频（本地文件，或边下边播的流式缓冲）。
    pub fn load(&self, source: AudioSource, start_at_ms: u64, expected_duration_ms: u64) {
        self.shared
            .duration_ms
            .store(expected_duration_ms, Ordering::Relaxed);
        self.send(AudioCmd::Load {
            source,
            start_at_ms,
            expected_duration_ms,
        });
    }

    pub fn toggle(&self) {
        self.send(AudioCmd::Toggle);
    }

    pub fn stop(&self) {
        self.send(AudioCmd::Stop);
    }

    pub fn seek_to(&self, position_ms: u64) {
        self.send(AudioCmd::SeekTo(position_ms));
    }

    pub fn seek_by(&self, delta_ms: i64) {
        self.send(AudioCmd::SeekBy(delta_ms));
    }

    pub fn set_volume(&self, volume: f32) {
        let clamped = volume.clamp(0.0, 1.0);
        self.shared.set_volume(clamped);
        self.send(AudioCmd::SetVolume(clamped));
    }

    /// 换一个输出设备，`None` 表示回到系统默认。
    ///
    /// 切换是在音频线程里重建设备，因此当前这首会停（解码器已经被消费掉了，
    /// 无法原地续播）。主线程收到 [`AudioEvent::DeviceOpened`] 后会把刚才那首
    /// 按原位置重新装载，用户侧看不出中断。
    pub fn use_device(&self, device: Option<String>) {
        self.send(AudioCmd::UseDevice(device));
    }

    pub fn state(&self) -> PlaybackState {
        self.shared.state()
    }

    pub fn position_ms(&self) -> u64 {
        self.shared.position_ms()
    }

    pub fn duration_ms(&self) -> u64 {
        self.shared.duration_ms()
    }

    pub fn volume(&self) -> f32 {
        self.shared.volume()
    }

    /// 停止播放并回收音频线程。
    ///
    /// 对 join 是**有界等待**：音频线程正常情况下一个轮询周期（200ms）内就会
    /// 退出，但万一卡在驱动层面的阻塞操作上（`snd_pcm_*` 系列不受我们控制），
    /// 无限等 join 会把整个进程拖在「半死」状态——进程活着，设备就一直被占，
    /// 其它应用全部打不开声音。等不到就放弃：进程照常退出，内核回收音频线程
    /// 持有的全部 fd，设备立即释放。
    pub fn shutdown(&mut self) {
        // 线程已经退出时 send 会失败，这是预期情况，忽略即可
        let _ = self.command_tx.send(AudioCmd::Shutdown);
        if let Some(thread) = self.thread.take() {
            const JOIN_TIMEOUT: Duration = Duration::from_secs(3);
            let deadline = std::time::Instant::now() + JOIN_TIMEOUT;
            while !thread.is_finished() && std::time::Instant::now() < deadline {
                thread::sleep(Duration::from_millis(20));
            }
            if thread.is_finished() {
                let _ = thread.join();
            } else {
                tlog!(
                    crate::logger::LEVEL_WARN,
                    "音频线程 {JOIN_TIMEOUT:?} 内未退出，放弃等待（进程退出时由系统回收设备）"
                );
            }
        }
    }

    fn send(&self, command: AudioCmd) {
        if self.command_tx.send(command).is_err() {
            tlog!(crate::logger::LEVEL_WARN, "音频线程已退出，命令被丢弃");
        }
    }
}

impl Drop for AudioHandle {
    fn drop(&mut self) {
        if self.thread.is_some() {
            self.shutdown();
        }
    }
}

// ============================================================================
// 音频线程
// ============================================================================

/// 「系统默认设备」的显示名。
pub const DEFAULT_DEVICE_LABEL: &str = "系统默认";

/// 已打开的输出设备：设备本身 + 挂在它 mixer 上的播放器。
///
/// 字段顺序就是析构顺序：**播放器必须排在设备前面**。它挂在设备的 mixer 上，
/// 先关设备会让播放器留在已经释放的 mixer 上。
struct Output {
    player: rodio::Player,
    /// 设备本体。**没有任何代码读它**——它存在的唯一意义是活到播放结束：一旦
    /// 析构，声音立刻断。Rust 的 dead_code 看不出「靠生命周期起作用」，所以
    /// 用下划线前缀避开警告。别把它当没用的字段删掉。
    _stream: rodio::MixerDeviceSink,
    /// 设备名。上报给界面显示——「播着却没声音」时，用户看一眼就知道声音去了哪。
    name: String,
}

impl Output {
    /// 打开输出设备。`requested` 为 `None` 时用系统默认。
    fn open(requested: Option<&str>) -> Result<Self, String> {
        // 设备必须在音频线程里创建：cpal 的 Stream 不是 Send
        if let Some(name) = requested {
            match find_device(name) {
                Ok(device) => {
                    let label = device_name(&device).unwrap_or_else(|| name.to_string());
                    match Self::from_device(device, label) {
                        Ok(output) => return Ok(output),
                        Err(error) => tlog!(
                            crate::logger::LEVEL_WARN,
                            "配置的输出设备「{name}」打不开：{error}，改用系统默认设备"
                        ),
                    }
                }
                // 指定了却找不到（声卡被拔、ALSA 卡号变了）时不让播放死掉：
                // 退回系统默认。界面显示的是**实际打开**的那张卡。
                Err(error) => {
                    tlog!(crate::logger::LEVEL_WARN, "{error}，改用系统默认设备");
                }
            }
        }
        Self::open_default()
    }

    /// 系统默认设备。
    ///
    /// **有声音服务器（PipeWire/PulseAudio）时只开 PCM `default`**——它由
    /// `/usr/share/alsa/alsa.conf.d/99-*-default.conf` 重定向进服务器，和其它
    /// 应用共享声卡。**绝不借用 rodio 的 `open_default_sink` 兜底**：那玩意儿
    /// 在 `default` 打不开时会遍历设备列表、抓第一个能开的——在服务器系统上
    /// 就是直连硬件（独占声卡，挤死所有人）。打不开就报错，让用户看到原因。
    ///
    /// 没有声音服务器（headless 的裸 ALSA 系统）时维持 rodio 原行为：那种
    /// 环境里 `default` 打不开就该试别的设备，直连也没有「挤死别人」的问题。
    fn open_default() -> Result<Self, String> {
        if sound_server_present() {
            return Self::open_server_default();
        }
        Self::open_bare_alsa_default()
    }

    /// 有声音服务器时的默认设备：PCM `default`，经服务器路由。
    fn open_server_default() -> Result<Self, String> {
        use rodio::cpal::traits::HostTrait;

        // cpal 在 ALSA host 上返回的就是 PCM `default`（名字被硬编码成
        // "Default Audio Device"，显示名用 `default_label()` 另取）。
        let device = rodio::cpal::default_host()
            .default_output_device()
            .ok_or_else(|| "找不到系统默认音频设备".to_string())?;
        Self::from_device(device, default_label())
    }

    /// 没有声音服务器时的默认设备：交给 rodio 处理（含设备列表回退）。
    ///
    /// 它先试 cpal 认的默认设备（ALSA 的 `default`），失败才按设备列表回退、
    /// 并跳过 `null`。
    fn open_bare_alsa_default() -> Result<Self, String> {
        let mut stream = rodio::DeviceSinkBuilder::open_default_sink().map_err(open_failed)?;

        // rodio 默认会在 DeviceSink 析构时往 stdout 打一行 "Dropping DeviceSink..."，
        // 那行字会直接糊在 TUI 界面上，必须关掉。
        stream.log_on_drop(false);

        let name = default_label();
        let player = rodio::Player::connect_new(stream.mixer());
        Ok(Self {
            player,
            _stream: stream,
            name,
        })
    }

    fn from_device(device: rodio::cpal::Device, name: String) -> Result<Self, String> {
        let builder = rodio::DeviceSinkBuilder::from_device(device).map_err(open_failed)?;
        // `open_sink_or_fallback`：按设备支持的格式逐个试，比 `open_stream` 宽容
        // （实测有设备用默认格式打不开、换一种格式就行）。
        let mut stream = builder.open_sink_or_fallback().map_err(open_failed)?;

        // rodio 默认会在 DeviceSink 析构时往 stdout 打一行 "Dropping DeviceSink..."，
        // 那行字会直接糊在 TUI 界面上，必须关掉。
        stream.log_on_drop(false);

        let player = rodio::Player::connect_new(stream.mixer());
        Ok(Self {
            player,
            _stream: stream,
            name,
        })
    }
}

fn open_failed(error: rodio::DeviceSinkError) -> String {
    format!("无法打开音频输出设备：{error}。请确认系统音频服务正常（Linux 下检查 PipeWire/ALSA）。")
}

/// 可用的输出设备：能打开、能出声、名字不重复。
///
/// ALSA 会把自己定义的**所有 PCM** 都报成设备——实测这台机器上有 52 项，其中
/// 大多数是插件（`lavrate` / `samplerate` / `jack` / `oss` / `speexrate`…）。
/// 把它们摆进设置页，用户选中一个打不开的就会把播放弄哑。三道过滤：
///
/// * **能给出默认输出配置**——这是「真的能播」的判据，插件和已被独占的设备都过不了；
/// * **排除 `null`**（"Discard all samples"）。它能打开、能「正常播放」，只是把所有
///   采样丢掉，从外面看毫无异常——正是「播放中却没声音」的另一种成因。它偏偏还能
///   给出配置，所以必须单独判掉；
/// * **有声音服务器时只留服务器路由的 PCM**（见 [`sound_server_present`]）。
///
/// 最后按名字去重：同一张卡会以 `hw:` / `plughw:` / `front:` / `surround*:` 等
/// 十几种形态出现，全列出来只会让人没法选。
fn output_devices() -> Vec<rodio::cpal::Device> {
    use rodio::cpal::traits::{DeviceTrait, HostTrait};

    let devices = match rodio::cpal::default_host().output_devices() {
        Ok(devices) => devices,
        Err(error) => {
            tlog!(crate::logger::LEVEL_WARN, "枚举音频输出设备失败：{error}");
            return Vec::new();
        }
    };

    let mut seen = std::collections::HashSet::new();
    devices
        .filter(|device| !is_null_device(device))
        // 直连硬件的 PCM（hw: / plughw: / front: / sysdefault …）是独占语义：
        // 拿走一个，声音服务器（以及它代理的麦克风、浏览器、通话……）就再也
        // 打不开那张卡。有服务器在场时全部剔除，只留共享的服务器入口。
        // **这是真实事故**：设置页里「Default Audio Device」这个名字看着无害，
        // 实际是 `sysdefault`——`plughw:0` 的别名（见
        // /usr/share/alsa/pcm/default.conf），选中后程序直连 USB 声卡，
        // 把麦克风和扬声器一起挤哑，PipeWire 日志里全是
        // "playback open failed: 设备或资源忙"。
        //
        // 没有声音服务器（headless 裸 ALSA）时不滤——那种环境直连没有
        // 「挤死别人」的问题，维持原有行为。非 Linux 平台拿不到 PCM 名，
        // `sound_server_present` 恒为 false，同样不受影响。
        .filter(|device| {
            !sound_server_present() || driver_of(device).is_some_and(|driver| is_server_routed(&driver))
        })
        .filter(|device| device.default_output_config().is_ok())
        .filter(|device| seen.insert(device_name(device).unwrap_or_default()))
        .collect()
}

/// 声音服务器（PipeWire / PulseAudio）是否在场。
///
/// 判据：服务器会往 ALSA 里注册自己的 PCM 插件（`pipewire` / `pulse`），
/// 枚举里出现任意一个就算在场。只看 PCM 名、不查询配置，开销可忽略。
fn sound_server_present() -> bool {
    use rodio::cpal::traits::HostTrait;

    rodio::cpal::default_host()
        .output_devices()
        .map(|devices| {
            devices
                .into_iter()
                .any(|device| driver_of(&device).is_some_and(|driver| is_sound_server_pcm(&driver)))
        })
        .unwrap_or(false)
}

/// PCM 是否由声音服务器自己提供（服务器的 ALSA 插件）。
fn is_sound_server_pcm(driver: &str) -> bool {
    matches!(driver, "pipewire" | "pulse")
}

/// PCM 是否经声音服务器路由（共享使用硬件，不会独占）。
///
/// `default` 在装有 pipewire-alsa / pulseaudio-alsa 的系统上被重定向进
/// 服务器；`pipewire` / `pulse` 是服务器入口本身。三者都安全。
fn is_server_routed(driver: &str) -> bool {
    matches!(driver, "default" | "pipewire" | "pulse")
}

/// 是不是那个「丢弃所有采样」的空设备。
///
/// 判据是 ALSA 的 PCM 名（cpal 的 `driver()` 就是它），不是显示名——
/// 显示名会跟着本地化/配置变。
fn is_null_device(device: &rodio::cpal::Device) -> bool {
    driver_of(device).is_some_and(|driver| driver == "null")
}

fn driver_of(device: &rodio::cpal::Device) -> Option<String> {
    use rodio::cpal::traits::DeviceTrait;

    device
        .description()
        .ok()
        .and_then(|description| description.driver().map(str::to_string))
}

/// 系统默认设备的显示名。
///
/// 取枚举里 ALSA PCM 名为 `default` 的那一项：它的名字形如
/// 「Default ALSA Output (currently PipeWire Media Server)」，一眼能看出声音
/// 实际交给了谁。**不要**用 `default_output_device().description()`——cpal 对
/// 它的名字是硬编码的 "Default Audio Device"，看不出路由到了哪里。
fn default_label() -> String {
    output_devices()
        .iter()
        .find(|device| driver_of(device).is_some_and(|driver| driver == "default"))
        .and_then(device_name)
        .unwrap_or_else(|| DEFAULT_DEVICE_LABEL.to_string())
}

/// 按名字找输出设备。找不到时把可用设备列出来，省得用户去猜名字。
fn find_device(name: &str) -> Result<rodio::cpal::Device, String> {
    output_devices()
        .into_iter()
        .find(|device| device_name(device).is_some_and(|candidate| candidate == name))
        .ok_or_else(|| {
            format!(
                "找不到音频输出设备「{name}」，当前可用：{}",
                list_output_devices().join("、")
            )
        })
}

/// 一个设备叫什么。`description()` 里的名字才是给人看的（cpal 0.17 起
/// `DeviceTrait::name` 已废弃），取不到就当它没名字。
fn device_name(device: &rodio::cpal::Device) -> Option<String> {
    use rodio::cpal::traits::DeviceTrait;

    device
        .description()
        .ok()
        .map(|description| description.name().to_string())
}

/// 系统里可用的输出设备名。主线程枚举一次，供设置页选择。
///
/// 只取名字不取设备：`cpal::Device` 未必能安全跨线程搬运，而设置页要的只是
/// 一串可显示、可回传的名字。
pub fn list_output_devices() -> Vec<String> {
    output_devices().iter().filter_map(device_name).collect()
}

fn run(
    rx: Receiver<AudioCmd>,
    bus: EventBus,
    shared: Arc<Shared>,
    levels: AudioLevels,
    device: Option<String>,
) {
    let mut runtime = match Output::open(device.as_deref()) {
        Ok(output) => {
            bus.send(Event::Audio(AudioEvent::DeviceOpened {
                name: output.name.clone(),
            }));
            Runtime {
                output: Some(output),
                shared,
                levels,
                bus,
                stream: None,
                last_position_ms: 0,
                loaded: false,
                finished_reported: true,
            }
        }
        Err(message) => {
            tlog!(crate::logger::LEVEL_ERROR, "{message}");
            shared.set_state(PlaybackState::Stopped);
            bus.send(Event::Audio(AudioEvent::Failed(message)));
            // 设备不可用时仍要消费命令，否则发送端会积压
            drain_until_shutdown(rx);
            return;
        }
    };

    let volume = runtime.shared.volume();
    if let Some(output) = runtime.output.as_mut() {
        output.player.set_volume(volume);
    }

    runtime.run_loop(rx);

    // `output`（含设备）在这里才 drop —— 必须活到播放结束，否则声音会立刻中断。
    // 字段顺序保证播放器先于设备析构。
    drop(runtime);
}

/// 设备不可用时，把命令读干净直到收到 Shutdown，避免通道无界增长。
fn drain_until_shutdown(rx: Receiver<AudioCmd>) {
    for command in rx.iter() {
        if matches!(command, AudioCmd::Shutdown) {
            break;
        }
    }
}

struct Runtime {
    /// 当前输出设备。`None` 表示设备不可用（启动失败，或切换到的那张卡打不开）。
    output: Option<Output>,
    shared: Arc<Shared>,
    /// 电平采集。包在解码器外面，采样透传的同时记下峰值。
    levels: AudioLevels,
    bus: EventBus,
    /// 当前音源如果是边下边播的流，这里留一份句柄。
    ///
    /// 播放器"变空"时要用它判断到底是**放完了**还是**数据断了**：
    /// 前者该切歌，后者该留住位置续播（见 [`Runtime::sync`]）。
    stream: Option<crate::audio::streaming::StreamingBuffer>,
    /// 上一次**还在正常播放**时读到的位置（毫秒）。
    ///
    /// 曲目结束的那一帧不能读 `Player::get_pos()`：源都没了，rodio 报的是 0。
    /// 那个 0 一旦写进共享快照，断流时就"没有位置可续"——用户看到进度条突然
    /// 跳回 00:00，按播放也只能从头听。所以位置只在这一帧之外更新。
    last_position_ms: u64,
    /// 是否已经装载了音源。
    loaded: bool,
    /// 本曲是否已上报过结束，防止同一首歌反复触发切歌。
    finished_reported: bool,
}

impl Runtime {
    fn run_loop(&mut self, rx: Receiver<AudioCmd>) {
        loop {
            match rx.recv_timeout(POLL_INTERVAL) {
                Ok(AudioCmd::Shutdown) => break,
                Ok(command) => self.handle(command),
                // 没有命令时也走一遍同步，让进度条持续前进
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            self.sync();
        }

        if let Some(output) = self.output.as_mut() {
            output.player.stop();
        }
        self.shared.set_state(PlaybackState::Stopped);
    }

    fn handle(&mut self, command: AudioCmd) {
        match command {
            AudioCmd::Load {
                source,
                start_at_ms,
                expected_duration_ms,
            } => self.load(source, start_at_ms, expected_duration_ms),
            AudioCmd::Toggle => match self.output.as_ref() {
                Some(output) if output.player.is_paused() => self.resume(),
                _ => self.pause(),
            },
            AudioCmd::Stop => self.stop(),
            AudioCmd::SeekTo(position_ms) => self.seek_to(position_ms),
            AudioCmd::SeekBy(delta_ms) => {
                let current = self.shared.position_ms() as i64;
                let target = current.saturating_add(delta_ms).max(0) as u64;
                self.seek_to(target);
            }
            AudioCmd::SetVolume(volume) => {
                if let Some(output) = self.output.as_mut() {
                    output.player.set_volume(volume);
                }
                self.shared.set_volume(volume);
            }
            AudioCmd::UseDevice(device) => self.use_device(device),
            // 在 run_loop 里已处理
            AudioCmd::Shutdown => {}
        }
    }

    /// 换一张声卡输出。
    ///
    /// 旧设备连同挂在它上面的播放器一起丢弃（字段顺序保证播放器先析构），
    /// 然后重新打开。当前这首会停：解码器已经被消费掉了，没法原地续播，
    /// 由主线程收到 [`AudioEvent::DeviceOpened`] 后按原位置重新装载。
    fn use_device(&mut self, device: Option<String>) {
        // 新设备**先打开再顶替**：打不开就原样留着旧的，别把正在放的声音弄没了。
        // （先丢旧设备会连播放器一起没，一旦新设备也打不开，播放就彻底不可用了。）
        match Output::open(device.as_deref()) {
            Ok(output) => {
                let name = output.name.clone();
                self.bus
                    .send(Event::Audio(AudioEvent::DeviceOpened { name }));
                self.stream = None;
                self.loaded = false;
                self.finished_reported = true;
                self.levels.clear();
                self.output = Some(output);
            }
            Err(message) => {
                tlog!(crate::logger::LEVEL_ERROR, "{message}");
                self.bus
                    .send(Event::Audio(AudioEvent::DeviceSwitchFailed(message)));
            }
        }
    }

    fn load(&mut self, source: AudioSource, start_at_ms: u64, expected_duration_ms: u64) {
        // 先记住这条源是不是流式：它被 move 进解码器之后就问不出来了，
        // 而"播放器空了"时正是靠它判断该切歌还是该续播。
        //
        // 这一步要放在可能提前 return 的路径**之前**：设备不可用时函数会直接返回，
        // 那也该把上一首的句柄放掉，别攥着它不放。
        self.stream = match &source {
            AudioSource::Stream(buffer) => Some(buffer.clone()),
            AudioSource::File(_) => None,
        };
        self.last_position_ms = start_at_ms;

        // 解码器先建好：它跟设备无关，且失败时不用去动设备借用
        let decoder = match build_decoder(source) {
            Ok(decoder) => decoder,
            Err(message) => return self.report_failure(message),
        };

        let Some(output) = self.output.as_mut() else {
            return;
        };

        output.player.stop();
        output.player.clear();
        self.loaded = false;
        // 装载期间先屏蔽「结束」上报，避免旧的 empty 状态误触发切歌
        self.finished_reported = true;

        // 解码器报出的时长最准；拿不到就用列表里的时长兜底，保证进度条可用
        let duration_ms = decoder
            .total_duration()
            .map(|duration| duration.as_millis() as u64)
            .filter(|millis| *millis > 0)
            .unwrap_or(expected_duration_ms);

        self.shared
            .duration_ms
            .store(duration_ms, Ordering::Relaxed);
        self.shared
            .position_ms
            .store(start_at_ms, Ordering::Relaxed);

        // 包一层电平采集：采样原样透传给播放器，顺带把峰值记进环形缓冲
        output
            .player
            .append(LevelMeter::new(decoder, self.levels.clone()));
        output.player.play();

        if start_at_ms > 0 {
            if let Err(error) = output.player.try_seek(Duration::from_millis(start_at_ms)) {
                tlog!(
                    crate::logger::LEVEL_WARN,
                    "跳转到 {start_at_ms}ms 失败：{error}"
                );
            }
        }

        self.loaded = true;
        self.finished_reported = false;
        self.shared.set_state(PlaybackState::Playing);
        self.bus
            .send(Event::Audio(AudioEvent::Ready { duration_ms }));
    }

    fn pause(&mut self) {
        if !self.loaded {
            return;
        }
        if let Some(output) = self.output.as_mut() {
            output.player.pause();
        }
        self.shared.set_state(PlaybackState::Paused);
    }

    fn resume(&mut self) {
        if !self.loaded {
            return;
        }
        if let Some(output) = self.output.as_mut() {
            output.player.play();
        }
        self.shared.set_state(PlaybackState::Playing);
        // 恢复播放后允许再次上报结束
        self.finished_reported = false;
    }

    fn stop(&mut self) {
        if let Some(output) = self.output.as_mut() {
            output.player.stop();
            output.player.clear();
        }
        // 清掉残留的柱子，否则会定格在最后一帧，看着像卡住了
        self.levels.clear();
        self.stream = None;
        self.last_position_ms = 0;
        self.loaded = false;
        self.finished_reported = true;
        self.shared.position_ms.store(0, Ordering::Relaxed);
        self.shared.duration_ms.store(0, Ordering::Relaxed);
        self.shared.set_state(PlaybackState::Stopped);
    }

    fn seek_to(&mut self, position_ms: u64) {
        if !self.loaded {
            return;
        }
        let Some(output) = self.output.as_mut() else {
            return;
        };

        let duration_ms = self.shared.duration_ms();
        let target = if duration_ms > 0 {
            position_ms.min(duration_ms.saturating_sub(1))
        } else {
            position_ms
        };

        match output.player.try_seek(Duration::from_millis(target)) {
            Ok(()) => {
                self.last_position_ms = target;
                self.shared.position_ms.store(target, Ordering::Relaxed);
                // 跳转后重新允许上报结束
                self.finished_reported = false;
            }
            Err(error) => tlog!(crate::logger::LEVEL_WARN, "跳转到 {target}ms 失败：{error}"),
        }
    }

    /// 把播放器的真实状态同步到共享快照，并检测曲目结束。
    fn sync(&mut self) {
        if !self.loaded {
            return;
        }
        let Some(output) = self.output.as_mut() else {
            return;
        };

        let paused = output.player.is_paused();
        let drained = output.player.empty();

        if drained && !paused {
            if !self.finished_reported {
                self.finished_reported = true;
                self.loaded = false;
                self.shared.set_state(PlaybackState::Stopped);

                // 位置用**上一帧还在播时**记下的那个，绝不在这里读 `get_pos()`：
                // 源已经结束，rodio 报 0，写进快照就等于把用户听到的位置抹掉。
                // 断流后要"从断点续播"全靠它。
                let position_ms = self.last_position_ms;

                // 「播放器空了」不等于「这首放完了」。
                //
                // rodio 的解码器把**任何**读错误都吞成 EOF（symphonia 的
                // `format.next_packet().ok()?`），所以流断了、下载失败了，
                // 表现出来和自然播完一模一样。照单全收的后果很具体：
                // 单曲循环（或队列里就一首）会"从头再放一遍"——用户看到的就是
                // 「播放进度回到开头」；顺序播放则会把这首没听完的歌跳过。
                let outcome = classify_drain(self.stream.as_ref());
                self.stream = None;
                match outcome {
                    // 用户切歌 / 停止，这是我们自己取消的，安静收场
                    DrainOutcome::Silent => {}
                    DrainOutcome::Failed(message) => {
                        self.bus.send(Event::Audio(AudioEvent::Failed(message)))
                    }
                    DrainOutcome::Interrupted => self
                        .bus
                        .send(Event::Audio(AudioEvent::StreamInterrupted { position_ms })),
                    DrainOutcome::Finished => {
                        self.bus.send(Event::Audio(AudioEvent::TrackFinished))
                    }
                }
            }
            return;
        }

        let position_ms = output.player.get_pos().as_millis() as u64;
        self.last_position_ms = position_ms;
        self.shared
            .position_ms
            .store(position_ms, Ordering::Relaxed);

        let state = if paused {
            PlaybackState::Paused
        } else {
            PlaybackState::Playing
        };
        if self.shared.state() != state {
            self.shared.set_state(state);
        }
    }

    /// 上报失败。
    ///
    /// 刻意**不**触发「播放结束」流程：否则一首坏文件会连锁触发自动切歌，
    /// 坏歌连成片时会瞬间刷掉整个队列。让用户看到错误后自己决定下一步。
    fn report_failure(&mut self, message: String) {
        tlog!(crate::logger::LEVEL_ERROR, "{message}");
        self.stream = None;
        self.loaded = false;
        self.finished_reported = true;
        self.shared.set_state(PlaybackState::Stopped);
        self.bus.send(Event::Audio(AudioEvent::Failed(message)));
    }
}

/// 播放器空掉之后的处置方式。
///
/// 抽成「枚举 + 自由函数」是为了**能直接测**：这条分支出错的表现是「网络抖一下
/// 歌就从头再放」或者「坏文件把整条队列刷掉」，两者都不会让编译失败，只会在
/// 用户那儿变成一句「怎么又回到开头了」。
#[derive(Debug, Clone, PartialEq, Eq)]
enum DrainOutcome {
    /// 不吭声：这条流是我们自己取消的（用户切歌 / 停止）。
    Silent,
    /// 流没能放完，且下载**失败**了——报错，但不切歌、不重播。
    Failed(String),
    /// 流只是断了（数据没跟上），歌没放完——留住位置续播。
    Interrupted,
    /// 真的放完了：本地文件，或者流已经完整下完。
    Finished,
}

/// 播放器空掉之后该上报什么。
///
/// 判据全在流自己身上（取消 / 失败 / 完整），不看"播放器空了"这个现象——
/// 因为 rodio 把读错误和正常结束都表现为"源结束"。
fn classify_drain(stream: Option<&crate::audio::streaming::StreamingBuffer>) -> DrainOutcome {
    match stream {
        Some(stream) if stream.is_cancelled() => DrainOutcome::Silent,
        Some(stream) if !stream.is_complete() => match stream.error() {
            Some(message) => DrainOutcome::Failed(message),
            None => DrainOutcome::Interrupted,
        },
        _ => DrainOutcome::Finished,
    }
}

/// 建解码器。与输出设备无关，单独抽出来是为了让 `load` 的错误分支不必持着
/// 设备借用（否则 `report_failure(&mut self)` 会和它冲突）。
///
/// rodio::Decoder 要的是 `Read + Seek`，文件和流式缓冲都满足，差别只在「读不到
/// 时是 EOF 还是阻塞等下载」。但 `Decoder<File>` 和 `Decoder<StreamingBuffer>`
/// 是两个不同类型，没法放进同一个变量——统一装箱成 `Box<dyn Source>`（rodio 为
/// `Box<dyn Source>` 实现了 Source，可以照样 append 给播放器）。
fn build_decoder(source: AudioSource) -> Result<Box<dyn Source<Item = f32> + Send>, String> {
    match source {
        AudioSource::File(path) => {
            let file = std::fs::File::open(&path)
                .map_err(|error| format!("打开音频文件 {} 失败：{error}", path.display()))?;
            rodio::Decoder::try_from(file)
                .map(|decoder| Box::new(decoder) as Box<dyn Source<Item = f32> + Send>)
                .map_err(|error| {
                    format!(
                        "解码 {} 失败：{error}。该文件可能不是有效音频，或格式不受支持。",
                        path.display()
                    )
                })
        }
        AudioSource::Stream(buffer) => rodio::Decoder::new(buffer)
            .map(|decoder| Box::new(decoder) as Box<dyn Source<Item = f32> + Send>)
            .map_err(|error| {
                format!("解码流失败：{error}。数据可能不是有效音频，或格式不受支持。")
            }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::streaming::StreamingBuffer;

    #[test]
    fn playback_state_code_round_trips() {
        for state in [
            PlaybackState::Stopped,
            PlaybackState::Loading,
            PlaybackState::Playing,
            PlaybackState::Paused,
        ] {
            assert_eq!(PlaybackState::from_code(state.code()), state);
        }
    }

    #[test]
    fn unknown_code_falls_back_to_stopped() {
        assert_eq!(PlaybackState::from_code(200), PlaybackState::Stopped);
    }

    #[test]
    fn shared_clamps_volume() {
        let shared = Shared::new(2.0);
        assert!((shared.volume() - 1.0).abs() < f32::EPSILON);

        shared.set_volume(-1.0);
        assert!(shared.volume().abs() < f32::EPSILON);
    }

    /// 本地文件放完 = 真的放完了，照常切歌。
    #[test]
    fn file_source_drain_is_a_finished_track() {
        assert_eq!(classify_drain(None), DrainOutcome::Finished);
    }

    /// 流**完整**下完再空掉 = 真的放完了。
    #[test]
    fn completed_stream_drain_is_a_finished_track() {
        let stream = StreamingBuffer::new(Some(4));
        stream.push(b"abcd");
        stream.finish(None);
        assert_eq!(classify_drain(Some(&stream)), DrainOutcome::Finished);
    }

    /// 流还在下、播放器却空了 —— 这是**断流**，不是放完。
    ///
    /// 这条是「进度回到开头」那个 bug 的核心：把它当成 `Finished`，主线程就会
    /// 走切歌流程，单曲循环下等于从头再放一遍。
    #[test]
    fn starving_stream_drain_is_an_interruption() {
        let stream = StreamingBuffer::new(None);
        stream.push(b"abcd");
        // 只下了 4 字节，既没 finish 也没取消
        assert_eq!(classify_drain(Some(&stream)), DrainOutcome::Interrupted);
    }

    /// 下载失败：如实报错，不当成播完（否则坏文件会连锁切歌）。
    #[test]
    fn failed_stream_drain_reports_the_reason() {
        let stream = StreamingBuffer::new(None);
        stream.finish(Some("连接被重置".to_string()));
        assert_eq!(
            classify_drain(Some(&stream)),
            DrainOutcome::Failed("连接被重置".to_string())
        );
    }

    /// 用户切歌导致的空掉要**安静**：不弹错误、也不切下一首。
    #[test]
    fn cancelled_stream_drain_is_silent() {
        let stream = StreamingBuffer::new(None);
        stream.push(b"abcd");
        stream.cancel();
        assert_eq!(classify_drain(Some(&stream)), DrainOutcome::Silent);
    }

    /// 取消优先于失败：网络断和用户切歌可能同时发生，那时候该安静收场
    /// ——用户已经换歌了，再弹一条"下载失败"只会让人以为新歌出了问题。
    #[test]
    fn cancellation_takes_precedence_over_error() {
        let stream = StreamingBuffer::new(None);
        stream.cancel();
        assert_eq!(classify_drain(Some(&stream)), DrainOutcome::Silent);
    }

    /// 服务器 PCM 的判定：`pipewire` / `pulse` 是服务器自己，`default` 只是
    /// 被重定向进服务器的入口——提供方不是服务器，但路由进去。
    #[test]
    fn sound_server_pcm_detection() {
        assert!(is_sound_server_pcm("pipewire"));
        assert!(is_sound_server_pcm("pulse"));
        // default / sysdefault / 直连硬件都不是服务器本体
        for driver in ["default", "sysdefault", "hw:CARD=Device,DEV=0", "null"] {
            assert!(!is_sound_server_pcm(driver), "{driver} 不该被认成服务器本体");
        }
    }

    /// 「经服务器路由」的判定：有服务器时**只有**这三个 PCM 允许出现在
    /// 可选列表里。`sysdefault` 是这条规则存在的理由——它叫
    /// "Default Audio Device"，实际是 `plughw:0` 的直连别名，选中即独占声卡。
    #[test]
    fn server_routed_devices_are_safe_to_share() {
        for driver in ["default", "pipewire", "pulse"] {
            assert!(is_server_routed(driver), "{driver} 应当被视为可共享");
        }
        // 直连硬件的（含那条骗人的 sysdefault）全部拒绝
        for driver in [
            "sysdefault",
            "sysdefault:CARD=Device,DEV=0",
            "hw:CARD=Device,DEV=0",
            "plughw:CARD=0,DEV=0",
            "front:CARD=Device,DEV=0",
            "surround51:CARD=Device,DEV=0",
            "iec958:CARD=Device,DEV=0",
            "dmix",
            "null",
        ] {
            assert!(!is_server_routed(driver), "{driver} 直连硬件，必须被过滤");
        }
    }
}
