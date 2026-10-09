//! 本地接口服务（KuGouMusicApi）的自动引导。
//!
//! # 为什么要有这个模块
//!
//! kugou-tui 自身不含任何音乐接口的实现：搜索、取链、歌词、云歌单全部走
//! KuGouMusicApi —— 一个第三方的 Node.js 服务。而 `cargo install` 只会把一个
//! 二进制放进 `~/.cargo/bin`，它既不携带那个服务，也没有 post-install 钩子可用。
//! 结果是「装完了却不能用」：用户还得自己 clone 仓库、装依赖、按平台起两个实例。
//!
//! 这个模块把剩下的那几步放进**运行时**：第一次启动时把服务准备好并拉起，之后
//! 每次启动先探端口，通了就直接复用。用户全程只敲 `kugou-tui`。
//!
//! # 边界：它做不到什么
//!
//! Node.js 运行时没法凭空变出来。机器上没有 node 时，这里能做的只是把「去装
//! Node.js」这句话讲清楚然后退出——不存在「纯 Rust 装完就能用」的版本，除非把
//! KuGouMusicApi 整套签名与加密逻辑在 Rust 里重写一遍。
//!
//! # 生命周期
//!
//! * 端口上**已经有**服务 → 复用，退出时**不碰它**。它可能是用户自己起的，也可能
//!   是另一个 kugou-tui 实例起的，杀掉会打断别人。
//! * 由本次启动拉起的 → 记在 [`Guard`] 里，进程正常退出时停止。刻意不做成常驻：
//!   一个终端播放器不该在用户关掉它之后还占着 node 进程。
//! * `--api-start` 是显式要求常驻的例外，走 [`detach`]。
//!
//! # 与 `scripts/kugou-api` 的关系
//!
//! 那份脚本做的是同一件事，但只有拿到源码仓库的人才有它——`cargo install` 的用户
//! 拿不到。所以逻辑在二进制里又实现了一遍，并刻意**沿用同样的路径约定**（PID 文件
//! `api-<name>.pid`、日志 `api-<name>.log`、实例名 standard / lite），两边不会互相打架。

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::error::{AppError, Result};
use crate::logger::{LEVEL_INFO, LEVEL_WARN, tlog};
use crate::source::SourceKind;

/// 接口服务的上游仓库。
///
/// 不是本项目的仓库：kugou-tui 只是它的一个客户端，接口实现全在那边。
const API_REPO: &str = "MakcRe/KuGouMusicApi";

/// 钉住的上游提交。
///
/// 与 `scripts/kugou-api-install` 里 `pinned_of kugou` 的值一致，改要两边一起改。
/// 为什么钉死：上游是活跃仓库，接口字段会漂移——跟 master 可能某天就解析不出歌名
/// 或歌词，而那种故障看起来像「本程序坏了」。
const API_REF: &str = "a5a98013cce79fe0ae2ad65fc84b68176ebcfc1e";

/// 归档下载地址。用 codeload 而不是 `/archive/`：后者会 302 跳到 codeload，
/// 多一次往返，也多一个可能失败的点。
fn archive_url() -> String {
    format!("https://codeload.github.com/{API_REPO}/tar.gz/{API_REF}")
}

/// 服务就绪的等待上限。
///
/// 冷启动要加载 express + axios + crypto-js 那一堆，实测 1-2 秒；给到 30 秒是为了
/// 覆盖「机械硬盘 + 首次加载」的情况。超时不代表失败，只是不再等——后面报错时会
/// 带上日志路径。
const READY_TIMEOUT: Duration = Duration::from_secs(30);
/// 单次探活的超时。本地回环，400ms 足够。
const PROBE_TIMEOUT: Duration = Duration::from_millis(400);
/// 下载归档的超时。1.8 MB，但在弱网下要留足裕度。
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(180);
/// `npm install` 的等待上限。超过这个时间基本可以判定网络挂住了，继续等只会让用户
/// 以为程序死了。npm 自己也有超时，这里是最后一道保险。
const INSTALL_TIMEOUT: Duration = Duration::from_secs(900);
/// 安装锁的最长等待。另一个实例正在装时，等着复用它的成果比自己再来一遍快。
const LOCK_WAIT: Duration = Duration::from_secs(180);
/// 锁文件超过这个时间没动过就视为陈旧（持有它的进程已经崩了）。
const LOCK_STALE: Duration = Duration::from_secs(15 * 60);

/// 一个服务实例。
struct Instance {
    /// 实例名，用于 PID 文件与日志文件名：`standard` / `lite`。
    name: &'static str,
    port: u16,
    pid: u32,
    child: Child,
    pid_file: PathBuf,
}

/// 本次进程拉起的实例集合。
///
/// 只在 [`shutdown`]（或进程正常退出）时停止**这里记下的**实例。
struct Guard {
    started: Vec<Instance>,
    /// 置为 true 后不再停止任何实例（`--api-start` 的常驻语义）。
    detached: bool,
}

impl Guard {
    fn new() -> Self {
        Self {
            started: Vec::new(),
            detached: false,
        }
    }

    fn track(&mut self, instance: Instance) {
        self.started.push(instance);
    }

    /// 停止本次拉起的全部实例。
    ///
    /// 先 TERM、再等一小会儿、最后 KILL：node 收到 TERM 会自己关掉监听，直接 KILL
    /// 会留下一个处于 TIME_WAIT 的端口，紧接着的重启会撞上「端口被占用」。
    fn stop_all(&mut self) {
        if self.detached {
            return;
        }
        for instance in self.started.drain(..) {
            let Instance {
                name,
                port,
                pid,
                mut child,
                pid_file,
            } = instance;
            let _ = child.kill();
            // kill 之后必须 wait，否则子进程变僵尸，PID 一直挂在进程表里
            let _ = child.wait();
            let _ = fs::remove_file(&pid_file);
            tlog!(
                LEVEL_INFO,
                "[bootstrap] 已停止本次拉起的接口服务 {}（PID {}，端口 {}）",
                name,
                pid,
                port
            );
        }
    }
}

static GUARD: OnceLock<Mutex<Guard>> = OnceLock::new();

fn guard() -> MutexGuard<'static, Guard> {
    GUARD
        .get_or_init(|| Mutex::new(Guard::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 停止本次拉起的全部实例。在 `main` 返回前调用。
///
/// 为什么不靠 `Drop`：`Cargo.toml` 里 `panic = "abort"`，panic 时析构函数根本不会跑，
/// 服务会变成孤儿。显式调用才是确定的。
pub fn shutdown() {
    guard().stop_all();
}

/// 标记为常驻：本次拉起的实例不再随进程退出而停止。
pub fn detach() {
    guard().detached = true;
}

/// 为当前音源准备好接口服务。
///
/// 返回一个简短的结果描述，供 `--api-start` 打印；正常启动时返回值的用语也适合
/// 直接丢给日志。
///
/// # 什么情况下会什么都不做
///
/// * `api_base` 指向的不是本机（远程服务不归我们管）；
/// * 该端口上已经有服务在响应（复用，不重复起）；
/// * 音源不是酷狗的两套平台（网易云是另一套服务，本模块不管）；
/// * 配置里关掉了自动拉起；
/// * 后端选了 `native`（**接口在进程内，没有服务可起**）。
pub fn prepare(config: &Config) -> Result<&'static str> {
    let kind = config.active_source_kind();

    // native 后端下酷狗接口是进程内函数调用，不存在「本机接口服务」这件事。
    // 这一步必须排在最前面：否则下面会去探测 3000/3001 端口，发现没有服务就
    // 尝试下载安装并拉起 Node——正是 `--api native` 要避免的。
    if config.api_backend.effective_for(kind) == crate::api::ApiBackend::Native {
        tlog!(
            LEVEL_INFO,
            "[bootstrap] 后端为 native，{} 的接口在进程内实现，跳过本地服务引导",
            kind.label()
        );
        return Ok("后端为 native，接口在进程内，无需本机服务");
    }

    let Some((name, platform)) = instance_of(kind) else {
        // 两种「不托管」的原因不同，话也要分开说：汽水是**本来就没有**本机服务
        // （它直连公网），说成「不托管」会让人以为要去别处准备一个。
        return Ok(if kind.is_remote() {
            "该音源直连公网，不需要本机接口服务"
        } else {
            "该音源不由本程序托管其接口服务"
        });
    };

    let Some((host, port)) = parse_endpoint(&config.api_base) else {
        return Err(AppError::Service(format!(
            "无法从接口地址「{}」解析出主机与端口。检查配置项 api_base。",
            config.api_base
        )));
    };
    if !is_local(&host) {
        // 接口指向别的机器时去拉起本地服务毫无意义，也不该动人家的端口
        return Ok("接口地址不在本机，跳过本地服务引导");
    }

    if listening(&host, port) {
        tlog!(
            LEVEL_INFO,
            "[bootstrap] {}:{} 已有服务在响应，直接复用",
            host,
            port
        );
        return Ok("复用已在运行的接口服务");
    }

    if !config.api_auto_start {
        return Err(AppError::Service(format!(
            "{host}:{port} 上没有接口服务，而配置里关闭了自动拉起（api_auto_start = false）。\n\
             请自行启动 KuGouMusicApi，或在配置文件里把 api_auto_start 改回 true。"
        )));
    }

    let dir = resolve_api_dir(config.api_dir.as_deref(), config.proxy.as_deref())?;
    let instance = spawn(&dir, &host, port, name, platform)?;
    guard().track(instance);
    Ok("已启动接口服务")
}

/// 切换音源后，确保目标音源的服务在跑。
///
/// 与 [`prepare`] 的区别：**不做下载与安装**。这里是在 TUI 已经画出来之后被调用的，
/// 一次 `npm install` 要几十秒，界面会僵住——而依赖目录是各平台共用的，只要启动时
/// 装过一次，切过去只需要 spawn 一个进程（一到两秒）。真的没装过时，返回的错误会
/// 告诉用户去跑一次 `--api-start`。
pub fn ensure_running(
    backend: crate::api::ApiBackend,
    kind: SourceKind,
    api_base: &str,
    api_dir: Option<&Path>,
) -> Result<()> {
    // native 后端切过去不需要服务（理由同 [`prepare`]）。
    if backend.effective_for(kind) == crate::api::ApiBackend::Native {
        return Ok(());
    }
    let Some((name, platform)) = instance_of(kind) else {
        return Ok(());
    };
    let Some((host, port)) = parse_endpoint(api_base) else {
        return Ok(());
    };
    if !is_local(&host) || listening(&host, port) {
        return Ok(());
    }

    let known = remembered_dir();
    let dir = match api_dir.or(known.as_deref()) {
        Some(dir) if dir.join("app.js").is_file() => dir.to_path_buf(),
        Some(dir) => {
            return Err(AppError::Service(format!(
                "「{}」里没有 app.js，不是有效的 KuGouMusicApi 目录。",
                dir.display()
            )));
        }
        None => locate_existing()?.ok_or_else(|| {
            AppError::Service(
                "本机还没有安装 KuGouMusicApi。先跑一次 `kugou-tui --api-start` 完成安装。"
                    .to_string(),
            )
        })?,
    };

    let instance = spawn(&dir, &host, port, name, platform)?;
    guard().track(instance);
    Ok(())
}

/// 停止 PID 文件里记录的全部实例（`--api-stop`）。
///
/// 这些实例可能是 `--api-start` 留下的常驻进程，也可能是上一次异常退出没来得及收的。
/// 只按 PID 文件动手——文件是本程序自己写的，不会牵连用户在别处起的服务。
pub fn stop_recorded() -> Result<usize> {
    let cache = crate::config::default_cache_dir();
    let entries = match fs::read_dir(&cache) {
        Ok(entries) => entries,
        // 目录还不存在 = 从没起过，不是错误
        Err(_) => return Ok(0),
    };

    let mut stopped = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let is_pid_file = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("api-") && name.ends_with(".pid"));
        if !is_pid_file {
            continue;
        }
        let Some(pid) = read_pid_file(&path) else {
            let _ = fs::remove_file(&path);
            continue;
        };
        if kill_pid(pid) {
            stopped += 1;
            tlog!(LEVEL_INFO, "[bootstrap] 已停止接口服务 PID {}", pid);
        }
        let _ = fs::remove_file(&path);
    }
    Ok(stopped)
}

// ======================================================================
// 实例与端点
// ======================================================================

/// 本机是否托管该音源的接口服务。
///
/// 面向「这个音源要不要准备服务？」这一问，`prepare` 与 `ensure_running`
/// 都据此跳过。单独给一个具名函数，是为了让别处（`source` 的测试）
/// 能直接断言这个约定，而不必把 `instance_of` 的返回结构也变成公开 API。
#[cfg(test)]
pub fn manages_service(kind: SourceKind) -> bool {
    instance_of(kind).is_some()
}

/// 音源 → （实例名, 服务端的 `platform` 参数）。
///
/// 酷狗的两个平台是两套独立的鉴权体系，平台由服务端启动参数决定，所以必须各跑一个
/// 进程、各占一个端口（详见 `source/mod.rs` 顶部的说明）。
fn instance_of(kind: SourceKind) -> Option<(&'static str, Option<&'static str>)> {
    match kind {
        SourceKind::Kugou => Some(("standard", None)),
        SourceKind::KugouConcept => Some(("lite", Some("lite"))),
        // 网易云走的是 NeteaseCloudMusicApi，另一套服务、另一套安装方式
        SourceKind::Netease => None,
        // 汽水直连公网（api.qishui.com），本机没有、也不该有它的服务进程。
        // 返回 `None` 会让 `prepare` 直接说「该音源不由本程序托管其接口服务」，
        // 也让 `ensure_running` 跳过——这正是我们要的：既不下载也不 spawn。
        SourceKind::Sodam => None,
    }
}

/// 从 `http://127.0.0.1:3000` 这类地址里取出 `(主机, 端口)`。
fn parse_endpoint(api_base: &str) -> Option<(String, u16)> {
    let trimmed = api_base.trim();
    let without_scheme = trimmed.split_once("://").map_or(trimmed, |(_, rest)| rest);
    let authority = without_scheme.split('/').next()?;
    match authority.rsplit_once(':') {
        Some((host, port)) => {
            // IPv6 在 URL 里带方括号，连接时要去掉
            let host = host.trim_matches(|c| c == '[' || c == ']');
            Some((host.to_string(), port.parse().ok()?))
        }
        None => {
            let default = if trimmed.starts_with("https") {
                443
            } else {
                80
            };
            Some((authority.to_string(), default))
        }
    }
}

/// 是否指向本机。只对回环地址自动拉起服务。
fn is_local(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

/// 该地址上是否已经有 HTTP 服务在响应。
///
/// 发一个最小的 `GET /` 而不是只建 TCP 连接：端口被别的程序占着时，纯连接探测会
/// 误判成「服务已就绪」，于是界面一路报错却查不到原因。读不到 `HTTP/` 开头也当作
/// 有人占着——宁可不去动它，也不要覆盖掉别人的端口。
fn listening(host: &str, port: u16) -> bool {
    let Ok(mut addrs) = (host, port).to_socket_addrs() else {
        return false;
    };
    let Some(addr) = addrs.next() else {
        return false;
    };
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, PROBE_TIMEOUT) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(PROBE_TIMEOUT));
    let request = format!("GET / HTTP/1.0\r\nHost: {host}:{port}\r\n\r\n");
    if stream.write_all(request.as_bytes()).is_err() {
        return true;
    }
    let mut head = [0u8; 8];
    match stream.read(&mut head) {
        Ok(read) => read >= 5,
        Err(_) => true,
    }
}

/// 等端口连通。只探 TCP：express 一旦 listen 就能连上，比等到业务接口可用快得多。
fn wait_ready(host: &str, port: u16, name: &str) -> Result<()> {
    let deadline = Instant::now() + READY_TIMEOUT;
    while Instant::now() < deadline {
        if listening(host, port) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Err(AppError::Service(format!(
        "服务进程已启动，但 {host}:{port} 在 {} 秒内没有开始响应。查看日志：{}",
        READY_TIMEOUT.as_secs(),
        crate::config::default_cache_dir()
            .join(format!("api-{name}.log"))
            .display()
    )))
}

// ======================================================================
// 定位 / 安装 KuGouMusicApi
// ======================================================================

/// 本次进程已经确定的接口服务目录。
static API_DIR: OnceLock<PathBuf> = OnceLock::new();

fn remember_dir(dir: &Path) {
    let _ = API_DIR.set(dir.to_path_buf());
}

fn remembered_dir() -> Option<PathBuf> {
    API_DIR.get().cloned()
}

/// 找出本机已有的 KuGouMusicApi 目录。
///
/// 顺序是有讲究的：**软件包提供的只读副本优先于用户目录**。前者自带依赖、装完即用，
/// 后者可能是一份自己 clone 的、还缺 `node_modules` 的源码。
fn locate_existing() -> Result<Option<PathBuf>> {
    for candidate in candidate_dirs(None) {
        if candidate.join("app.js").is_file() {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

/// 确定要用哪个目录，必要时下载安装。
fn resolve_api_dir(explicit: Option<&Path>, proxy: Option<&str>) -> Result<PathBuf> {
    // 用户显式指定了目录：它必须存在。悄悄换位置或往里面下载，都是自作主张。
    if let Some(dir) = explicit {
        if dir.join("app.js").is_file() {
            remember_dir(dir);
            return Ok(dir.to_path_buf());
        }
        return Err(AppError::Service(format!(
            "配置的接口服务目录「{}」里没有 app.js。检查配置项 api_dir 或环境变量 KUGOU_API_DIR。",
            dir.display()
        )));
    }

    if let Some(dir) = locate_existing()? {
        remember_dir(&dir);
        ensure_deps(&dir)?;
        return Ok(dir);
    }

    let dir = install_dir();
    eprintln!("首次运行：正在准备酷狗接口服务（只需一次）");
    download(&dir, proxy)?;
    ensure_deps(&dir)?;
    remember_dir(&dir);
    Ok(dir)
}

/// 候选目录，按优先级排列。
fn candidate_dirs(explicit: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(dir) = explicit {
        dirs.push(dir.to_path_buf());
    }
    // 发行包（AUR / deb）把服务连同生产依赖装在这里
    dirs.push(PathBuf::from("/usr/share/kugou-tui/api/kugou"));
    // 本程序自己安装的位置
    dirs.push(install_dir());
    // 与 scripts/kugou-api 的兜底保持一致：老用户按文档 clone 过的位置
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join("KuGouMusicApi"));
    }
    dirs
}

/// 本程序安装接口服务的默认位置。
fn install_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_else(std::env::temp_dir))
        .join("kugou-tui")
        .join("api")
        .join("kugou")
}

/// 确保依赖已安装。已有 `node_modules` 就直接返回。
fn ensure_deps(dir: &Path) -> Result<()> {
    if dir.join("node_modules").is_dir() {
        return Ok(());
    }
    // 系统目录是只读的、依赖由软件包带。缺了说明包坏了，让用户重装比往 /usr 里
    // 写更靠谱（往里写既装不进去，也不该做）。
    if dir.starts_with("/usr/share/kugou-tui") {
        return Err(AppError::Service(format!(
            "「{}」缺少 node_modules。这是软件包提供的位置，请重装 kugou-tui。",
            dir.display()
        )));
    }

    node_major()?;
    let _lock = acquire_install_lock(dir)?;
    // 拿到锁之后再看一次：等待期间可能已经被另一个实例装好了
    if dir.join("node_modules").is_dir() {
        return Ok(());
    }

    eprintln!("  安装依赖：npm install --omit=dev（首次需要几十秒）");
    run_npm(dir)?;
    Ok(())
}

/// 下载并解包上游归档。
///
/// 先落到一个临时目录、解包完再整体改名：中途失败不会留下一个「看起来装好了、其实
/// 只有一半」的目录，而那正是最难排查的一类故障。
fn download(dir: &Path, proxy: Option<&str>) -> Result<()> {
    let parent = dir
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .map_err(|error| AppError::io_at(parent.display().to_string(), error))?;

    let staging = parent.join(format!(".staging-{}-{}", &API_REF[..7], std::process::id()));
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging)
        .map_err(|error| AppError::io_at(staging.display().to_string(), error))?;

    let archive = staging.with_extension("tar.gz");
    let url = archive_url();
    eprintln!("  下载 KuGouMusicApi（{}）…", &API_REF[..7]);
    fetch(&url, &archive, proxy)?;
    unpack(&archive, &staging)?;
    let _ = fs::remove_file(&archive);

    if dir.exists() {
        fs::remove_dir_all(dir)
            .map_err(|error| AppError::io_at(dir.display().to_string(), error))?;
    }
    fs::rename(&staging, dir).map_err(|error| {
        AppError::Service(format!("无法把解包结果移动到 {}：{}", dir.display(), error))
    })?;
    Ok(())
}

/// 下载一个文件。
fn fetch(url: &str, dest: &Path, proxy: Option<&str>) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| AppError::Service(format!("创建下载用的运行时失败：{error}")))?;

    runtime.block_on(async {
        let mut builder = reqwest::Client::builder()
            .timeout(DOWNLOAD_TIMEOUT)
            .connect_timeout(Duration::from_secs(15))
            .user_agent(concat!("kugou-tui/", env!("CARGO_PKG_VERSION")));
        if let Some(proxy_url) = proxy.map(str::trim).filter(|url| !url.is_empty()) {
            let parsed = reqwest::Proxy::all(proxy_url)
                .map_err(|error| AppError::Config(format!("代理地址 {proxy_url} 无效：{error}")))?;
            builder = builder.proxy(parsed);
        }
        let client = builder
            .build()
            .map_err(|error| AppError::Service(format!("构造 HTTP 客户端失败：{error}")))?;

        let response = client
            .get(url)
            .send()
            .await
            .map_err(|error| {
                AppError::Service(format!(
                    "下载 {url} 失败：{error}。检查网络；使用代理时可在配置文件里设置 proxy。"
                ))
            })?
            .error_for_status()
            .map_err(|error| AppError::Service(format!("下载 {url} 失败：{error}")))?;

        let bytes = response
            .bytes()
            .await
            .map_err(|error| AppError::Service(format!("下载 {url} 时读取响应失败：{error}")))?;
        fs::write(dest, &bytes)
            .map_err(|error| AppError::io_at(dest.display().to_string(), error))?;
        Ok(())
    })
}

/// 解包 `.tar.gz`，跳过路径穿越的条目。
///
/// 归档来自第三方仓库，条目名不该被无条件信任：`../` 或绝对路径会写到目录外面去。
fn unpack(archive: &Path, dest: &Path) -> Result<()> {
    let file = File::open(archive)
        .map_err(|error| AppError::io_at(archive.display().to_string(), error))?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
    for entry in archive
        .entries()
        .map_err(|error| AppError::Service(format!("读取归档失败：{error}")))?
    {
        let mut entry =
            entry.map_err(|error| AppError::Service(format!("读取归档条目失败：{error}")))?;
        let path = entry
            .path()
            .map_err(|error| AppError::Service(format!("归档条目路径非法：{error}")))?
            .to_path_buf();
        // 剥掉顶层目录：GitHub 的归档一律是 `KuGouMusicApi-<sha>/` 包一层，而目录名
        // 带提交号——留着的话每次换钉住的提交，服务路径就会变一次。
        let relative: PathBuf = path.components().skip(1).collect();
        if relative.as_os_str().is_empty() {
            continue;
        }
        if relative
            .components()
            .any(|part| matches!(part, Component::ParentDir | Component::RootDir))
        {
            tlog!(
                LEVEL_WARN,
                "[bootstrap] 跳过可疑的归档条目 {}",
                path.display()
            );
            continue;
        }
        entry.unpack(dest.join(&relative)).map_err(|error| {
            AppError::Service(format!("解包 {} 失败：{error}", relative.display()))
        })?;
    }
    Ok(())
}

// ======================================================================
// 子进程
// ======================================================================

/// 启动一个服务实例，并等它就绪。
fn spawn(
    dir: &Path,
    host: &str,
    port: u16,
    name: &'static str,
    platform: Option<&str>,
) -> Result<Instance> {
    let cache = crate::config::default_cache_dir();
    fs::create_dir_all(&cache)
        .map_err(|error| AppError::io_at(cache.display().to_string(), error))?;
    let log_path = cache.join(format!("api-{name}.log"));
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|error| AppError::io_at(log_path.display().to_string(), error))?;

    eprintln!("  启动服务：http://{host}:{port}");
    let mut command = Command::new("node");
    command
        .current_dir(dir)
        .arg("app.js")
        .arg(format!("--port={port}"))
        // 同时给环境变量：服务读的是 PORT，命令行参数是为了让人 `ps` 一眼看出端口
        .env("PORT", port.to_string())
        .stdin(Stdio::null())
        .stdout(
            log.try_clone()
                .map_err(|error| AppError::io_at(log_path.display().to_string(), error))?,
        )
        .stderr(log);
    if let Some(platform) = platform {
        command.arg(format!("--platform={platform}"));
    }
    // 脱离终端会话：否则服务会跟着启动它的终端一起收到 SIGHUP，而用户很可能就是
    // 从那个终端里关掉 kugou-tui 的。
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS：Unix 上 setsid 的等价物
        command.creation_flags(0x00000008);
    }

    let child = command.spawn().map_err(|error| {
        AppError::Service(format!(
            "启动 node 失败：{error}。请确认已安装 Node.js（>= 12）且 node 在 PATH 里。"
        ))
    })?;
    let pid = child.id();

    let pid_file = cache.join(format!("api-{name}.pid"));
    // 记两个字段（PID 与端口），与 scripts/kugou-api 的格式一致：只存 PID 的话，
    // 改了端口重启之后会把「旧端口上的进程」误当成当前配置的服务。
    let _ = fs::write(&pid_file, format!("{pid} {port}"));
    tlog!(
        LEVEL_INFO,
        "[bootstrap] 已拉起接口服务 {}（PID {}，端口 {}，目录 {}）",
        name,
        pid,
        port,
        dir.display()
    );

    wait_ready(host, port, name)?;
    Ok(Instance {
        name,
        port,
        pid,
        child,
        pid_file,
    })
}

/// 读 PID 文件的第一个字段。
fn read_pid_file(path: &Path) -> Option<u32> {
    let content = fs::read_to_string(path).ok()?;
    // PowerShell 5.1 的 Set-Content 可能写出带 UTF-8 BOM 的文件。BOM 不是空白字符，
    // split_whitespace 去不掉，会让首字段变成 "\u{feff}1234" 而 parse 失败，所以先剥掉。
    content
        .trim_start_matches('\u{feff}')
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// 结束指定 PID 的进程。
fn kill_pid(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // 0 号信号只做存在性检查；这里直接发 TERM，让 node 有机会自己收尾
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) == 0 }
    }
    #[cfg(windows)]
    {
        Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }
}

/// 检查 node 是否可用。
fn node_major() -> Result<u32> {
    let output = Command::new("node")
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .map_err(|error| {
            AppError::Service(format!(
                "找不到 node（{error}）。KuGouMusicApi 是 Node.js 服务，请先安装 Node.js（>= 12）：\
                 https://nodejs.org ；装好之后重新运行 kugou-tui 即可。"
            ))
        })?;
    let text = String::from_utf8_lossy(&output.stdout);
    let major = text
        .trim()
        .trim_start_matches('v')
        .split('.')
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    if major < 12 {
        return Err(AppError::Service(format!(
            "node 版本过低（{}）。KuGouMusicApi 要求 >= 12，请升级 Node.js。",
            text.trim()
        )));
    }
    Ok(major)
}

/// 运行 `npm install --omit=dev`。
fn run_npm(dir: &Path) -> Result<()> {
    let mut command = npm_command();
    command
        .args(["install", "--omit=dev", "--no-audit", "--no-fund"])
        .current_dir(dir)
        .stdin(Stdio::null());
    let mut child = command.spawn().map_err(|error| {
        AppError::Service(format!(
            "无法执行 npm（{error}）。npm 通常随 Node.js 一起提供，请确认安装完整。"
        ))
    })?;

    // 自己轮询 `try_wait` 而不是 `status()`：后者会一直等到进程结束，而 npm 在断网时
    // 能卡住很久。这里给它一个上限，超时就杀掉并如实报错——用户看到「npm 超时」至少
    // 知道该去查网络，而不是对着一个没有输出的终端猜程序是不是死了。
    let deadline = Instant::now() + INSTALL_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() > deadline {
                    kill_npm_child(&mut child);
                    return Err(AppError::Service(format!(
                        "npm install 超过 {} 分钟仍未结束（多半是网络不通）。手动重试：cd \"{}\" && npm install --omit=dev",
                        INSTALL_TIMEOUT.as_secs() / 60,
                        dir.display()
                    )));
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            Err(error) => return Err(AppError::Service(format!("等待 npm 失败：{error}"))),
        }
    };

    if !status.success() {
        return Err(AppError::Service(format!(
            "npm install 失败（退出码 {}）。手动重试：cd \"{}\" && npm install --omit=dev",
            status.code().unwrap_or(-1),
            dir.display()
        )));
    }
    Ok(())
}

/// npm 命令。Windows 上的 npm 是 `npm.cmd`，`Command` 不会替你去解析它。
fn npm_command() -> Command {
    #[cfg(windows)]
    {
        let mut command = Command::new("cmd");
        command.args(["/C", "npm"]);
        command
    }
    #[cfg(not(windows))]
    {
        Command::new("npm")
    }
}

/// 结束 npm 子进程并回收它。
///
/// Windows 上子进程是 `cmd /C npm` 的 cmd.exe，`Child::kill` 只杀得掉 cmd.exe，
/// 真正的 node.exe（npm-cli.js）会留在后台继续装——所以走 `taskkill /T /F` 把
/// 整棵进程树一起收（与 [`kill_pid`] 同一做法）。超时后必须真的收干净，否则用户
/// 以为已经中断，后台却还在改 node_modules。
fn kill_npm_child(child: &mut std::process::Child) {
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &child.id().to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(not(windows))]
    {
        let _ = child.kill();
    }
    let _ = child.wait();
}

/// 安装锁：防止两个 kugou-tui 同时往同一个目录里 `npm install`。
///
/// 并发写同一个 `node_modules` 会留下一半的依赖，症状是服务起得来、一请求就报找不到
/// 模块——极其难查。锁只覆盖「安装」这一步，起服务本身是幂等的（端口探测挡着）。
fn acquire_install_lock(dir: &Path) -> Result<LockGuard> {
    let parent = dir.parent().unwrap_or(dir);
    fs::create_dir_all(parent)
        .map_err(|error| AppError::io_at(parent.display().to_string(), error))?;
    let path = parent.join(".install.lock");
    let deadline = Instant::now() + LOCK_WAIT;

    loop {
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                let _ = writeln!(file, "{}", std::process::id());
                return Ok(LockGuard { path });
            }
            Err(_) => {
                if lock_is_stale(&path) {
                    let _ = fs::remove_file(&path);
                    continue;
                }
                if Instant::now() > deadline {
                    return Err(AppError::Service(format!(
                        "另一个 kugou-tui 正在准备接口服务（锁文件 {}）。等它结束后重试；\
                         确认没有别的实例在跑时，删掉这个锁文件即可。",
                        path.display()
                    )));
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }
}

fn lock_is_stale(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    let Ok(modified) = metadata.modified() else {
        return false;
    };
    modified.elapsed().unwrap_or_default() > LOCK_STALE
}

/// 锁的持有标记，析构时释放。
struct LockGuard {
    path: PathBuf,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::ApiBackend;
    use crate::config::Config;

    /// native 后端下 `prepare` 必须**在探测端口之前**就返回。
    ///
    /// 这条是阶段 1 验收的核心之一：如果顺序反了，3000/3001 上没服务时它会去
    /// 下载安装并拉起 Node——正是 `--api native` 要杜绝的事。测试用的是
    /// `Config::default()`（api_base 指向 127.0.0.1:3000），本机此刻没有服务，
    /// 所以「先探测」的实现在这里会走到下载分支而失败/超时，能真正区分两种实现。
    #[test]
    fn native_backend_skips_service_bootstrap() {
        let config = Config {
            api_backend: ApiBackend::Native,
            // 就算顺序写错，也不让它真去下载
            api_auto_start: false,
            ..Config::default()
        };

        let outcome = prepare(&config).expect("native 下 prepare 不该失败");
        assert!(outcome.contains("native"), "实际：{outcome}");
    }

    /// `ensure_running` 在 native 下同样什么都不做（切音源时会走这里）。
    #[test]
    fn native_backend_skips_ensure_running() {
        let result = ensure_running(
            ApiBackend::Native,
            SourceKind::KugouConcept,
            "http://127.0.0.1:3001",
            None,
        );
        assert!(result.is_ok(), "native 下不该尝试拉起服务：{result:?}");
    }

    /// 非酷狗音源在 native 配置下仍按 node 处理：它们没有 native 实现，
    /// 但也不该被本模块拉起服务（网易云/汽水本来就不由本程序托管）。
    #[test]
    fn non_kugou_sources_are_not_managed_even_with_native() {
        for kind in [SourceKind::Netease, SourceKind::Sodam] {
            assert!(!manages_service(kind), "{} 不该被托管", kind.label());
            assert!(
                ensure_running(ApiBackend::Native, kind, "http://127.0.0.1:3002", None).is_ok()
            );
        }
    }

    /// PID 文件可能由 PowerShell 的 Set-Content 写出，5.1 下会带 UTF-8 BOM；
    /// 带不带 BOM 都必须能读出 PID，否则 `--api-stop` 会找不到自己拉起的服务。
    #[test]
    fn pid_file_parses_with_and_without_a_bom() {
        let path = std::env::temp_dir().join(format!("kugou-tui-pid-{}", std::process::id()));

        std::fs::write(&path, "4321 3002").expect("写临时 PID 文件");
        assert_eq!(read_pid_file(&path), Some(4321), "普通 PID 文件应能解析");

        std::fs::write(&path, "\u{feff}4321 3002").expect("写带 BOM 的临时 PID 文件");
        assert_eq!(
            read_pid_file(&path),
            Some(4321),
            "带 BOM 的 PID 文件也要能解析"
        );

        let _ = std::fs::remove_file(&path);
    }
}
