//! kugou-tui —— 轻量级酷狗音乐命令行 TUI 播放器。
//!
//! # 模块划分
//!
//! ```text
//! cli / config       命令行参数与配置持久化
//! error              统一错误类型
//! logger             极简文件日志（不引入 tracing）
//! util               伪随机数与时间戳（不引入 rand/chrono）
//! event              全进程唯一的事件总线
//! keymap             按键 → 语义动作
//!
//! api/               酷狗接口封装（KuGouMusicApi 客户端）
//!   client.rs          HTTP 层
//!   model.rs           领域模型 + 防御性 JSON 解析
//!   catalog.rs         搜索 / 歌单 / 歌手 / 排行榜 / 播放直链
//!   lyric.rs           歌词获取与 LRC 解析
//!   cloud.rs           设备指纹 / 云端歌单增删
//!
//! audio/             音频子系统
//!   cache.rs           磁盘缓存与容量回收
//!   download.rs        直链下载（先落盘再播）
//!   engine.rs          独占线程的 rodio 播放引擎
//!
//! app/               编排层
//!   state.rs           纯数据状态
//!   queue.rs           播放队列与播放模式
//!   update.rs          事件 → 状态变更（唯一改状态的地方）
//!   mod.rs             App 装配与主循环
//!
//! ui/                终端界面
//!   theme.rs           配色
//!   widgets.rs         渲染原语
//!   views/             各面板
//! ```
//!
//! # 数据流
//!
//! ```text
//! 用户按键 ─┐
//! 音频事件 ─┼─▶ EventBus ─▶ App::handle_event ─▶ AppState ─▶ ui::render ─▶ 终端
//! 网络结果 ─┘                      │
//!                                 └─▶ runtime.spawn(...) ─▶ 新的网络请求
//! ```

mod api;
mod app;
mod audio;
mod bootstrap;
mod cli;
mod config;
mod error;
mod event;
mod keymap;
mod logger;
// MPRIS 与系统托盘都是 D-Bus 上的接口，只在 Unix 桌面上有意义；
// 非 Unix 平台整个模块（连同 zbus 依赖）都不参与编译，见 Cargo.toml 的说明。
#[cfg(unix)]
mod mpris;
mod source;
#[cfg(unix)]
mod tray;
mod ui;
mod util;
mod window;

use anyhow::Context;
use clap::Parser;

use crate::cli::Cli;
use crate::config::Config;
use crate::logger::tlog;

fn main() -> anyhow::Result<()> {
    // 分配器策略（glibc）：大块内存固定走 mmap。
    //
    // glibc 默认用**动态** mmap 阈值：一次大分配（封面解码、下载缓冲）会把阈值抬到
    // 它的大小，之后同样大的块改从 arena 里切——free 只是把页还进 arena，RSS 从此
    // 抬到历史峰值不回落，「听歌听多了破一百 MB」的病根之一。把阈值钉死，大块一律
    // mmap：munmap 即还 OS，不经过 arena，也就**不依赖 trim 的时机**。
    //
    // # 为什么是 256 KiB 而不是 1 MiB
    //
    // 原先钉的是 1 MiB，实测偏大：酷狗封面解码出来是 300–900 KB 的位图，**正好落在
    // 它下面**，仍然走 arena。`audio::engine::tests::mmap_threshold_effect_probe`
    // 拿「尺寸随轮次变化」的分配模式量过（固定尺寸测不出来——那总能复用同一批
    // chunk）：
    //
    // | 阈值 | 20 轮、不 trim 的 RSS |
    // |---|---|
    // | 1 MiB | +192 → +332 KiB，前 11 轮单调上升后停住 |
    // | 256 KiB | 恒定 +0 |
    //
    // 代价是 ≥256 KiB 的分配多几次 mmap/munmap 系统调用（每首歌个位数，微秒级），
    // 换来的是「free 即归还」，不受堆布局与 trim 时机影响。低于阈值的分配行为不变。
    //
    // 门控必须精确到 glibc：`mallopt` / `M_MMAP_THRESHOLD` 是 glibc 专有符号，
    // Darwin（macOS）同属 Unix 但 libc 里没有这两样，用 `cfg(unix)` 会直接编译失败。
    // Windows 的 MSVC 堆本来就积极归还，同样不需要。
    #[cfg(target_env = "gnu")]
    unsafe {
        // M_MMAP_THRESHOLD 的惯例写法是传 -3（glibc 的内部编号）
        libc::mallopt(libc::M_MMAP_THRESHOLD, 256 * 1024);
    }

    let cli = Cli::parse();

    // `--api-stop` 不看配置：它要停的是 PID 文件里记录的进程，读配置文件反而可能在
    // 配置损坏时连「停止」这条退路都用不上。
    if cli.api_stop {
        match bootstrap::stop_recorded() {
            Ok(0) => println!("没有正在运行的接口服务实例"),
            Ok(count) => println!("已停止 {count} 个接口服务实例"),
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(1);
            }
        }
        return Ok(());
    }

    // 优先级：命令行 > 环境变量（clap 直接读入 Cli）> 配置文件 > 默认值
    let mut config = Config::load();

    // 启动对齐：顶层 api_base / cookie / dfid 一律跟随选中的音源。
    //
    // 这三者是分别持久化的，历史上出现过「界面显示概念版、实际却在打标准版」的
    // 不一致——原因是某次用 `--api-base` 临时指向别处后被写回了顶层。以 `sources.active`
    // 为准统一一次即可；放在 merge_cli 之前，所以命令行的 --api-base 依旧能覆盖本次会话。
    // 【诊断】记录对齐前后的值，排查「启动时地址不对」。
    // 注意：必须在 logger::init 之后才能打日志，所以先记下来，初始化完再输出。
    let before = format!("{:?}", config.active_source_kind());
    let before_base = config.api_base.clone();

    let active = config.active_source_kind();
    config.switch_source(active);

    let after_base = config.api_base.clone();
    let after = format!("{:?}", config.active_source_kind());

    config.merge_cli(&cli);

    // 日志失败不阻塞使用，只在 stderr 提一句
    let log_path = Config::log_path();
    if let Err(error) = logger::init(&log_path) {
        eprintln!("警告：无法创建日志文件 {}：{error}", log_path.display());
    }

    // `--print-config` 是纯诊断：打印几行配置不该连带去下载一个接口服务。
    if cli.print_config {
        print_effective_config(&config);
        return Ok(());
    }

    // 接口服务的自动引导。
    //
    // **必须赶在 `redirect_stderr_to_log()` 之前**：那之后 stderr 就进了日志文件，
    // 而这里要给用户看的是进度（下载、装依赖、起服务）和失败原因——写进日志等于没说。
    // npm 子进程的输出同样继承 stderr，放在前面才能实时看到它在做什么。
    if let Err(error) = bootstrap::prepare(&config) {
        eprintln!("\n无法启动酷狗接口服务：\n  {error}");
        std::process::exit(1);
    }
    if cli.api_start {
        // 显式要求常驻：本次拉起的实例不随进程退出停止
        bootstrap::detach();
        println!("接口服务已就绪：{}", config.api_base);
        println!("停止它：kugou-tui --api-stop");
        return Ok(());
    }

    // 尽早把 stderr 接到日志文件上。
    //
    // **必须赶在 `App::new` 之前**：音频设备是在那里初始化的（`AudioHandle::spawn`
    // 起的线程），而 libjack / libasound 会直接往 fd 2 写报错。TUI 期间这些字符会
    // 打在 ratatui 画好的界面上，且**永远不会被增量重绘覆盖**（详见函数文档）。
    //
    // 时机不是小事：放在 `App::run()` 里试过，release 构建下音频线程跑得快，
    // 报错在到达那里之前就已经打出去了；debug 构建下反而"看起来生效"——
    // 纯粹是时序巧合。放在这里才是确定的。
    logger::redirect_stderr_to_log();

    tlog!(
        logger::LEVEL_INFO,
        "kugou-tui {} 启动，API={}，日志={}",
        env!("CARGO_PKG_VERSION"),
        config.api_base,
        log_path.display()
    );
    tlog!(
        logger::LEVEL_INFO,
        "[诊断] 音源对齐：{} ({}) → {} ({})，merge_cli 后 API={}",
        before_base,
        before,
        after_base,
        after,
        config.api_base
    );

    let mut app = app::App::new(config).context("初始化失败")?;

    // `--search` 让用户直接进入结果页，省一次按键
    if let Some(keyword) = cli
        .search
        .as_deref()
        .map(str::trim)
        .filter(|k| !k.is_empty())
    {
        app.startup_search(keyword);
    }

    let result = app.run();

    // 停掉本次拉起的接口服务。放在 `run()` 返回之后而不是靠析构：release 构建里
    // `panic = "abort"`，panic 时析构函数根本不会跑，服务会变成孤儿。
    bootstrap::shutdown();

    result
}

/// `--print-config`：把最终生效的配置打印出来，方便排查「为什么没读我的配置文件」。
fn print_effective_config(config: &Config) {
    let cache_limit = if config.cache_limit_mib == 0 {
        "不限".to_string()
    } else {
        format!("{} MiB", config.cache_limit_mib)
    };

    println!("kugou-tui {}", env!("CARGO_PKG_VERSION"));
    println!("配置文件  : {}", Config::path().display());
    println!("日志文件  : {}", Config::log_path().display());
    println!("API 地址  : {}", config.api_base);
    // 顺带提示服务端该配什么 `platform`：两个平台的鉴权不通用，配错了会退化成试听
    let kind = config.active_source_kind();
    println!(
        "当前音源  : {}{}",
        kind.label(),
        match kind.platform_env() {
            Some(value) => format!("（服务端需 platform={value}）"),
            // 汽水直连公网，没有「服务端」这回事；写成「服务端不设 platform」
            // 会让人以为本机该有个服务在跑。
            None if kind.is_remote() => "（直连公网，无需本地服务）".to_string(),
            None => "（服务端不设 platform）".to_string(),
        }
    );
    println!(
        "登录状态  : {}",
        if config.is_logged_in() {
            "已登录"
        } else {
            "未登录（云端歌单不可用）"
        }
    );

    // 汽水的签名凭证单独说清：它决定「VIP 整曲 / 无损能不能拿到」，
    // 而现象是「只有 30 秒试听」——不说清的话用户很难联想到要配这个。
    if kind == crate::source::SourceKind::Sodam {
        let app = &config.sources.sodam_app;
        println!(
            "签名凭证  : {}",
            if app.is_complete() {
                "完整（可取整曲）"
            } else if app.has_device_fingerprint() {
                "缺 x-helios / x-medusa（VIP 整曲与无音乐会退化成试听片段）"
            } else {
                "未配置（仅试听片段与免费音质）"
            }
        );
    }
    println!(
        "设备指纹  : {}",
        config.dfid.as_deref().unwrap_or("（尚未获取）")
    );
    println!("音质      : {}", config.quality);
    println!("音量      : {:.0}%", config.volume * 100.0);
    println!("播放模式  : {}", config.playback_mode.label());
    println!("缓存目录  : {}", config.cache_dir.display());
    println!("缓存上限  : {cache_limit}");
    println!("刷新间隔  : {} ms", config.tick_ms);
    println!("每页条目  : {}", config.page_size);
    println!(
        "代理      : {}",
        config.proxy.as_deref().unwrap_or("（未设置）")
    );
    println!(
        "自动拉起  : {}",
        if config.api_auto_start {
            "是"
        } else {
            "否（接口服务需自己启动）"
        }
    );
    println!(
        "服务目录  : {}",
        config
            .api_dir
            .as_deref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "（自动查找）".to_string())
    );
    println!(
        "16 色模式 : {}",
        if config.basic_color { "是" } else { "否" }
    );
    // 托盘与 MPRIS 都是 D-Bus 接口，非 Unix 平台上根本不会编译进去，
    // 这里如实说明，免得用户对着配置项怀疑「开了怎么没反应」。
    #[cfg(unix)]
    println!("系统托盘 : {}", if config.tray { "启用" } else { "关闭" });
    #[cfg(not(unix))]
    println!("系统托盘 : 不可用（{} 无 D-Bus）", std::env::consts::OS);
}
