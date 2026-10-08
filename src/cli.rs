//! 命令行参数。
//!
//! 命令行 > 环境变量 > 配置文件 > 内置默认值，优先级由 [`crate::config::Config::merge_cli`]
//! 统一实现，避免优先级逻辑散落在各处。

use std::path::PathBuf;

use clap::Parser;

/// 酷狗音乐命令行 TUI 播放器。
///
/// 本机接口服务（KuGouMusicApi）会在首次运行时自动准备并拉起，无需手工安装；
/// 前提是本機已装 Node.js。想自己管服务时用 `--no-api-start`。
#[derive(Debug, Clone, Parser)]
#[command(
    name = "kugou-tui",
    version,
    about = "轻量级酷狗音乐 TUI 播放器",
    long_about = None,
)]
pub struct Cli {
    /// 接口服务地址（`--api node` 时使用）。
    #[arg(short = 'a', long, env = "KUGOU_API_BASE", value_name = "URL")]
    pub api_base: Option<String>,

    /// 用哪套后端实现酷狗接口。
    ///
    /// `node`（默认）走本机 KuGouMusicApi；`native` 用内嵌的纯 Rust 实现，
    /// 不需要 Node.js。只对酷狗两个音源生效，网易云与汽水照旧。
    #[arg(long, env = "KUGOU_API_BACKEND", value_name = "node|native")]
    pub api: Option<crate::api::ApiBackend>,

    /// 登录 cookie，形如 `token=xxx; userid=xxx; dfid=xxx`。
    #[arg(short = 'c', long, env = "KUGOU_COOKIE", value_name = "COOKIE")]
    pub cookie: Option<String>,

    /// 启动后立刻搜索该关键词。
    #[arg(short = 's', long, value_name = "KEYWORDS")]
    pub search: Option<String>,

    /// 初始音量，取值 0-100。
    #[arg(long, value_name = "0-100", value_parser = clap::value_parser!(u8).range(0..=100))]
    pub volume: Option<u8>,

    /// 音频缓存目录。
    #[arg(long, value_name = "DIR")]
    pub cache_dir: Option<PathBuf>,

    /// 音频缓存上限（MiB），0 表示不限制。
    #[arg(long, value_name = "MiB")]
    pub cache_limit: Option<u64>,

    /// 界面刷新间隔（毫秒），调大可进一步降低 CPU 占用。
    #[arg(long, value_name = "MS", value_parser = clap::value_parser!(u64).range(50..=5000))]
    pub tick_ms: Option<u64>,

    /// 搜索结果与歌单广场的每页条目数。
    ///
    /// 只影响这两处：歌单 / 榜单 / 歌手的具体歌曲一律取全（那些接口每页硬限 30，
    /// 客户端会自动翻页）。
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(5..=200))]
    pub page_size: Option<u32>,

    /// 访问 KuGouMusicApi 时使用的 HTTP 代理。
    #[arg(long, env = "KUGOU_PROXY", value_name = "URL")]
    pub proxy: Option<String>,

    /// 汽水音乐的登录 cookie，形如 `sessionid_ss=xxx; sessionid=yyy`。
    ///
    /// 汽水**只支持手填 cookie**，不支持扫码（见 `Capability` 的说明）。
    /// 留空则匿名使用：搜索、歌词、免费歌曲与试听片段都不受影响。
    #[arg(long, env = "SODAM_COOKIE", value_name = "COOKIE")]
    pub sodam_cookie: Option<String>,

    /// 汽水音乐的设备指纹 `device_id`（16 位数字）。
    ///
    /// 与签名头绑定：这里填的值必须与当初抓包时的设备一致，
    /// 否则整曲取流端点会返回空响应。
    #[arg(long, env = "SODAM_DEVICE_ID", value_name = "ID")]
    pub sodam_device_id: Option<String>,

    /// 汽水音乐的应用签名头 `x-helios`。
    ///
    /// **VIP 整曲与无损才需要它**：没有它时汽水的 App 端点会返回
    /// 「HTTP 200 + 空 body」，表现为 VIP 歌只能拿到 30 秒试听。
    /// 抓包方式见 `docs/USER_GUIDE.md` 的「汽水音乐音源」一节。
    #[arg(long, env = "SODAM_X_HELIOS", value_name = "VALUE")]
    pub sodam_x_helios: Option<String>,

    /// 汽水音乐的应用签名头 `x-medusa`（与 `--sodam-x-helios` 成对使用）。
    #[arg(long, env = "SODAM_X_MEDUSA", value_name = "VALUE")]
    pub sodam_x_medusa: Option<String>,

    /// 汽水音乐的 install id `iid`。
    #[arg(long, env = "SODAM_IID", value_name = "ID")]
    pub sodam_iid: Option<String>,

    /// 汽水应用签名服务地址（libmssdk 的 `/sign`）。
    ///
    /// 留空则用上游内置的**公共签名服务**（VIP 整曲开箱即用）；
    /// 填 `none` 显式关掉签名，只留试听与免费曲目。
    #[arg(long, env = "QISHUI_SIGNER_URL", value_name = "URL")]
    pub sodam_signer_url: Option<String>,

    /// 签名服务的 Bearer Token。用公共地址时留空即可（用内置的默认 token）。
    #[arg(long, env = "QISHUI_SIGNER_TOKEN", value_name = "TOKEN")]
    pub sodam_signer_token: Option<String>,
    /// 启动时使用固定色板（16 色），适配老终端。
    #[arg(long)]
    pub basic_color: bool,

    /// 启动时不注册系统托盘图标。配置文件里的 `tray` 也可关。
    #[arg(long)]
    pub no_tray: bool,

    /// 不自动拉起本机接口服务。
    ///
    /// 服务由你自己管理（跑在别的机器上、交给 systemd、或本来就常驻着）。端口上
    /// 没有服务时程序会直接报错，而不是去启动一个。
    #[arg(long, conflicts_with = "api_start")]
    pub no_api_start: bool,

    /// 只准备并启动本机接口服务，然后退出，不进入界面。
    ///
    /// 与默认行为不同：这里拉起的服务**留在后台**（默认行为是随本程序退出而停止），
    /// 之后每次启动直接复用，冷启动更快。停掉它用 `--api-stop`。
    ///
    /// 首次运行时会顺带完成下载与依赖安装——想提前把这一分钟花掉、之后再秒开，
    /// 就先跑一次这个。
    #[arg(long)]
    pub api_start: bool,

    /// 停止本程序拉起过的本机接口服务（`--api-start` 留下的那些）。
    #[arg(long)]
    pub api_stop: bool,

    /// 打印最终生效的配置、缓存目录与日志路径后退出。
    #[arg(long)]
    pub print_config: bool,
}
