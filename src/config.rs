//! 配置持久化。
//!
//! 落盘位置（两套都是各自平台的正统位置，靠 `dirs` 抹平，这里不写平台分支）：
//!
//! | 用途 | Linux / macOS | Windows |
//! |------|---------------|---------|
//! | 配置 | `$XDG_CONFIG_HOME/kugou-tui/config.toml`（即 `~/.config/…`） | `%APPDATA%\kugou-tui\config.toml` |
//! | 缓存 / 日志 | `$XDG_CACHE_HOME/kugou-tui/`（即 `~/.cache/…`） | `%LOCALAPPDATA%\kugou-tui\` |
//!
//! 缓存与配置分开放，这样备份配置时不会把几百 MB 的音频缓存一起带走。
//! 需要整体挪走（便携安装、把配置放 U 盘）时用 `KUGOU_TUI_CONFIG_DIR` 覆盖配置根目录，
//! 缓存目录仍可用 `--cache-dir` 单独指定。
//!
//! 优先级：命令行参数 > 环境变量 > 配置文件 > 内置默认值。
//! 环境变量由 clap 的 `env` 属性直接读入 [`Cli`]，因此这里只需实现后两级。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::app::queue::PlaybackMode;
use crate::cli::Cli;
use crate::error::{AppError, Result};
use crate::logger::tlog;
use crate::source::{SourceKind, SourceSet};
use crate::ui::theme::ThemeName;

/// KuGouMusicApi 的默认监听地址。
pub const DEFAULT_API_BASE: &str = "http://127.0.0.1:3000";

const DEFAULT_VOLUME: f32 = 0.7;
const DEFAULT_PAGE_SIZE: u32 = 30;
const DEFAULT_TICK_MS: u64 = 200;
const DEFAULT_CACHE_LIMIT_MIB: u64 = 512;
const DEFAULT_QUALITY: &str = "128";
const APP_DIR_NAME: &str = "kugou-tui";

/// WebSocket 服务的默认端口。
///
/// 与 MoeKoeMusic 一致（其文档写 `ws://127.0.0.1:6520/`），这样面向它的第三方
/// 客户端不用改配置就能连上。
pub const DEFAULT_WS_PORT: u16 = 6520;

/// `/song/url` 支持的音质取值。
///
/// 前五个是常规音质；`viper_*` 是酷狗的「蝰蛇音效」系列，仅部分歌曲支持，
/// 拿不到时服务端会返回空 url，客户端的错误提示会说明原因。
pub const SUPPORTED_QUALITIES: &[&str] = &[
    "128",
    "320",
    "flac",
    "high",
    "super",
    "viper_clear",
    "viper_atmos",
    "viper_tape",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// KuGouMusicApi 服务地址，例如 `http://127.0.0.1:3000`。
    pub api_base: String,

    /// 登录态 cookie，形如 `token=xxx; userid=xxx; dfid=xxx`。
    ///
    /// 搜索接口缺少认证信息会返回 `error_code: 152`，云歌单同步也依赖它。
    pub cookie: Option<String>,

    /// 设备指纹。为空时由 `/register/dev` 自动获取并回写。
    pub dfid: Option<String>,

    /// 音量，范围 `0.0 ~ 1.0`。
    pub volume: f32,

    /// 音频输出设备名（cpal 枚举到的名字）。`None` 表示跟随系统默认。
    ///
    /// 为什么需要显式指定：Linux 上 cpal 打开的是 ALSA 的 `default`，而 `default`
    /// 可能被 `/etc/asound.conf` 写死成某一张声卡。实测就踩过：写死成板载卡，
    /// 而用户在听 USB 声卡 —— 表现是「进度在走、完全没声音」，而且因为是直连
    /// 硬件（绕过了 PipeWire），`pactl list sink-inputs` 里连这个程序都看不到。
    /// 能在界面上改设备，这类问题就不用去翻系统配置了。
    ///
    /// 注意：有声音服务器（PipeWire/PulseAudio）时，可选列表里只剩经服务器
    /// 路由的设备（`default` / `pipewire` / `pulse`）——直连硬件的 PCM 会被
    /// 引擎过滤掉，因为 ALSA 硬件设备是独占语义，抓走一张卡就会挤死服务器上
    /// 的其它所有应用（详见 `audio::engine` 里 `output_devices` 的注释）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_device: Option<String>,

    /// 播放模式。
    pub playback_mode: PlaybackMode,

    /// 音频缓存目录。
    pub cache_dir: PathBuf,

    /// 缓存上限（MiB），`0` 表示不限制。
    pub cache_limit_mib: u64,

    /// 歌词时间偏移（毫秒）。正值表示歌词提前显示。
    pub lyric_offset_ms: i64,

    /// 歌词换行过渡的时长上限（毫秒），`0` 表示不做过渡（直接切换）。
    ///
    /// 实际时长会按行距自适应：`min(本值, 该行到下一行的间隔 × 0.55)`。快歌的行
    /// 只有几百毫秒，按本值走会出现「上一次过渡还没走完就该换下一行」——看着不是
    /// 顺滑，是拖沓。
    ///
    /// 另外两种情况会**强制关闭**过渡，与这里的取值无关（见
    /// `LyricPane::transition_ms_for`）：`lite_mode`（它的卖点就是少重绘）与
    /// `basic_color`（16 色没有中间色阶，淡入会退化成整块硬翻，比不动画更怪）。
    #[serde(default = "default_lyric_anim_ms")]
    pub lyric_anim_ms: u64,

    /// 界面刷新间隔（毫秒）。这是控制 CPU 占用的主要旋钮。
    pub tick_ms: u64,

    /// 列表每页条目数。
    pub page_size: u32,

    /// 访问 KuGouMusicApi 时使用的 HTTP 代理。
    pub proxy: Option<String>,

    /// 音质。可选 `128` / `320` / `flac` / `high`。
    ///
    /// 128 是普通音质，下载体积最小、缓存最省空间，作为默认值。
    pub quality: String,

    /// 是否强制使用 16 色板（适配老终端）。
    pub basic_color: bool,

    /// 单曲下载目录。**不设置时**按 `~/Music` 处理（首次启动也要能正常下载）。
    /// 设置页里限定在几个常见位置之间选，避免路径写错把下载弄失败。
    pub download_dir: Option<String>,

    /// 界面主题。存的是 [`crate::ui::theme::ThemeName::id`]，解析不出来时回落
    /// 到默认主题——改坏配置文件不该让程序起不来。
    pub theme: ThemeName,

    /// 自定义键位：`动作名 -> 按键`，例如 `quit = "Q"`、`open_sources = "s"`。
    ///
    /// 动作名是 [`crate::keymap::Action`] 变体的 snake_case（见 keymap.rs 的
    /// `action_from_name`）。未列出的动作沿用默认键位，因此老配置文件不需要改。
    #[serde(default)]
    pub keymap: std::collections::BTreeMap<String, String>,

    /// 终端字符的「高:宽」比，用来矫正登录二维码的视觉形状。
    ///
    /// `2.0`（默认）表示一个字符的视觉高度约为宽度的两倍——此时用半块字符
    /// （▀▀ ▄▄ ██，一个字符承载两行模块）正好把二维码画成正方形。
    ///
    /// 但**字符高宽比并不总是 2:1**——某些宽字符字体（Nerd Font、CJK 等）的
    /// 字符会更扁/更方。如果你觉得二维码被拉长或压扁，把这个值改成自己实测
    /// 的字符高:宽比即可（按 L 出现二维码后，用尺子量一个字符就行）：
    ///
    /// * 比实际大 → 二维被纵向压扁（横向被"拉长"）
    /// * 比实际小 → 二维被纵向拉长（横向被"压扁"）
    ///
    /// 不会改字符、只用来切半块/全块模式：小于 1.5 用全块字符（一个模块一字符），
    /// 大于等于 1.5 用半块字符。
    #[serde(default = "default_qr_aspect")]
    pub qr_aspect: f32,

    /// 简易模式：关掉吃内存和 CPU 的那几样，换更低的占用。
    ///
    /// 开启后：不下载/解码封面（省掉图片解码与图形协议的开销，是最占内存的一
    /// 块）、不画实时频谱、刷新间隔降到 5fps。听歌本身不受影响。
    ///
    /// 实测正常模式播放中约 16.9 MiB（VmRSS），关掉这几样能再降一截——在低配
    /// 机器或电池供电时有用。分场景数字见 `docs/DESIGN.md`。
    #[serde(default)]
    pub lite_mode: bool,

    /// 首页大封面的铺满方式，见 [`CoverFill`]。
    #[serde(default)]
    pub cover_fill: CoverFill,

    /// 是否注册系统托盘（KDE/MATE 风格的 StatusNotifierItem）。
    ///
    /// 默认开启。探测失败（无图形会话 / 无 `StatusNotifierWatcher` / 无 session bus）
    /// 时静默跳过，不影响播放。命令行可用 `--no-tray` 临时关闭。
    ///
    /// 改动**重启生效**：运行中切换后需要重新启动进程，KDE 风格的 watcher 才会
    /// 把新增/移除的 item 同步到状态栏。
    #[serde(default = "default_tray")]
    pub tray: bool,

    /// 启动时若本机接口服务没在跑，是否自动拉起一个。
    ///
    /// 默认开启，这是「装完就能用」的关键：`cargo install` 只放一个二进制，接口服务
    /// 得由程序自己在第一次运行时准备好（见 `bootstrap.rs`）。想完全自己管服务
    /// （比如跑在别的机器上、或用 systemd 托管）就把它关掉，命令行也有
    /// `--no-api-start` 临时关闭。
    #[serde(default = "default_api_auto_start")]
    pub api_auto_start: bool,

    /// 本机 KuGouMusicApi 的目录。不设置时按 `bootstrap.rs` 里的候选顺序自动查找，
    /// 找不到就下载安装到用户数据目录。
    ///
    /// 显式设置后**不再自动查找与下载**——指错位置会直接报错，而不是悄悄换一个。
    /// 环境变量 `KUGOU_API_DIR` 可覆盖它。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_dir: Option<PathBuf>,

    /// 各音源的连接与身份配置，以及当前选中的音源。
    ///
    /// 切换音源时，`api_base` / `cookie` / `dfid` 会从选中的音源同步过来。
    /// 这三个字段仍是运行时实际读取的值，这样改动面最小，也不会漏掉某处引用。
    pub sources: SourceSet,

    /// 酷狗接口由哪套后端实现。
    ///
    /// 默认 `node`（本机 KuGouMusicApi）。`native` 是内嵌的纯 Rust 实现，
    /// 不需要 Node.js；两个酷狗平台共享这个开关，网易云与汽水不受影响。
    #[serde(default)]
    pub api_backend: crate::api::ApiBackend,

    /// 是否启动 WebSocket 服务，供第三方客户端读取播放状态、歌词并遥控播放。
    ///
    /// 默认开启。**只监听 `127.0.0.1`**，不接受来自其他主机的连接；命令行可用
    /// `--no-ws` 临时关闭。协议与 MoeKoeMusic 兼容，见 `ws.rs` 的模块注释。
    #[serde(default = "default_ws")]
    pub ws: bool,

    /// WebSocket 服务监听的端口。
    ///
    /// 默认 6520（与 MoeKoeMusic 相同，面向它的客户端可直接连上）。
    #[serde(default = "default_ws_port")]
    pub ws_port: u16,
}

/// 把命令行传上来的可选字符串收拾成「有内容才 Some」。
///
/// 空串与纯空白都当**没传**：用户在 shell 里拼变量时很容易传出空值，
/// 若照单全收就会把配置里已有的值抹掉——那比「参数没生效」更难排查。
fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn default_api_auto_start() -> bool {
    true
}

fn default_qr_aspect() -> f32 {
    2.0
}

/// 歌词换行过渡的默认时长（毫秒）。
///
/// 200ms 是照着 Apple Music 的手感取的（它大约 0.3s，但终端一帧 33ms、
/// 总共只有六七帧，再长就成慢动作了）。
fn default_lyric_anim_ms() -> u64 {
    200
}

fn default_tray() -> bool {
    // 桌面集成是「有更好」，默认开启比默认关闭更符合 kugou-tui 的定位（终端里的
    // 音乐客户端，状态栏上挂个图标是核心使用场景）。配置项里改成 false 即可关。
    //
    // 非 Unix 平台上托盘（StatusNotifierItem）与 MPRIS 都是 D-Bus 接口，根本
    // 不存在，默认就关——免得配置里留一个「开了也不会有反应」的 true。
    cfg!(unix)
}

fn default_ws() -> bool {
    // 与 `tray` 不同，WebSocket 不依赖桌面环境，各平台行为一致，所以默认就开。
    // 它只绑 127.0.0.1，且控制面只映射到已有的播放 `Action`，风险可控；
    // 不用的人用 `--no-ws` 或配置里 `ws = false` 关掉即可。
    true
}

fn default_ws_port() -> u16 {
    DEFAULT_WS_PORT
}

/// 首页那块大封面怎么铺满它的区域。
///
/// # 为什么需要这个开关
///
/// 封面区是「多少列 × 多少行」，换算成像素后几乎永远不是正方形，而专辑封面
/// 大多是正方形。**框和图的形状不一致时，「铺满」「不变形」「不裁剪」三者只能
/// 同时满足两个**，必须挑一个放弃。这个配置就是让用户自己挑。
///
/// 三种取值对应的取舍：
///
/// | 取值 | 铺满 | 变形 | 裁剪 |
/// |------|------|------|------|
/// | `crop`（默认） | 是 | 否 | 是 |
/// | `stretch` | 是 | 是 | 否 |
/// | `fit` | 否 | 否 | 否 |
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CoverFill {
    /// 居中裁剪：先等比放大到盖住整个区域，再居中裁掉溢出的边。
    ///
    /// 等价 CSS 的 `object-fit: cover`。铺满 100%、不变形，代价是方形封面在宽框
    /// 里会被裁掉上下两条边。默认值——专辑封面基本居中构图，裁掉一点边框通常
    /// 看不出来，而「框里空着一块」是一眼就能看见的。
    #[default]
    Crop,
    /// 拉伸铺满：直接把图拉到和区域一样大。
    ///
    /// 铺满 100%、不裁剪，代价是**变形**——方形封面在宽框里会被横向拉宽。
    Stretch,
    /// 完整显示：不裁不拉，把封面框缩到图片自己的比例再居中放进去。
    ///
    /// 图一定完整、也不变形，代价是框比图宽时左右会露出底色——也就是「没填满」。
    Fit,
}

impl CoverFill {
    /// 设置页与文档里显示的名字。
    pub fn label(self) -> &'static str {
        match self {
            Self::Crop => "居中裁剪",
            Self::Stretch => "拉伸铺满",
            Self::Fit => "完整显示",
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            api_base: DEFAULT_API_BASE.to_string(),
            cookie: None,
            dfid: None,
            volume: DEFAULT_VOLUME,
            audio_device: None,
            playback_mode: PlaybackMode::Sequential,
            cache_dir: default_cache_dir(),
            cache_limit_mib: DEFAULT_CACHE_LIMIT_MIB,
            lyric_offset_ms: 0,
            lyric_anim_ms: default_lyric_anim_ms(),
            tick_ms: DEFAULT_TICK_MS,
            page_size: DEFAULT_PAGE_SIZE,
            proxy: None,
            quality: DEFAULT_QUALITY.to_string(),
            basic_color: false,
            theme: ThemeName::default(),
            download_dir: None,
            keymap: std::collections::BTreeMap::new(),
            qr_aspect: default_qr_aspect(),
            lite_mode: false,
            cover_fill: CoverFill::default(),
            tray: default_tray(),
            api_auto_start: default_api_auto_start(),
            api_dir: None,
            sources: SourceSet::default(),
            api_backend: crate::api::ApiBackend::default(),
            ws: default_ws(),
            ws_port: default_ws_port(),
        }
    }
}

impl Config {
    /// 当前选中的音源种类。
    pub fn active_source_kind(&self) -> SourceKind {
        self.sources.active
    }

    /// 切换到指定音源：把它的连接与身份信息同步到运行时字段。
    ///
    /// 只改配置，**不碰播放队列与当前曲目**，因此切换过程中播放不会中断。
    pub fn switch_source(&mut self, kind: SourceKind) {
        let profile = self.sources.profile(kind).clone();
        self.sources.active = kind;
        self.api_base = profile.api_base;
        self.cookie = profile.cookie;
        self.dfid = profile.device_id;
    }

    /// 把当前运行时字段回填进选中音源的档案。
    ///
    /// 登录成功或自动取到 dfid 后调用，保证切走再切回来时身份还在。
    pub fn sync_active_source(&mut self) {
        let kind = self.sources.active;

        // 汽水**不参与镜像**：它的凭据存在自己档案里（`[sources.sodam].cookie`），
        // 而顶层 `cookie` 是酷狗的地盘。
        //
        // 镜像会让刚登录成功的汽水被顶层的空值覆盖——`save()` 第一件事就是调
        // 这里，所以时序是「写入 cookie → 立刻 save → 被覆盖成 None」，
        // 表现出来就是**登录成功、一重启却说没登录**，而且配置文件里根本找不到
        // cookie 这个键（用户看到的现象正是如此）。
        if kind == SourceKind::Sodam {
            return;
        }

        let profile = self.sources.profile_mut(kind);
        // 刻意**不**回写 api_base：它是音源自己的身份，不该被运行时的地址覆盖。
        // 否则一旦用 `--api-base` 临时指向别处，就会把该音源的地址改坏——
        // 表现是「明明选了概念版，却一直在打标准版」，而 dfid 又只对概念版有效，
        // 于是取链报 20028。地址只由 switch_source() 从档案里取。
        profile.cookie = self.cookie.clone();
        profile.device_id = self.dfid.clone();
    }
}

impl Config {
    /// 配置文件路径。
    pub fn path() -> PathBuf {
        config_root().join("config.toml")
    }

    /// 日志文件路径。
    pub fn log_path() -> PathBuf {
        default_cache_dir().join("kugou-tui.log")
    }

    /// 读取配置。任何异常都退化为默认配置，保证程序总能启动。
    pub fn load() -> Self {
        let path = Self::path();
        let config = match std::fs::read_to_string(&path) {
            Ok(text) => match toml::from_str::<Self>(&text) {
                Ok(config) => config.normalized(),
                Err(error) => {
                    tlog!(
                        crate::logger::LEVEL_WARN,
                        "解析配置文件 {} 失败：{error}，改用默认配置",
                        path.display()
                    );
                    Self::default()
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(error) => {
                tlog!(
                    crate::logger::LEVEL_WARN,
                    "读取配置文件 {} 失败：{error}，改用默认配置",
                    path.display()
                );
                Self::default()
            }
        };
        config.with_env()
    }

    /// 环境变量覆盖。
    ///
    /// 与 `cli.rs` 里那些 `env = "..."` 的参数不冲突：那些是「命令行 > 环境变量 >
    /// 配置文件」的常规优先级，而这里的两项没有对应的命令行参数（`api_dir` 指向的
    /// 是本机服务目录，做成命令行参数只会让人一次性的值写进配置里）。
    fn with_env(mut self) -> Self {
        if let Some(dir) = std::env::var_os("KUGOU_API_DIR").filter(|value| !value.is_empty()) {
            self.api_dir = Some(PathBuf::from(dir));
        }
        if let Ok(value) = std::env::var("KUGOU_API_AUTO_START") {
            match value.trim().to_ascii_lowercase().as_str() {
                "0" | "false" | "no" | "off" => self.api_auto_start = false,
                "1" | "true" | "yes" | "on" => self.api_auto_start = true,
                _ => {}
            }
        }
        self
    }

    /// 写回配置文件，使用 pretty 格式方便用户手工编辑。
    ///
    /// 文件里存着登录 token，所以目录收成 `0700`、文件收成 `0600`——默认的
    /// `0644` 会让同机器上的其他用户直接读到你的账号凭据。
    ///
    /// # 为什么这里要自己 sync 一次
    ///
    /// 顶层 `cookie` / `dfid` 是当前会话的真相（`App` 里所有读写都走它们），
    /// 而启动时 `switch_source()` 会**反过来**用音源档案覆盖这两个顶层字段。
    /// 如果写盘前不先回填进档案，登录后重启就会退回未登录、dfid 也要重新探测。
    ///
    /// 曾经只在「按 v 切音源」这一个地方做同步，结果漏掉了登录与自动取 dfid
    /// 两条路径。同步收进 `save()` 之后，任何一次落盘都不可能再漏。
    pub fn save(&mut self) -> Result<()> {
        self.sync_active_source();

        let path = Self::path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| AppError::io_at(parent.display().to_string(), error))?;
            restrict_permissions(parent, 0o700)?;
        }

        let text = toml::to_string_pretty(self)
            .map_err(|error| AppError::Config(format!("序列化配置失败：{error}")))?;
        std::fs::write(&path, text)
            .map_err(|error| AppError::io_at(path.display().to_string(), error))?;
        restrict_permissions(&path, 0o600)
    }

    /// 用命令行参数覆盖配置。仅当用户显式传参时才覆盖。
    pub fn merge_cli(&mut self, cli: &Cli) {
        if let Some(api_base) = cli.api_base.as_deref() {
            let trimmed = api_base.trim();
            if !trimmed.is_empty() {
                self.api_base = trimmed.to_string();
            }
        }
        if let Some(cookie) = cli.cookie.as_deref() {
            let trimmed = cookie.trim();
            if !trimmed.is_empty() {
                self.cookie = Some(trimmed.to_string());
            }
        }
        if let Some(volume) = cli.volume {
            self.volume = f32::from(volume) / 100.0;
        }
        if let Some(cache_dir) = cli.cache_dir.as_ref() {
            self.cache_dir = cache_dir.clone();
        }
        if let Some(limit) = cli.cache_limit {
            self.cache_limit_mib = limit;
        }
        if let Some(tick_ms) = cli.tick_ms {
            self.tick_ms = tick_ms;
        }
        if let Some(page_size) = cli.page_size {
            self.page_size = page_size;
        }
        if let Some(proxy) = cli.proxy.as_deref() {
            let trimmed = proxy.trim();
            if !trimmed.is_empty() {
                self.proxy = Some(trimmed.to_string());
            }
        }
        if cli.basic_color {
            self.basic_color = true;
        }
        if cli.no_tray {
            self.tray = false;
        }
        if cli.no_api_start {
            self.api_auto_start = false;
        }
        if let Some(backend) = cli.api {
            self.api_backend = backend;
        }
        if cli.no_ws {
            self.ws = false;
        }
        if let Some(port) = cli.ws_port {
            self.ws_port = port;
        }

        // 汽水的凭据与签名：只覆盖**显式传了**的项，其余保留配置文件里的值。
        // 逐项覆盖而不是整块替换——用户可能只想补一个签名头，
        // 不该因为没传 `--sodam-iid` 就把已配的 iid 清掉。
        if let Some(cookie) = non_empty(cli.sodam_cookie.as_deref()) {
            let profile = self.sources.profile_mut(SourceKind::Sodam);
            profile.cookie = Some(cookie.to_string());
        }
        if let Some(device_id) = non_empty(cli.sodam_device_id.as_deref()) {
            // 设备的指纹存在**档案**里（`device_id` 字段），签名凭证里另存一份
            // `device_id` 是为了与 `x-helios` 成对持久化；两者同步写入。
            self.sources.profile_mut(SourceKind::Sodam).device_id = Some(device_id.to_string());
            self.sources.sodam_app.device_id = device_id.to_string();
        }
        if let Some(iid) = non_empty(cli.sodam_iid.as_deref()) {
            self.sources.sodam_app.iid = iid.to_string();
        }
        if let Some(helios) = non_empty(cli.sodam_x_helios.as_deref()) {
            self.sources.sodam_app.x_helios = helios.to_string();
        }
        if let Some(medusa) = non_empty(cli.sodam_x_medusa.as_deref()) {
            self.sources.sodam_app.x_medusa = medusa.to_string();
        }
        // 签名服务地址/Token。这里**不能**套 `non_empty` 的「空即不覆盖」：
        // `none` 是一个有意义的值（显式关掉签名），它必须能传进来。
        if let Some(url) = cli.sodam_signer_url.as_deref().map(str::trim)
            && !url.is_empty()
        {
            self.sources.sodam_app.signer_url = url.to_string();
        }
        if let Some(token) = non_empty(cli.sodam_signer_token.as_deref()) {
            self.sources.sodam_app.signer_token = token.to_string();
        }

        self.normalize();
    }

    /// 把配置约束到合法区间，避免手改配置文件写出越界值。
    pub fn normalized(mut self) -> Self {
        self.normalize();
        self
    }

    fn normalize(&mut self) {
        // 从未初始化过的音源：地址为空时补默认值并置为启用。
        //
        // 老配置文件里没有这些音源的段，serde 会走 `SourceProfile::default()`，
        // 那里给的是空地址——照着空地址发请求必然失败，用户只会看到「连不上」
        // 却不知道要填什么。这里按音源种类补上默认端口，并视为启用：
        // 显式填过地址的音源则尊重用户设置，不动它的 enabled。
        for kind in SourceKind::ALL {
            let profile = self.sources.profile_mut(kind);
            if profile.api_base.trim().is_empty() {
                profile.api_base = kind.default_api_base().to_string();
                profile.enabled = true;
            }
        }

        // 汽水的设备指纹在**两个地方**各存了一份：档案的 `device_id`（通用字段）
        // 与签名凭证的 `device_id`（要跟 `x-helios` 成对）。这里补齐缺失的一侧，
        // 让「只在一处配了」也能工作。
        //
        // 哪一侧优先：档案里的显式值。反过来（凭证侧有值就灌进档案）也说得通，
        // 但档案是用户直接编辑的那份，语义上更「权威」。
        if self.sources.sodam_app.device_id.trim().is_empty() {
            if let Some(device_id) = self
                .sources
                .profile(SourceKind::Sodam)
                .device_id
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                self.sources.sodam_app.device_id = device_id.to_string();
            }
        } else if self
            .sources
            .profile(SourceKind::Sodam)
            .device_id
            .as_deref()
            .map(str::trim)
            .filter(|value| value.is_empty())
            .is_some()
        {
            self.sources.profile_mut(SourceKind::Sodam).device_id =
                Some(self.sources.sodam_app.device_id.trim().to_string());
        }

        // 签名凭证的字符串统一去空白：抓包工具很容易在末尾带一个换行或空格，
        // 而签名头是**精确比对**的，多一个字符就失效。
        for field in [
            &mut self.sources.sodam_app.device_id,
            &mut self.sources.sodam_app.iid,
            &mut self.sources.sodam_app.fp,
            &mut self.sources.sodam_app.x_helios,
            &mut self.sources.sodam_app.x_medusa,
            &mut self.sources.sodam_app.user_agent,
        ] {
            let trimmed = field.trim();
            if trimmed.len() != field.len() {
                *field = trimmed.to_string();
            }
        }

        self.api_base = self.api_base.trim().trim_end_matches('/').to_string();
        if self.api_base.is_empty() {
            self.api_base = DEFAULT_API_BASE.to_string();
        }
        if !self.api_base.starts_with("http://") && !self.api_base.starts_with("https://") {
            self.api_base = format!("http://{}", self.api_base);
        }

        self.volume = if self.volume.is_finite() {
            self.volume.clamp(0.0, 1.0)
        } else {
            DEFAULT_VOLUME
        };

        self.tick_ms = self.tick_ms.clamp(50, 5_000);
        self.page_size = self.page_size.clamp(5, 200);
        self.lyric_offset_ms = self.lyric_offset_ms.clamp(-10_000, 10_000);

        let quality = self.quality.trim().to_ascii_lowercase();
        self.quality = if SUPPORTED_QUALITIES.contains(&quality.as_str()) {
            quality
        } else {
            DEFAULT_QUALITY.to_string()
        };

        if self.cache_dir.as_os_str().is_empty() {
            self.cache_dir = default_cache_dir();
        } else {
            // 允许在配置文件里写 `~/music-cache`
            self.cache_dir = expand_tilde(&self.cache_dir);
        }

        if let Some(cookie) = self.cookie.as_ref() {
            let trimmed = cookie.trim();
            if trimmed.is_empty() {
                self.cookie = None;
            } else if trimmed.len() != cookie.len() {
                self.cookie = Some(trimmed.to_string());
            }
        }
    }

    /// 组装最终发给 API 服务的 cookie 串。
    ///
    /// 规则统一在 [`crate::source::cookie_header_for`] 里，这里只负责把当前会话的
    /// 凭据与音源递过去：先规范化（网易云服务端下发的是整段 `Set-Cookie`，不修就
    /// 认不出里面的 `MUSIC_U`），再按当前音源决定要不要补 `dfid`（只有酷狗用得上）。
    ///
    /// `/song/url` 缺少 dfid 会返回「本次请求需要验证」，所以酷狗那边少不得。
    pub fn cookie_header(&self) -> Option<String> {
        crate::source::cookie_header_for(
            self.active_source_kind(),
            self.cookie.as_deref(),
            self.dfid.as_deref(),
        )
    }

    /// 是否已配置登录态。
    pub fn is_logged_in(&self) -> bool {
        // 汽水的凭据存在**自己档案**里，不走顶层的 `self.cookie`：
        // 它直连公网，没有「服务端替它管 cookie」这回事。
        if self.active_source_kind() == SourceKind::Sodam {
            return self
                .sources
                .profile(SourceKind::Sodam)
                .cookie
                .as_deref()
                .is_some_and(|cookie| cookie.contains("sessionid"));
        }

        self.cookie
            .as_deref()
            .map(|cookie| {
                // 酷狗：自己拼的 \`token=; userid=\`；
                // 网易云：服务端下发的那串里是 \`MUSIC_U=\`，没有 token/userid。
                // 只认前者的后果是——网易云登录完再重启，登录态凭空消失。
                (cookie.contains("token=") && cookie.contains("userid="))
                    || cookie.contains("MUSIC_U=")
            })
            .unwrap_or(false)
    }

    /// 确保缓存目录存在。
    pub fn ensure_cache_dir(&self) -> Result<()> {
        std::fs::create_dir_all(&self.cache_dir)
            .map_err(|error| AppError::io_at(self.cache_dir.display().to_string(), error))
    }
}

/// 缓存根目录。
pub fn default_cache_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(APP_DIR_NAME)
}

/// 配置根目录。
///
/// 可以被 `KUGOU_TUI_CONFIG_DIR` 整个覆盖。两个用途：
///
/// * **便携安装**——把程序、配置、缓存一起放 U 盘或绿色目录里，插到哪台机器都一样；
/// * **可测**——`dirs` 在 Windows 上走的是 Win32 的 Known Folder（`SHGetKnownFolderPath`），
///   环境变量（`APPDATA` 等）管不着它，没有这个开关就没法在 Windows 上把
///   「配置能活过一次重启」那条测试隔离到临时目录里，只能跳过。
pub(crate) fn config_root() -> PathBuf {
    if let Some(dir) = std::env::var_os("KUGOU_TUI_CONFIG_DIR") {
        // 空值当作没设：`KUGOU_TUI_CONFIG_DIR=` 这种写法在 shell 里很常见，
        // 而一个空路径会让配置落到当前目录，比忽略它更难查。
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(APP_DIR_NAME)
}

/// 把 `~` 展开成用户主目录，供配置文件里手写路径时使用。
///
/// 两种分隔符都吃：Windows 上用户很自然会写成 `~\Music`。
pub fn expand_tilde(path: &Path) -> PathBuf {
    let Some(text) = path.to_str() else {
        return path.to_path_buf();
    };
    let Some(rest) = text.strip_prefix('~') else {
        return path.to_path_buf();
    };
    let Some(home) = dirs::home_dir() else {
        return path.to_path_buf();
    };
    home.join(rest.trim_start_matches(['/', '\\']))
}

/// 收紧文件或目录权限。非 Unix 平台上是空操作。
///
/// 刻意用 `set_permissions` 而不是依赖 umask：umask 是进程级、由启动环境决定的，
/// 不能让「凭据文件谁能读」取决于用户从哪个 shell 启动程序。
pub(crate) fn restrict_permissions(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .map_err(|error| AppError::io_at(path.display().to_string(), error))?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::Config;
    use crate::Cli;
    use crate::source::SourceKind;
    use crate::source::sodam;
    use clap::Parser;

    #[test]
    fn normalizes_api_base_and_volume() {
        let mut config = Config {
            api_base: "127.0.0.1:3000/".to_string(),
            volume: 9.0,
            ..Config::default()
        };
        config = config.normalized();
        assert_eq!(config.api_base, "http://127.0.0.1:3000");
        assert!((config.volume - 1.0).abs() < f32::EPSILON);
    }

    /// 端到端锁住「登录态与 dfid 必须活过一次重启」。
    ///
    /// 启动时 `main.rs` 会无条件 `switch_source(active)`，用**音源档案**覆盖顶层字段。
    /// 所以只写顶层而不回填进档案，重启后登录态就没了（同理 dfid 要重新探测）。
    ///
    /// 这里跑真实落盘 + 真实 `load()`。为了不碰用户配置，先把
    /// `KUGOU_TUI_CONFIG_DIR` 指到临时目录，结束后恢复原值——`catch_unwind`
    /// 保证失败时也会恢复。
    ///
    /// **刻意不用 `XDG_CONFIG_HOME`**：那是 XDG 规范，只有 Unix 认；
    /// Windows 上 `dirs` 走的是 `SHGetKnownFolderPath`，那个环境变量对它无效，
    /// 于是这条测试会去改写用户真实的 `%APPDATA%\kugou-tui\config.toml`。
    #[test]
    fn login_survives_restart() {
        let temp = std::env::temp_dir().join(format!("kugou-tui-cfgtest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).expect("建临时配置目录");

        // SAFETY: 单测进程内临时改写并恢复；其它测试不读配置路径，无交叉影响。
        let previous = std::env::var_os("KUGOU_TUI_CONFIG_DIR");
        unsafe { std::env::set_var("KUGOU_TUI_CONFIG_DIR", &temp) };

        let outcome = std::panic::catch_unwind(|| {
            let mut config = Config::default();
            config.sources.active = SourceKind::KugouConcept;
            config.switch_source(SourceKind::KugouConcept);
            // 模拟「应用内扫码登录成功 + 自动取到 dfid」：只写顶层，不手动 sync
            config.cookie = Some("token=abc; userid=42".to_string());
            config.dfid = Some("df-xyz".to_string());
            config.save().expect("保存配置");

            // 模拟重启：main.rs 在 merge_cli 之前会无条件 switch_source(active)
            let mut restarted = Config::load();
            let active = restarted.active_source_kind();
            restarted.switch_source(active);

            assert_eq!(
                restarted.cookie.as_deref(),
                Some("token=abc; userid=42"),
                "重启后登录态丢失"
            );
            assert_eq!(
                restarted.dfid.as_deref(),
                Some("df-xyz"),
                "重启后 dfid 丢失"
            );
            assert!(restarted.is_logged_in());
        });

        match previous {
            Some(value) => unsafe { std::env::set_var("KUGOU_TUI_CONFIG_DIR", value) },
            None => unsafe { std::env::remove_var("KUGOU_TUI_CONFIG_DIR") },
        }
        let _ = std::fs::remove_dir_all(&temp);

        assert!(outcome.is_ok(), "登录态/设备指纹在重启后丢失");
    }

    /// 锁住 H1 的修复：顶层 cookie/dfid 必须能被回填进选中音源的档案。
    ///
    /// 启动时 `switch_source()` 会**反过来**用档案覆盖顶层字段，所以只写顶层而不
    /// 同步进档案的话，登录态和 dfid 下次启动就没了。
    #[test]
    fn sync_active_source_writes_identity_into_profile() {
        let mut config = Config {
            cookie: Some("token=t; userid=1".to_string()),
            dfid: Some("df-1".to_string()),
            ..Config::default()
        };
        config.sources.active = SourceKind::KugouConcept;

        config.sync_active_source();

        let profile = config.sources.profile(SourceKind::KugouConcept);
        assert_eq!(profile.cookie.as_deref(), Some("token=t; userid=1"));
        assert_eq!(profile.device_id.as_deref(), Some("df-1"));
        // 另一个音源不该被牵连（两个平台的登录态不通用）
        assert!(config.sources.profile(SourceKind::Kugou).cookie.is_none());
    }

    /// **回归**：`save()` 不能把刚登录的汽水凭据覆盖掉。
    ///
    /// 真实现象：汽水扫码登录成功、配置文件里却找不到 cookie，重启后显示「未登录」。
    /// 根因是 `save()` 第一件事就是调 `sync_active_source()`，而它会执行
    /// `profile.cookie = self.cookie.clone()`——顶层 cookie 是酷狗的地盘，汽水时
    /// 恒为 None，于是刚写进去的凭据立刻被覆盖成 None。
    #[test]
    fn saving_must_not_clobber_the_soda_cookie() {
        let mut config = Config::default();
        config.sources.active = SourceKind::Sodam;
        // 模拟「扫码登录成功」：凭据写进汽水档案。顶层 cookie 保持为空（那是酷狗的）。
        config.sources.profile_mut(SourceKind::Sodam).cookie =
            Some("sessionid_ss=abc; sid_guard=xyz".to_string());

        config.sync_active_source();

        assert_eq!(
            config.sources.profile(SourceKind::Sodam).cookie.as_deref(),
            Some("sessionid_ss=abc; sid_guard=xyz"),
            "汽水的凭据不能被顶层的空 cookie 覆盖"
        );
        // 顶层不该被反向写入汽水的凭据
        assert!(config.cookie.is_none(), "顶层 cookie 是酷狗的地盘");
    }

    /// 反过来，酷狗/网易云仍要按老规矩把顶层身份镜像进档案——这是它们
    /// 从顶层字段读登录态的既有路径，不能一起取消。
    #[test]
    fn other_sources_still_mirror_the_top_level_identity() {
        for kind in [
            SourceKind::Kugou,
            SourceKind::KugouConcept,
            SourceKind::Netease,
        ] {
            let mut config = Config {
                cookie: Some("token=t; userid=1".to_string()),
                dfid: Some("df-1".to_string()),
                ..Config::default()
            };
            config.sources.active = kind;
            config.sync_active_source();

            let profile = config.sources.profile(kind);
            assert_eq!(
                profile.cookie.as_deref(),
                Some("token=t; userid=1"),
                "{kind:?}"
            );
            assert_eq!(profile.device_id.as_deref(), Some("df-1"), "{kind:?}");
        }
    }

    /// 同步刻意**不**回写 api_base：地址属于音源自己，不该被运行时的临时值污染。
    #[test]
    fn sync_active_source_keeps_profile_address() {
        let mut config = Config {
            api_base: "http://127.0.0.1:9999".to_string(),
            ..Config::default()
        };
        config.sources.active = SourceKind::Kugou;
        config.sync_active_source();
        assert_eq!(
            config.sources.profile(SourceKind::Kugou).api_base,
            "http://127.0.0.1:3000"
        );
    }

    /// 老配置文件里没有 `lyric_anim_ms`，读进来必须拿到默认值——新增配置项不能
    /// 让用户手上那份配置突然读不出来。
    #[test]
    fn missing_new_fields_fall_back_to_defaults() {
        let parsed: Config = toml::from_str(
            r#"
api_base = "http://127.0.0.1:3000"
volume = 0.5
"#,
        )
        .expect("老配置应当能读");
        // 断言字面量而不是那个函数：这里要锁的是「对外承诺的默认值就是 200ms」，
        // 拿常量比自己跟自己比，改了默认值也照样通过。
        assert_eq!(parsed.lyric_anim_ms, 200);
        // 新增的 `api_backend` 同理：老配置里没有这一项，必须落到 node。
        // 若这里悄悄变成 native，老用户升级后会在毫不知情的情况下换掉整条接口链路。
        assert_eq!(parsed.api_backend, crate::api::ApiBackend::Node);
    }

    /// `--api native` 要能覆盖配置里的值，也要能落盘、读回。
    #[test]
    fn cli_api_backend_overrides_and_round_trips() {
        let cli = Cli::parse_from(["kugou-tui", "--api", "native"]);
        let mut config = Config::default();
        assert_eq!(config.api_backend, crate::api::ApiBackend::Node);

        config.merge_cli(&cli);
        assert_eq!(config.api_backend, crate::api::ApiBackend::Native);

        let text = toml::to_string_pretty(&config).expect("要能序列化");
        assert!(text.contains("api_backend = \"native\""), "实际：\n{text}");
        let parsed: Config = toml::from_str(&text).expect("要能读回");
        assert_eq!(parsed.api_backend, crate::api::ApiBackend::Native);
    }

    /// 不传 `--api` 时保留配置里已有的值，不要被默认值抹掉。
    #[test]
    fn missing_api_flag_keeps_the_configured_backend() {
        let cli = Cli::parse_from(["kugou-tui"]);
        let mut config = Config {
            api_backend: crate::api::ApiBackend::Native,
            ..Config::default()
        };
        config.merge_cli(&cli);
        assert_eq!(config.api_backend, crate::api::ApiBackend::Native);
    }

    #[test]
    fn appends_dfid_only_when_absent() {
        let mut config = Config {
            cookie: Some("token=t; userid=1".to_string()),
            dfid: Some("abc".to_string()),
            ..Config::default()
        };
        assert_eq!(
            config.cookie_header().as_deref(),
            Some("token=t; userid=1; dfid=abc")
        );

        config.cookie = Some("token=t; userid=1; dfid=keep".to_string());
        assert_eq!(
            config.cookie_header().as_deref(),
            Some("token=t; userid=1; dfid=keep")
        );
    }

    // ==================================================================
    // 汽水音乐
    // ==================================================================

    /// 命令行传汽水凭据时要写进**它自己的档案**，不能碰到当前音源（酷狗）。
    ///
    /// 写错地方的后果很隐蔽：cookie 进了 `[sources.kugou]`，于是下次启动
    /// 切回酷狗时带着一串汽水的 cookie 发过去，而汽水的登录态永远读不到。
    #[test]
    fn cli_sodam_credentials_land_in_the_soda_profile() {
        let cli = Cli::parse_from([
            "kugou-tui",
            "--sodam-cookie",
            "sessionid_ss=abc",
            "--sodam-device-id",
            "2204957404565290",
            "--sodam-iid",
            "iid-7",
            "--sodam-x-helios",
            "helios-1",
            "--sodam-x-medusa",
            "medusa-1",
        ]);

        let mut config = Config::default();
        config.merge_cli(&cli);

        let soda = config.sources.profile(SourceKind::Sodam);
        assert_eq!(soda.cookie.as_deref(), Some("sessionid_ss=abc"));
        assert_eq!(soda.device_id.as_deref(), Some("2204957404565290"));
        // 酷狗那边不能被污染
        assert!(config.sources.profile(SourceKind::Kugou).cookie.is_none());

        let app = &config.sources.sodam_app;
        assert_eq!(app.device_id, "2204957404565290");
        assert_eq!(app.iid, "iid-7");
        assert_eq!(app.x_helios, "helios-1");
        assert_eq!(app.x_medusa, "medusa-1");
        assert!(
            !app.device_id.is_empty() && !app.x_helios.is_empty() && !app.x_medusa.is_empty(),
            "三个静态签名头都要落到配置里"
        );
    }

    /// 只传一部分参数时，其余的必须保留配置文件里的值。
    ///
    /// 整块替换会让「只想临时补一个签名头」变成「把已配的 iid 抹掉」——
    /// 而 iid 抹掉后整曲取流直接失效，比参数没生效更难排查。
    #[test]
    fn cli_sodam_credentials_merge_rather_than_replace() {
        let mut config = Config::default();
        config.sources.sodam_app = sodam::client::AppCredentials {
            device_id: "dev-1".to_string(),
            iid: "iid-original".to_string(),
            x_helios: "helios-original".to_string(),
            x_medusa: "medusa-original".to_string(),
            ..Default::default()
        };

        let cli = Cli::parse_from(["kugou-tui", "--sodam-x-helios", "helios-new"]);
        config.merge_cli(&cli);

        assert_eq!(
            config.sources.sodam_app.x_helios, "helios-new",
            "传了的要更新"
        );
        assert_eq!(
            config.sources.sodam_app.iid, "iid-original",
            "没传的必须保留"
        );
        assert_eq!(config.sources.sodam_app.x_medusa, "medusa-original");
        assert_eq!(config.sources.sodam_app.device_id, "dev-1");
    }

    /// 空串与纯空白都当「没传」——shell 拼变量时很容易传出空值，
    /// 照单全收会把已有配置抹掉。
    #[test]
    fn empty_cli_values_do_not_clobber_existing_config() {
        let mut config = Config::default();
        config.sources.sodam_app.x_helios = "helios-keep".to_string();

        let cli = Cli::parse_from(["kugou-tui", "--sodam-x-helios", "   "]);
        config.merge_cli(&cli);

        assert_eq!(config.sources.sodam_app.x_helios, "helios-keep");
    }

    /// 签名头是**精确比对**的，抓包工具带来的尾随空白会让它直接失效。
    #[test]
    fn normalize_trims_signature_credentials() {
        let mut config = Config::default();
        config.sources.sodam_app.x_helios = "  helios \n".to_string();
        config.sources.sodam_app.x_medusa = "medusa\t".to_string();
        config.normalize();

        assert_eq!(config.sources.sodam_app.x_helios, "helios");
        assert_eq!(config.sources.sodam_app.x_medusa, "medusa");
    }

    /// 设备指纹在档案与签名凭证里各存一份，normalize 要把它们对齐，
    /// 否则「只在一处配了」会静默失效。
    #[test]
    fn normalize_syncs_the_two_copies_of_sodam_device_id() {
        // 只在档案里配了 → 补进签名凭证
        let mut config = Config::default();
        config.sources.profile_mut(SourceKind::Sodam).device_id = Some("dev-1".to_string());
        config.normalize();
        assert_eq!(config.sources.sodam_app.device_id, "dev-1");

        // 只在签名凭证里配了 → 补进档案
        let mut config = Config::default();
        config.sources.profile_mut(SourceKind::Sodam).device_id = Some("  ".to_string());
        config.sources.sodam_app.device_id = "dev-2".to_string();
        config.normalize();
        assert_eq!(
            config
                .sources
                .profile(SourceKind::Sodam)
                .device_id
                .as_deref(),
            Some("dev-2")
        );
    }

    /// 汽水的登录态不走顶层 `cookie`，`is_logged_in` 必须按当前音源分别判断。
    #[test]
    fn is_logged_in_uses_the_per_source_credentials() {
        let mut config = Config::default();
        config.sources.active = SourceKind::Sodam;
        assert!(!config.is_logged_in(), "没配 cookie 就是未登录");

        config.sources.profile_mut(SourceKind::Sodam).cookie = Some("sessionid_ss=abc".to_string());
        assert!(config.is_logged_in(), "配了 sessionid 就算已登录");

        // 切回酷狗后，汽水的登录态不该让它显示成「已登录」
        config.sources.active = SourceKind::Kugou;
        assert!(!config.is_logged_in());
    }

    /// 老配置里没有汽水的段时，normalize 要补上公网地址并置为启用。
    #[test]
    fn normalize_backfills_sodam_for_existing_configs() {
        let mut config = Config::default();
        // 模拟老配置：汽水的段缺失，serde 给了默认（地址为空）
        config.sources.sodam.api_base = String::new();
        config.normalize();

        assert_eq!(config.sources.sodam.api_base, "https://api.qishui.com");
        assert!(
            config.sources.sodam.enabled,
            "新音源默认要能用，不能要用户手动开"
        );
    }

    /// `device_id` 对汽水是**设备标识**而不是酷狗的 dfid，
    /// 所以不能被拼进 cookie 头。
    #[test]
    fn sodam_cookie_header_does_not_append_device_id() {
        let mut profile = crate::source::SourceProfile::new(SourceKind::Sodam);
        profile.cookie = Some("sessionid_ss=abc".to_string());
        profile.device_id = Some("2204957404565290".to_string());

        assert_eq!(
            profile.cookie_header(SourceKind::Sodam).as_deref(),
            Some("sessionid_ss=abc"),
            "汽水不认 dfid，拼上去反而可能被判异常"
        );
    }

    /// 汽水的配置要能**写进 TOML 再读回来**——`config.toml` 是用户手动编辑的地方，
    /// 序列化不过就等于没法手填。
    #[test]
    fn sodam_credentials_survive_a_toml_round_trip() {
        let mut config = Config::default();
        config.sources.active = SourceKind::Sodam;
        config.sources.profile_mut(SourceKind::Sodam).cookie =
            Some("sessionid_ss=abc; sessionid=def".to_string());
        config.sources.profile_mut(SourceKind::Sodam).device_id = Some("dev-1".to_string());
        config.sources.sodam_app = sodam::client::AppCredentials {
            device_id: "dev-1".to_string(),
            iid: "iid-1".to_string(),
            fp: String::new(),
            x_helios: "helios-1".to_string(),
            x_medusa: "medusa-1".to_string(),
            user_agent: String::new(),
            signer_url: "http://127.0.0.1:8799".to_string(),
            signer_token: String::new(),
        };

        let text = toml::to_string_pretty(&config).expect("汽水配置要能序列化");
        // 落盘后是用户能看懂、能手改的东西
        assert!(text.contains("[sources.sodam]"), "实际：\n{text}");
        assert!(text.contains("[sources.sodam_app]"), "实际：\n{text}");
        assert!(text.contains("sessionid_ss=abc"));

        let parsed: Config = toml::from_str(&text).expect("汽水配置要能读回来");
        assert_eq!(parsed.sources.active, SourceKind::Sodam);
        assert_eq!(
            parsed.sources.profile(SourceKind::Sodam).cookie.as_deref(),
            Some("sessionid_ss=abc; sessionid=def")
        );
        assert_eq!(parsed.sources.sodam_app.x_helios, "helios-1");
        assert_eq!(parsed.sources.sodam_app.x_medusa, "medusa-1");
        assert_eq!(parsed.sources.sodam_app.iid, "iid-1");
        assert_eq!(parsed.sources.sodam_app.device_id, "dev-1");
        assert_eq!(parsed.sources.sodam_app.x_helios, "helios-1");
    }
}
