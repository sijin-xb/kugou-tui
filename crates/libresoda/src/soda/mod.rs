//! 汽水音乐（Soda Music / 汽水音乐）实现。
//!
//! 模块划分与上游 `music-lib/soda` 一一对应：
//!
//! | 上游文件 | 本 crate |
//! | --- | --- |
//! | `soda.go` | `mod.rs`、`types.rs`、`link.rs`、`quality.rs`、`track.rs` |
//! | `search.go` | `search.rs` |
//! | `song.go` | `song.rs` |
//! | `album.go` | `album.rs` |
//! | `playlist.go` | `playlist.rs` |
//! | `lyric.go` | `lyric.rs` |
//! | `account.go` | `account.rs` |
//! | `download.go` | `download.rs` |
//! | `user_playlist.go` | `user_playlist.rs` |
//! | `crypto.go` | `crypto.rs` |
//! | `login.go` | `login.rs`（进行中，见 `docs/PORTING.md`） |

pub mod account;
pub mod album;
pub mod artist;
pub mod browser;
#[cfg(feature = "cdp-signer")]
pub mod cdp_signer;
pub mod collection;
pub mod crypto;
pub mod download;
pub mod feed;
pub mod link;
pub mod lyric;
pub mod media_ref;
pub mod playback;
pub mod playlist;
pub mod playlist_edit;
pub mod qr_login;
pub mod quality;
pub mod search;
pub mod signature;
pub mod song;
pub mod stream;
pub mod track;
pub mod types;
pub mod user_playlist;

use crate::model::{Playlist, PlaylistCategory, QRLoginResult, Song};
use crate::util::Params;
use std::sync::{Arc, Mutex, OnceLock};

pub use types::DownloadInfo;

/// 汽水客户端。持有 Cookie（登录态）与 VIP 探测缓存。
// `Arc<dyn SignatureProvider>` 不实现 Debug，这里手写一份更安全的 Debug 输出
// （只暴露"是否有 Cookie / 是否已缓存 VIP / 签名提供者名字"，不打印凭据）。
pub struct Soda {
    cookie: Mutex<String>,
    is_vip_cache: Mutex<Option<bool>>,
    /// 音质档位偏好（客户端 gear key：`best`/`lossless`/`highest`/`medium`…），空 = 永远选最优。
    quality_preference: Mutex<String>,
    signature: Mutex<Option<Arc<dyn signature::SignatureProvider>>>,
    browser: Mutex<Option<Arc<dyn browser::BrowserRequester>>>,
    app_credentials: Mutex<Option<signature::AppCredentials>>,
}

impl std::fmt::Debug for Soda {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Soda")
            .field("has_cookie", &self.has_cookie())
            .field("vip_cached", &self.cached_vip())
            .field(
                "signature_provider",
                &self.signature_provider().map(|provider| provider.name()),
            )
            .field(
                "app_credentials",
                &self
                    .app_credentials()
                    .map(|credentials| {
                        format!(
                            "complete={} device_id.len={} x_helios.len={} x_medusa.len={}",
                            credentials.is_complete(),
                            credentials.device_id.len(),
                            credentials.x_helios.len(),
                            credentials.x_medusa.len()
                        )
                    })
                    .unwrap_or_else(|| "none".to_string()),
            )
            .finish()
    }
}

impl Default for Soda {
    fn default() -> Self {
        Self::new("")
    }
}

impl Soda {
    /// 创建实例；`cookie` 为空表示匿名（只能获取免费明文流）。
    pub fn new(cookie: impl Into<String>) -> Self {
        Self {
            cookie: Mutex::new(cookie.into()),
            is_vip_cache: Mutex::new(None),
            quality_preference: Mutex::new(String::new()),
            signature: Mutex::new(None),
            browser: Mutex::new(None),
            app_credentials: Mutex::new(None),
        }
    }

    /// 设置应用级签名凭证（`x-helios` / `x-medusa` + 设备指纹）。
    ///
    /// 不设置时 App 端点（`/luna/pc/track_v2`）只会返回空 body，VIP 曲目拿不到
    /// 整曲；抓取方式见 `docs/FULL-QUALITY-STREAM.md`，代码侧见
    /// [`signature::AppCredentials`]。
    pub fn set_app_credentials(&self, credentials: signature::AppCredentials) {
        if let Ok(mut slot) = self.app_credentials.lock() {
            *slot = Some(credentials.normalized());
        }
    }

    /// 从 JSON 文件加载应用签名凭证（等价 `set_app_credentials` + 读取文件）。
    pub fn load_app_credentials(&self, path: impl AsRef<std::path::Path>) -> crate::Result<()> {
        let credentials = signature::AppCredentials::from_file(path)?;
        self.set_app_credentials(credentials);
        Ok(())
    }

    /// 当前应用签名凭证（未设置时为 `None`）。
    pub fn app_credentials(&self) -> Option<signature::AppCredentials> {
        self.app_credentials
            .lock()
            .ok()
            .and_then(|slot| slot.clone())
    }

    /// 清空应用签名凭证。
    pub fn clear_app_credentials(&self) {
        if let Ok(mut slot) = self.app_credentials.lock() {
            *slot = None;
        }
    }

    /// 当前会附加到 App 端点请求上的应用签名头（诊断用，不含 Cookie）。
    pub fn app_signature_headers(&self) -> Vec<(String, String)> {
        pc_request_options(self)
            .into_iter()
            .flat_map(|option| option.headers().to_vec())
            .filter(|(name, _)| {
                name.eq_ignore_ascii_case("x-helios") || name.eq_ignore_ascii_case("x-medusa")
            })
            .collect()
    }

    /// 设置浏览器请求器（请求由跑着官方安全组件的页面发出，自带签名）。
    pub fn set_browser_requester(&self, requester: Arc<dyn browser::BrowserRequester>) {
        if let Ok(mut slot) = self.browser.lock() {
            *slot = Some(requester);
        }
    }

    /// 当前浏览器请求器（未设置时为 `None`，走本地 HTTP 直连）。
    pub fn browser_requester(&self) -> Option<Arc<dyn browser::BrowserRequester>> {
        self.browser.lock().ok().and_then(|slot| slot.clone())
    }

    /// 启用内置 CDP 签名页（Rust 直控 Chromium，不需要 Node）。
    ///
    /// 浏览器在第一次登录请求时才懒启动；每个二维码一个独立 `BrowserContext`。
    /// 没装 Chrome/Chromium 时会返回错误，可用 `QISHUI_CHROMIUM_PATH` 指定路径。
    #[cfg(feature = "cdp-signer")]
    pub fn enable_cdp_signer(&self) {
        self.set_browser_requester(Arc::new(cdp_signer::CdpSigner::new()));
    }

    /// 设置签名提供者（`msToken` / `a_bogus` / `bd-ticket-guard-*`）。
    ///
    /// 典型用法是 [`signature::CommandSignature`]：把签名交给能调用 mssdk 的
    /// 外部程序（Windows 桥接器、wine、`ssh win-box mssdk-bridge.exe`）。
    pub fn set_signature_provider(&self, provider: Arc<dyn signature::SignatureProvider>) {
        if let Ok(mut slot) = self.signature.lock() {
            *slot = Some(provider);
        }
    }

    /// 当前签名提供者（未设置时为 `None`，等价"不签名"）。
    pub fn signature_provider(&self) -> Option<Arc<dyn signature::SignatureProvider>> {
        self.signature.lock().ok().and_then(|slot| slot.clone())
    }

    /// 当前 Cookie（内部用 `Mutex` 持有，登录成功后可原地更新）。
    pub fn cookie(&self) -> String {
        self.cookie
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    /// 更新 Cookie（会清空 VIP 探测缓存，等价上游重新 `New` 的语义）。
    pub fn set_cookie(&self, cookie: impl Into<String>) {
        if let Ok(mut guard) = self.cookie.lock() {
            *guard = cookie.into();
        }
        if let Ok(mut cache) = self.is_vip_cache.lock() {
            *cache = None;
        }
    }

    pub(crate) fn has_cookie(&self) -> bool {
        !self.cookie().trim().is_empty()
    }

    pub(crate) fn cached_vip(&self) -> Option<bool> {
        self.is_vip_cache.lock().ok().and_then(|cache| *cache)
    }

    pub(crate) fn set_cached_vip(&self, value: bool) {
        if let Ok(mut cache) = self.is_vip_cache.lock() {
            *cache = Some(value);
        }
    }

    /// 设置音质档位偏好（客户端 gear key）：`best`/`auto`/空 = 永远选最优，
    /// `lossless` = 无损及以下，`highest` = 极高及以下，`medium` = 标准及以下。
    ///
    /// 取流时会优先选「不超过该档位」的最优流；该档位没有候选时回退到整体最优。
    pub fn set_quality_preference(&self, preference: impl Into<String>) {
        if let Ok(mut slot) = self.quality_preference.lock() {
            *slot = preference.into().trim().to_string();
        }
    }

    /// 当前音质档位偏好（空串表示"永远选最优"）。
    pub fn quality_preference(&self) -> String {
        self.quality_preference
            .lock()
            .map(|slot| slot.clone())
            .unwrap_or_default()
    }
}

/// 进程级默认实例（等价上游 `defaultSoda`）。
pub fn default_instance() -> &'static Soda {
    static INSTANCE: OnceLock<Soda> = OnceLock::new();
    INSTANCE.get_or_init(Soda::default)
}

/// 简写：等价上游包级函数使用的默认客户端。
pub fn soda() -> &'static Soda {
    default_instance()
}

/// PC App 端公共查询参数（等价 `sodaPCAppParams`）。
pub fn pc_app_params() -> Params {
    pc_app_params_with(None)
}

/// 带应用签名凭证的 PC 公共参数：凭证里的 `device_id` / `iid` / `fp` 会替换
/// 每次现生成的临时值。
///
/// 服务端会把「设备指纹与签名头是否匹配」一起校验，所以只要带了
/// `x-helios` / `x-medusa`，就必须同时用抓包时那台设备的指纹。
pub fn pc_app_params_with(credentials: Option<&signature::AppCredentials>) -> Params {
    let now = crate::util::now_millis();
    let fallback_device_id = now.to_string();
    let fallback_iid = (now + 1).to_string();
    let device_id = credentials
        .map(|value| value.device_id.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or(fallback_device_id);
    let iid = credentials
        .map(|value| value.iid.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or(fallback_iid);
    let fp = credentials
        .map(|value| value.fp_or_device_id())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| device_id.clone());

    let mut params = Params::new();
    params.set("aid", "386088");
    params.set("app_name", "luna_pc");
    params.set("region", "cn");
    params.set("geo_region", "cn");
    params.set("os_region", "cn");
    params.set("sim_region", "");
    params.set("device_id", device_id.clone());
    params.set("cdid", "");
    params.set("iid", iid);
    params.set("version_name", "3.3.0");
    params.set("version_code", "30030000");
    params.set("channel", "official");
    params.set("build_mode", "master");
    params.set("network_carrier", "");
    params.set("ac", "wifi");
    params.set("tz_name", "Asia/Shanghai");
    params.set("resolution", "");
    params.set("device_platform", "windows");
    params.set("device_type", "Windows");
    params.set("os_version", "Windows 11");
    params.set("fp", fp);
    params
}

/// `https://api.qishui.com/luna/pc/me`（等价 `sodaPCMeURL`）。
pub fn pc_me_url() -> String {
    format!(
        "https://api.qishui.com/luna/pc/me?{}",
        pc_app_params().encode()
    )
}

/// `https://api.qishui.com/luna/pc/track_v2`（等价 `sodaPCTrackV2URL`）。
pub fn pc_track_v2_url() -> String {
    pc_track_v2_url_with(None)
}

/// 带应用签名凭证的 `pc/track_v2` 地址。
pub fn pc_track_v2_url_with(credentials: Option<&signature::AppCredentials>) -> String {
    format!(
        "https://api.qishui.com/luna/pc/track_v2?{}",
        pc_app_params_with(credentials).encode()
    )
}

/// `https://api.qishui.com/luna/pc/user/playlist`（等价 `sodaPCUserPlaylistURL`）。
pub fn pc_user_playlist_url(user_id: &str, cursor: &str, count: i64) -> String {
    let count = if count <= 0 { 50 } else { count };
    let mut params = pc_app_params();
    params.set("user_id", user_id.trim());
    params.set("cursor", cursor.trim());
    params.set("count", count.to_string());
    format!(
        "https://api.qishui.com/luna/pc/user/playlist?{}",
        params.encode()
    )
}

/// `https://api.qishui.com/luna/pc/playlist/detail`（等价 `sodaPCPlaylistDetailURL`）。
pub fn pc_playlist_detail_url(playlist_id: &str, cursor: &str, count: i64) -> String {
    let count = if count <= 0 { 100 } else { count };
    let mut params = pc_app_params();
    params.set("playlist_id", playlist_id.trim());
    params.set("cursor", cursor.trim());
    params.set("count", count.to_string());
    format!(
        "https://api.qishui.com/luna/pc/playlist/detail?{}",
        params.encode()
    )
}

/// PC App 端点 GET 的通用实现（公共参数 + 额外查询参数）。
pub(crate) fn pc_get_json(
    soda: &Soda,
    path: &str,
    extra: &[(&str, String)],
) -> crate::Result<serde_json::Value> {
    let credentials = soda.app_credentials();
    let mut params = pc_app_params_with(credentials.as_ref());
    for (key, value) in extra {
        // 用 add：客户端把数组参数编码成重复 key（item_types=a&item_types=b）
        params.add(*key, value.clone());
    }
    let url = format!("https://api.qishui.com{path}?{}", params.encode());
    let raw = crate::http::get(&url, &pc_request_options(soda))?;
    if raw.is_empty() {
        return Err(crate::error::SodaError::http(format!(
            "soda {path} returned empty body（通常是缺应用级签名头，或设备指纹与签名器不一致）"
        )));
    }
    serde_json::from_slice(&raw).map_err(|err| {
        crate::error::SodaError::json(format!("soda {path} json decode error: {err}"))
    })
}

/// PC App 端点 POST 的通用实现：拼公共参数 → 补 `X-SS-STUB` → 交给签名提供者 → 发请求。
///
/// 汽水对 App 端点要求逐请求签名（`x-helios` / `x-medusa`），且签名覆盖
/// 「URL + 全部请求头 + body」，所以参数与请求头必须在签名之前全部就位。
///
/// 上游 `music-lib` 没有这层封装（它没实现写操作）；这是给自写客户端用的新增能力。
pub(crate) fn pc_post_json(
    soda: &Soda,
    path: &str,
    body: &serde_json::Value,
) -> crate::Result<serde_json::Value> {
    let credentials = soda.app_credentials();
    let body_bytes = serde_json::to_vec(body).map_err(|err| {
        crate::error::SodaError::json(format!("soda {path} json encode error: {err}"))
    })?;
    let mut options = pc_request_options(soda);
    options.push(
        crate::http::RequestOption::new()
            .header("Content-Type", "application/json; charset=utf-8")
            .header("X-SS-STUB", qr_login::md5_hex_upper(&body_bytes)),
    );
    let mut url = format!(
        "https://api.qishui.com{path}?{}",
        pc_app_params_with(credentials.as_ref()).encode()
    );
    let body_text = String::from_utf8_lossy(&body_bytes).to_string();
    if let Some(signed) = signature::apply_stream_signature(soda, &url, &body_text, &mut options) {
        url = signed;
    }
    let raw = crate::http::post_json(&url, &body_bytes, &options)?;
    if raw.is_empty() {
        return Err(crate::error::SodaError::http(format!(
            "soda {path} returned empty body（通常是缺应用级签名头，或设备指纹与签名器不一致）"
        )));
    }
    serde_json::from_slice(&raw).map_err(|err| {
        crate::error::SodaError::json(format!("soda {path} json decode error: {err}"))
    })
}

/// `pc/track_v2` 请求头（等价 `pcRequestOptions`）。
pub(crate) fn pc_request_options(soda: &Soda) -> Vec<crate::http::RequestOption> {
    let credentials = soda.app_credentials();
    let user_agent = credentials
        .as_ref()
        .map(|value| value.user_agent_or_default())
        .unwrap_or_else(|| types::PC_APP_USER_AGENT.to_string());
    let mut option = crate::http::RequestOption::new()
        .header("User-Agent", user_agent)
        .header("x-luna-background-type", "foreground")
        .header("x-luna-is-background-req", "0")
        .header("x-luna-is-local-user", "1");
    if let Some(credentials) = &credentials {
        for (name, value) in credentials.headers() {
            option = option.header(name, value);
        }
    }
    vec![option.cookie(&soda.cookie())]
}

// ---------------------------------------------------------------------------
// 包级函数：等价上游 `func Search(...)` / `func Parse(...)` 等，使用默认实例。
// ---------------------------------------------------------------------------

pub fn search(keyword: &str) -> crate::Result<Vec<Song>> {
    default_instance().search(keyword)
}

pub fn parse(link: &str) -> crate::Result<Song> {
    default_instance().parse(link)
}

pub fn search_album(keyword: &str) -> crate::Result<Vec<Playlist>> {
    default_instance().search_album(keyword)
}

pub fn get_album_songs(id: &str) -> crate::Result<Vec<Song>> {
    default_instance().get_album_songs(id)
}

pub fn parse_album(link: &str) -> crate::Result<(Playlist, Vec<Song>)> {
    default_instance().parse_album(link)
}

pub fn search_playlist(keyword: &str) -> crate::Result<Vec<Playlist>> {
    default_instance().search_playlist(keyword)
}

pub fn get_playlist_songs(id: &str) -> crate::Result<Vec<Song>> {
    default_instance().get_playlist_songs(id)
}

pub fn parse_playlist(link: &str) -> crate::Result<(Playlist, Vec<Song>)> {
    default_instance().parse_playlist(link)
}

pub fn get_recommended_playlists() -> crate::Result<Vec<Playlist>> {
    default_instance().get_recommended_playlists()
}

pub fn get_playlist_categories() -> crate::Result<Vec<PlaylistCategory>> {
    default_instance().get_playlist_categories()
}

pub fn get_category_playlists(
    category_id: &str,
    page: i64,
    limit: i64,
) -> crate::Result<Vec<Playlist>> {
    default_instance().get_category_playlists(category_id, page, limit)
}

pub fn get_user_playlists(page: i64, limit: i64) -> crate::Result<Vec<Playlist>> {
    default_instance().get_user_playlists(page, limit)
}

pub fn get_lyrics(song: &Song) -> crate::Result<String> {
    default_instance().get_lyrics(song)
}

pub fn is_vip_account() -> crate::Result<bool> {
    default_instance().is_vip_account()
}

pub fn get_download_info(song: &Song) -> crate::Result<DownloadInfo> {
    default_instance().get_download_info(song)
}

pub fn get_download_url(song: &Song) -> crate::Result<String> {
    default_instance().get_download_url(song)
}

pub fn download(song: &Song, output_path: &std::path::Path) -> crate::Result<()> {
    default_instance().download(song, output_path)
}

pub fn create_qr_login() -> crate::Result<qr_login::QrCreateResult> {
    qr_login::create_qr(default_instance())
}

pub fn check_qr_login(key: &str) -> crate::Result<QRLoginResult> {
    qr_login::check_qr(default_instance(), key)
}
