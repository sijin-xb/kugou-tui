//! 设置页的条目定义。
//!
//! # 为什么把「候选值」集中在这里
//!
//! 每个条目要么是枚举（主题、音质、播放模式），要么是离散档位（刷新间隔、
//! 每页条数、缓存上限）。散在各处写死的话，设置页显示的顺序和实际能取到的
//! 值很容易对不上——用户按右键切出来的档位和他看到的不一致，是最难查的那种 bug。
//!
//! 这里只放**纯数据**：界面怎么画、值怎么落到 [`crate::config::Config`] 上，
//! 分别归 `ui::views::settings` 与 `app::update`。

use std::path::{Path, PathBuf};

use crate::app::queue::PlaybackMode;
use crate::app::state::AppState;
use crate::config::CoverFill;
use crate::ui::views::settings::toggle_text;

/// 设置项。顺序即设置页里的显示顺序。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Setting {
    /// 界面主题。
    Theme,
    /// 播放音质。
    Quality,
    /// 播放模式（顺序 / 列表循环 / 单曲循环 / 随机）。
    PlaybackMode,
    /// 界面刷新间隔，越大越省电。
    RefreshMs,
    /// 歌词时间偏移。
    LyricOffsetMs,
    /// 列表每页条数。
    PageSize,
    /// 音频缓存上限。
    CacheLimitMib,
    /// 强制 16 色（老终端）。
    BasicColor,
    /// 侧栏歌词面板。
    LyricPanel,
    /// 侧边栏（标签导航）。
    Sidebar,
    /// 简易模式：关封面与频谱，省内存和 CPU。
    LiteMode,
    /// 单曲下载到哪个目录（只能选预设的几个常见位置）。
    DownloadDir,
    /// 首页大封面怎么铺满它的区域。
    CoverFill,
    /// 声音从哪张卡出来。
    AudioDevice,
}

impl Setting {
    pub const ALL: [Setting; 14] = [
        Self::Theme,
        Self::Quality,
        Self::PlaybackMode,
        Self::RefreshMs,
        Self::LyricOffsetMs,
        Self::PageSize,
        Self::CacheLimitMib,
        Self::BasicColor,
        Self::LyricPanel,
        Self::Sidebar,
        Self::LiteMode,
        Self::DownloadDir,
        Self::CoverFill,
        Self::AudioDevice,
    ];

    /// 左侧的名字。
    pub fn label(self) -> &'static str {
        match self {
            Self::Theme => "主题",
            Self::Quality => "音质",
            Self::PlaybackMode => "播放模式",
            Self::RefreshMs => "刷新间隔",
            Self::LyricOffsetMs => "歌词偏移",
            Self::PageSize => "每页条数",
            Self::CacheLimitMib => "缓存上限",
            Self::BasicColor => "16 色模式",
            Self::LyricPanel => "歌词面板",
            Self::Sidebar => "侧边导航",
            Self::LiteMode => "简易模式",
            Self::DownloadDir => "下载目录",
            Self::CoverFill => "封面铺满",
            Self::AudioDevice => "输出设备",
        }
    }

    /// 右侧的说明：讲清楚改了会怎样，而不是重复一遍当前值。
    pub fn description(self) -> &'static str {
        match self {
            Self::Theme => "配色方案，即时生效",
            Self::Quality => "越高越占缓存，切歌后生效",
            Self::PlaybackMode => "顺序 / 列表循环 / 单曲循环 / 随机",
            Self::RefreshMs => "越小越流畅，也越费 CPU",
            Self::LyricOffsetMs => "正值让歌词提前显示",
            Self::PageSize => "搜索与歌单一次取多少条",
            Self::CacheLimitMib => "超了自动删最旧的，0 为不限",
            Self::BasicColor => "老终端画不出真彩时打开",
            Self::LyricPanel => "右侧歌词栏（快捷键 l）",
            Self::Sidebar => "左侧标签导航（快捷键 \\）",
            Self::LiteMode => "关封面与频谱，省内存和 CPU",
            Self::DownloadDir => "下载单曲保存到这里（默认 ~/Music）",
            Self::CoverFill => "铺满 / 不变形 / 不裁剪，只能取两个",
            Self::AudioDevice => "声音送到哪张卡，没声音时先看这里",
        }
    }
}

/// 「系统默认」在候选列表里的显示名。
pub const DEFAULT_DEVICE_LABEL: &str = crate::audio::engine::DEFAULT_DEVICE_LABEL;

/// 设备名最长显示多少个字符。设备名可以很长（带一整串 USB 描述符），
/// 不截断会把右边的说明文字挤没。
const MAX_DEVICE_LABEL_CHARS: usize = 18;

/// 输出设备候选：首项是「系统默认」，其后是枚举到的设备名。
pub fn device_options(devices: &[String]) -> Vec<String> {
    let mut options = Vec::with_capacity(devices.len() + 1);
    options.push(DEFAULT_DEVICE_LABEL.to_string());
    options.extend(devices.iter().cloned());
    options
}

/// 在输出设备候选里按 `delta` 前进。
///
/// 外层 `None` 表示当前值不在候选里（设备被拔了、或配置文件手改过）——保持原样，
/// 不擅自跳到第一项；内层 `None` 表示选中的是「系统默认」。
pub fn cycle_device(
    options: &[String],
    current: Option<&str>,
    delta: isize,
) -> Option<Option<String>> {
    if options.is_empty() {
        return None;
    }
    let label = current.unwrap_or(DEFAULT_DEVICE_LABEL);
    let index = options.iter().position(|option| option == label)?;
    let len = options.len() as isize;
    let next = (index as isize + delta).rem_euclid(len) as usize;
    let picked = options.get(next)?.clone();
    Some((picked != DEFAULT_DEVICE_LABEL).then_some(picked))
}

/// 长设备名截短，末尾加省略号。按字符数而不是字节数，中文设备名不会被切坏。
pub fn trim_device_label(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    if chars.len() <= MAX_DEVICE_LABEL_CHARS {
        return name.to_string();
    }
    let head: String = chars[..MAX_DEVICE_LABEL_CHARS - 1].iter().collect();
    format!("{head}…")
}

/// 刷新间隔候选（毫秒）：从省电到流畅。
pub const REFRESH_MS_OPTIONS: [u64; 4] = [200, 100, 50, 33];

/// 每页条数候选。
pub const PAGE_SIZE_OPTIONS: [u32; 4] = [20, 30, 50, 100];

/// 缓存上限候选（MiB），`0` 表示不限制。
pub const CACHE_LIMIT_OPTIONS: [u64; 5] = [0, 256, 512, 1024, 2048];

/// 歌词偏移的步进与上下限（毫秒）。
pub const LYRIC_OFFSET_STEP: i64 = 100;
pub const LYRIC_OFFSET_LIMIT: i64 = 5_000;

/// 播放模式候选，顺序与 `r` 键轮转一致。
pub const PLAYBACK_MODES: [PlaybackMode; 4] = [
    PlaybackMode::Sequential,
    PlaybackMode::RepeatAll,
    PlaybackMode::RepeatOne,
    PlaybackMode::Shuffle,
];

/// 封面铺满方式候选。顺序即设置页里按 → 的轮转顺序，默认值排第一。
pub const COVER_FILLS: [CoverFill; 3] = [CoverFill::Crop, CoverFill::Stretch, CoverFill::Fit];

/// 单曲下载目录的预设选项。
///
/// **故意不开自由输入**：路径一旦写错（权限不足 / 不存在的目录 / 输入了错误的
/// 字符），下载会失败但用户可能想不到是路径的问题。限定在几个常见位置，
/// 既够用，又把出错范围卡死在已知选项里——`expand_user` 会自动把 `~/` 换成
/// `$HOME`，所以这几个路径在任何系统上都是有效的。
pub const DOWNLOAD_DIR_OPTIONS: [&str; 3] = ["~/Music", "~/Downloads", "~/Downloads/Music"];

/// 把音质档位翻成人话。
///
/// 服务端返回的就是 `128` / `flac` 这些原始值，直接显示用户不知道自己选了什么。
pub fn quality_label(quality: &str) -> String {
    match quality {
        "128" => "标准 128kbps".to_string(),
        "320" => "较高 320kbps".to_string(),
        "flac" => "无损 FLAC".to_string(),
        "high" => "高品".to_string(),
        "super" => "超高".to_string(),
        "viper_clear" => "蝰蛇母带".to_string(),
        "viper_atmos" => "蝰蛇全景声".to_string(),
        "viper_tape" => "蝰蛇磁带".to_string(),
        other => other.to_string(),
    }
}

/// 某项当前值的显示文本。
pub fn value_text(setting: Setting, state: &AppState) -> String {
    match setting {
        Setting::Theme => state.config.theme.label().to_string(),
        Setting::Quality => quality_label(&state.config.quality),
        Setting::PlaybackMode => state.config.playback_mode.label().to_string(),
        Setting::RefreshMs => format!(
            "{}ms（{} fps）",
            state.config.tick_ms,
            1000 / state.config.tick_ms.max(1)
        ),
        Setting::LyricOffsetMs => format!("{:+}ms", state.config.lyric_offset_ms),
        Setting::PageSize => format!("{} 条", state.config.page_size),
        Setting::CacheLimitMib => {
            if state.config.cache_limit_mib == 0 {
                "不限".to_string()
            } else {
                format!("{} MiB", state.config.cache_limit_mib)
            }
        }
        Setting::BasicColor => toggle_text(state.config.basic_color),
        Setting::LyricPanel => toggle_text(state.show_lyric_panel),
        Setting::Sidebar => toggle_text(state.sidebar_visible),
        Setting::LiteMode => toggle_text(state.config.lite_mode),
        Setting::DownloadDir => expand_download_dir(state.config.download_dir.as_deref()),
        Setting::CoverFill => state.config.cover_fill.label().to_string(),
        // 显示**实际打开**的那张卡，而不是配置里选的：选了但没打开（设备被拔、
        // 名字变了）时，用户看到的是真相，不是自己以为的选择。
        Setting::AudioDevice => {
            if state.output_device.is_empty() {
                "未打开".to_string()
            } else {
                trim_device_label(&state.output_device)
            }
        }
    }
}

/// 设置页要显示的全部值，顺序与 [`Setting::ALL`] 一致。
pub fn values(state: &AppState) -> Vec<String> {
    Setting::ALL
        .iter()
        .map(|setting| value_text(*setting, state))
        .collect()
}

/// 在候选列表里按 `delta` 前进，越界回绕。
///
/// 返回 `None` 表示当前值不在候选里（比如手改过配置文件），此时调用方保持原值。
pub fn cycle<T: PartialEq + Copy>(options: &[T], current: T, delta: isize) -> Option<T> {
    if options.is_empty() {
        return None;
    }
    let index = options.iter().position(|option| *option == current)?;
    let len = options.len() as isize;
    let next = (index as isize + delta).rem_euclid(len) as usize;
    options.get(next).copied()
}

/// 把 `~/foo` 展开成绝对路径。其它形式的输入原样返回。
///
/// 设为 None 时（配置文件里没填）按 `~/Music` 处理——这是默认下载目录，
/// 跟**新用户**第一次启动时不应该让程序坏在「路径不存在」上。
///
/// # 为什么不能只读 `HOME`
///
/// `HOME` 是 Unix 的约定，Windows 上通常**根本没有这个变量**（那边是
/// `USERPROFILE`）。只认它的话 `~/Music` 会原样留在字符串里，最后落成一个叫
/// `~` 的目录——下载看着「成功」，用户在自己以为的位置却找不到文件。
/// 所以先认 `HOME`（Unix 上的行为完全不变），拿不到再交给 [`dirs::home_dir`]：
/// 它在 Windows 上走 `SHGetKnownFolderPath`，是那边唯一可靠的做法。
pub fn expand_download_dir(value: Option<&str>) -> String {
    expand_with_home(value, home_dir().as_deref())
}

/// 用户主目录。`HOME` 优先（Unix 的约定），拿不到再问 `dirs`。
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(dirs::home_dir)
}

/// [`expand_download_dir`] 的纯函数版本：主目录由调用方给。
///
/// 抽出来是为了能直接测——测试是**并行**跑的，去改进程级的 `HOME` 会连带影响
/// 同一进程里其它测试，那是一种偶发失败。
///
/// `~/foo` 与 `~\foo` 都认：Windows 上用户很自然会用反斜杠。
/// `~user/foo` 这种跨用户的写法不处理（两个平台语义都不一样），原样返回，
/// 让下载器去报「路径不存在」，比在这里猜一个要好。
fn expand_with_home(value: Option<&str>, home: Option<&Path>) -> String {
    let raw = value.unwrap_or("~/Music");
    let suffix = raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\"));
    if let (Some(suffix), Some(home)) = (suffix, home) {
        // 用 `to_string_lossy` 而不是 `home.display()`：这里要的就是一个 `String`
        // （拼进 `format!` 之后还要被上层当字符串用），而 `display()` 给的是
        // `Display` 适配器，还得再转一道。
        return format!("{}/{}", home.to_string_lossy(), suffix);
    }
    raw.to_string()
}

/// 把歌名-歌手这种字符串清洗成可当文件名的形式。
///
/// 路径分隔符、控制字符、Windows 保留字符都替换成 `_`。其它字符原样保留——
/// 汉字、空格、常见标点都保留（用户期待的就是原文件名）。
pub fn sanitize_filename(label: &str) -> String {
    label
        .chars()
        .map(|ch| match ch {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0' => '_',
            ch if (ch as u32) < 0x20 => '_',
            other => other,
        })
        .collect()
}

/// 同上，但用于字符串候选（`cycle` 要求 `Copy`，`String` 不满足）。
pub fn cycle_str<'a>(options: &'a [&'a str], current: &str, delta: isize) -> Option<&'a str> {
    if options.is_empty() {
        return None;
    }
    let index = options.iter().position(|option| *option == current)?;
    let len = options.len() as isize;
    let next = (index as isize + delta).rem_euclid(len) as usize;
    options.get(next).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_setting_has_a_label_and_description() {
        for setting in Setting::ALL {
            assert!(!setting.label().is_empty());
            assert!(!setting.description().is_empty());
        }
    }

    #[test]
    fn cycle_wraps_around_in_both_directions() {
        let options = [1u8, 2, 3];
        assert_eq!(cycle(&options, 1, 1), Some(2));
        assert_eq!(cycle(&options, 3, 1), Some(1), "走到末尾要绕回开头");
        assert_eq!(cycle(&options, 1, -1), Some(3), "反向也要绕");
        assert_eq!(cycle(&options, 2, 0), Some(2));
    }

    #[test]
    fn cycle_returns_none_for_unknown_current_value() {
        // 配置文件被手改过、值不在候选里时，保持原样而不是跳到第一项
        assert_eq!(cycle(&[1u8, 2, 3], 9, 1), None);
        assert_eq!(cycle::<u8>(&[], 1, 1), None);
    }

    /// 输出设备候选：首项必须是「系统默认」，其余按枚举顺序跟在后面。
    #[test]
    fn device_options_put_system_default_first() {
        let devices = vec!["hw:0".to_string(), "hw:1".to_string()];
        assert_eq!(
            device_options(&devices),
            vec![
                "系统默认".to_string(),
                "hw:0".to_string(),
                "hw:1".to_string()
            ]
        );
        assert_eq!(device_options(&[]), vec!["系统默认".to_string()]);
    }

    /// 轮转顺序：系统默认 → 第一张卡 → 第二张卡 → 回到系统默认。
    /// 「系统默认」用内层 `None` 表示，和「配置里没填」是同一个状态。
    #[test]
    fn cycle_device_wraps_and_maps_default_to_none() {
        let options = device_options(&["hw:0".to_string(), "hw:1".to_string()]);

        assert_eq!(
            cycle_device(&options, None, 1),
            Some(Some("hw:0".to_string()))
        );
        assert_eq!(
            cycle_device(&options, Some("hw:0"), 1),
            Some(Some("hw:1".to_string()))
        );
        assert_eq!(cycle_device(&options, Some("hw:1"), 1), Some(None));
        assert_eq!(
            cycle_device(&options, None, -1),
            Some(Some("hw:1".to_string())),
            "反向也要绕"
        );
    }

    /// 设备被拔掉 / 配置文件手改过时，当前值不在候选里 → 保持原样，
    /// 不擅自跳到「系统默认」。
    #[test]
    fn cycle_device_keeps_values_outside_the_options() {
        let options = device_options(&["hw:0".to_string()]);
        assert_eq!(cycle_device(&options, Some("已经拔掉的那张卡"), 1), None);
        assert_eq!(cycle_device(&[], None, 1), None);
    }

    #[test]
    fn trim_device_label_shortens_long_names_only() {
        assert_eq!(trim_device_label("default"), "default");
        assert_eq!(trim_device_label("板载声卡"), "板载声卡");

        let long = "a".repeat(40);
        let trimmed = trim_device_label(&long);
        assert_eq!(trimmed.chars().count(), MAX_DEVICE_LABEL_CHARS);
        assert!(trimmed.ends_with('…'), "截短要有省略号");
    }

    #[test]
    fn playback_modes_follow_the_r_key_order() {
        // 设置页里的顺序要和按 r 轮转的顺序一致，否则用户会以为切错了
        for (index, mode) in PLAYBACK_MODES.iter().enumerate() {
            let next = PLAYBACK_MODES[(index + 1) % PLAYBACK_MODES.len()];
            assert_eq!(mode.next(), next, "{mode:?} 的下一档对不上");
        }
    }

    /// `~/foo` 展开、`None` 兜底成 `~/Music`——这是「首次启动还没填下载目录」
    /// 也能正常工作的基础。
    ///
    /// 走纯函数版本、显式传主目录：测试是并行跑的，去改进程级的 `HOME` 会连带
    /// 影响同进程的其它测试（原来就是这么写的，属于偶发失败的隐患）。
    #[test]
    fn expand_download_dir_handles_all_forms() {
        let home = Path::new("/home/tester");

        assert_eq!(
            expand_with_home(Some("~/Music"), Some(home)),
            "/home/tester/Music"
        );
        assert_eq!(
            expand_with_home(Some("~/Downloads/Music"), Some(home)),
            "/home/tester/Downloads/Music"
        );
        assert_eq!(
            expand_with_home(None, Some(home)),
            "/home/tester/Music",
            "None 兜底为 ~/Music，第一次启动不该让下载坏在路径上"
        );
        assert_eq!(
            expand_with_home(Some("/absolute/path"), Some(home)),
            "/absolute/path",
            "绝对路径原样"
        );
    }

    /// Windows 上用户会用反斜杠，`~\Music` 也得展开。
    #[test]
    fn expand_download_dir_accepts_backslash_separator() {
        let home = Path::new(r"C:\Users\tester");
        assert_eq!(
            expand_with_home(Some(r"~\Music"), Some(home)),
            r"C:\Users\tester/Music",
            "反斜杠写法不能原样漏出去，否则会落成一个叫 `~` 的目录"
        );
    }

    /// **拿不到主目录时宁可原样返回，也不能猜。**
    ///
    /// 这条锁的是 Windows 上的真实故障：`HOME` 不存在（那边是 `USERPROFILE`），
    /// 旧实现只认 `HOME`，于是 `~/Music` 原样留着，最后在缓存目录旁边落出一个
    /// 名字就叫 `~` 的文件夹。
    #[test]
    fn expand_download_dir_keeps_raw_when_home_is_unknown() {
        assert_eq!(
            expand_with_home(Some("~/Music"), None),
            "~/Music",
            "无从展开时原样返回，由调用方报错"
        );
        assert_eq!(
            expand_with_home(Some("~other/Music"), Some(Path::new("/home/tester"))),
            "~other/Music",
            "跨用户的写法不猜"
        );
    }

    /// 文件名清洗：去掉会让 OS 拒写或变成子目录的危险字符，
    /// 同时保留汉字、空格、常见标点——用户期待原文件名。
    #[test]
    fn sanitize_filename_strips_dangerous_chars() {
        // 用户实测反馈过："忘不掉的你 /" 这种带斜杠的歌名会直接创建子目录
        assert_eq!(sanitize_filename("歌手 - 歌名"), "歌手 - 歌名");
        assert_eq!(
            sanitize_filename("a/b\\c:d*e?f\"g<h>i|j"),
            "a_b_c_d_e_f_g_h_i_j",
            "所有路径分隔符与 Windows 保留字符"
        );
        assert_eq!(
            sanitize_filename("歌名\n换行\0结束"),
            "歌名_换行_结束",
            "控制字符也要拦"
        );
        assert_eq!(
            sanitize_filename("Hello, World! (Remix)"),
            "Hello, World! (Remix)",
            "空格与常见标点保留"
        );
    }
}
