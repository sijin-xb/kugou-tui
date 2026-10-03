//! 汽水音乐（Soda Music）音源。
//!
//! # 与其它音源的根本差别：没有本地接口服务
//!
//! 酷狗与网易云都是**本机跑一个 Node 服务**，客户端只跟 `127.0.0.1` 说话。
//! 汽水不一样——它直接打**公网**（`api.qishui.com` / `beta-luna.douyin.com`），
//! 解析与取链都在本模块里实现。因此：
//!
//! * `bootstrap` 不为它准备任何服务（[`crate::bootstrap::instance_of`] 返回 `None`）；
//! * 它的 `api_base` 是公网域名，配置里可改（换 CDN 或自建反代都用得上）；
//! * 断网即不可用，与另两个音源的「本地服务挂了」是两回事。
//!
//! # 第二个差别：音频是加密的
//!
//! 汽水下发的流是 MP4/CENC 加密流，密钥藏在 `play_auth` 里。
//! 这意味着**取链不能只返回一个 URL**——必须把「整首下载 + 解密 + 落盘」
//! 做完，才能交给播放器（见 [`crypto`]）。另两个音源给的是明文直链，
//! 丢给下载器边下边播即可。
//!
//! 代价是汽水的歌**不能边下边播**（得整首下完才能解），首播等待比另两个音源长。
//! 这是加密方案决定的，不是实现取巧。
//!
//! # 会员与试听
//!
//! 汽水的取流分两层（实测结论，与 libresoda 一致）：
//!
//! | 层 | 端点 | 能拿到什么 |
//! |---|---|---|
//! | Web / 分享页 | `GET /luna/pc/track_v2`（`device_platform=web`）、`/luna/h5/seo_track` | **只有 30/60 秒试听**，不需要签名 |
//! | App | `POST /luna/pc/track_v2`（带 `x-helios` / `x-medusa`） | **整曲**，缺签名头时返回 HTTP 200 + **空 body** |
//!
//! 所以「VIP 歌只能听到 30 秒」的根因**不是**会员判定，而是缺应用级签名头。
//! 本模块的实现顺序正是按这个结论排的：先试 App 端点（能拿整曲），
//! 失败再退到 Web 端点（拿试听），并把原因如实透出。
//!
//! 那个「HTTP 200 + 空 body」尤其要小心：它和「接口下线」在状态码上完全一样。
//! [`fetch_json`] 把空 body 单独识别成一种错误，不会让它伪装成成功。

pub mod client;
pub mod crypto;
pub mod types;

use serde_json::Value;

use crate::api::client::ApiClient;
use crate::api::model::{Lyric, Song};
use crate::error::{AppError, Result};
use crate::logger::{LEVEL_DEBUG, LEVEL_INFO, LEVEL_WARN, tlog};

pub use client::SodaClient;
use client::{ANDROID_SEARCH_USER_AGENT, WEB_USER_AGENT, encode_query, pc_app_params};
use types::{StreamCandidate, Track};

/// Android 搜索网关。搜索走这里而不是 PC 端点。
const ANDROID_API_BASE: &str = "https://api.qishui.com/luna";
/// H5 分享页兜底端点。PC 端点查不到时用它。
const SEO_BASE: &str = "https://beta-luna.douyin.com/luna/h5/seo_track";
/// PC 端点。
const PC_BASE: &str = "https://api.qishui.com";

/// 搜索结果里每页取多少条。
const SEARCH_PAGE_SIZE: u32 = 20;

// ============================================================================
// 客户端构造
// ============================================================================

/// 从分派层拿到的 `ApiClient` 里取出汽水的配置，重建一个专用客户端。
///
/// # 为什么不复用 `ApiClient` 本身
///
/// 见 [`client`] 模块的说明：现有客户端只发 GET、只能往 `{base}{path}` 拼、
/// 且只带 cookie 一个身份，而汽水三条都不满足。`ApiClient` 在这里只被当作
/// 「汽水配置项的载体」——`base` 是公网地址，`cookie` 是登录态。
///
/// 应用签名凭证从进程级槽位取（见 [`client::active_credentials`]），
/// 它由 `client_for()` 在造客户端时写入。
pub fn client_of(api: &ApiClient) -> Result<SodaClient> {
    SodaClient::new(
        api.cookie().map(str::to_string),
        client::active_credentials(),
        api.proxy(),
    )
}

// ============================================================================
// 底层请求
// ============================================================================

/// 发请求拿 JSON，并校验业务错误码。
///
/// # 空 body 为什么要单独识别
///
/// 汽水对「缺少应用级签名头」的请求返回的是 **HTTP 200 + 0 字节**。
/// 光看状态码会以为成功，然后解析 JSON 时报一句「expected value」，
/// 完全指不出真正的原因。这里把空 body 提前拦下来，给出「缺签名头」或
/// 「签名可能已过期」两种明确提示——它们对应完全不同的处置动作
/// （去配置 vs 重新抓包）。
async fn fetch_json(
    client: &SodaClient,
    url: &str,
    user_agent: &str,
    headers: &[(&str, &str)],
    what: &str,
    body: Option<&[u8]>,
) -> Result<Value> {
    let response = fetch_bytes(client, url, user_agent, headers, body, what).await?;
    if response.is_empty() {
        return Err(empty_body_error(client, what));
    }
    serde_json::from_slice::<Value>(&response).map_err(|error| {
        let preview: String = String::from_utf8_lossy(&response)
            .chars()
            .take(120)
            .collect();
        tlog!(LEVEL_WARN, "汽水 {what} 响应解析失败：{error}");
        AppError::NonJsonBody {
            path: what.to_string(),
            status: 200,
            preview,
        }
    })
}

/// 空 body 的错误：区分「没配签名」与「签名过期」，两者处置方式不同。
fn empty_body_error(client: &SodaClient, what: &str) -> AppError {
    if client.credentials().is_complete() {
        AppError::Other(format!(
            "{what} 返回空响应：应用签名凭证可能已过期，请重新抓包更新 x-helios / x-medusa"
        ))
    } else {
        AppError::Other(format!(
            "{what} 返回空响应：缺少应用级签名头（x-helios / x-medusa），VIP 整曲取不到；\
             免费歌曲与试听片段不受影响"
        ))
    }
}

/// 发请求拿原始字节。
async fn fetch_bytes(
    client: &SodaClient,
    url: &str,
    user_agent: &str,
    headers: &[(&str, &str)],
    body: Option<&[u8]>,
    what: &str,
) -> Result<Vec<u8>> {
    let result = match body {
        Some(body) => {
            // 应用签名覆盖 body，所以 body 的 MD5 要和 body 一起算、一起发。
            // 用局部 `String` 而不是 `Box::leak`：后者每请求泄漏一次 header 文本，
            // 长时间运行 + 反复切歌会稳定涨内存。
            let stub = client::md5_hex_upper(body);
            let mut with_stub: Vec<(&str, &str)> = headers.to_vec();
            with_stub.push(("X-SS-STUB", stub.as_str()));
            client.post_bytes(url, user_agent, &with_stub, body).await
        }
        None => client.get_bytes(url, user_agent, headers).await,
    };
    result.map_err(|error| {
        tlog!(LEVEL_DEBUG, "汽水接口 {what} 失败：{error}");
        error
    })
}

// ============================================================================
// 搜索
// ============================================================================

/// 单曲搜索。
///
/// 走 Android 搜索网关 `/search/track`。这个网关**不需要签名**，
/// 因此没配凭证也能搜——搜索是本音源唯一完全免费的完整能力。
pub async fn search_songs(
    client: &SodaClient,
    keyword: &str,
    page: u32,
    page_size: u32,
) -> Result<Vec<Song>> {
    let count = if page_size == 0 {
        SEARCH_PAGE_SIZE
    } else {
        page_size
    };
    let cursor = page.saturating_sub(1).saturating_mul(count);

    let url = format!(
        "{ANDROID_API_BASE}/search/track?{}",
        encode_query(&android_search_params(keyword, cursor, count))
    );

    let root = fetch_json(
        client,
        &url,
        ANDROID_SEARCH_USER_AGENT,
        &[],
        "汽水搜索",
        None,
    )
    .await?;
    check_status(&root, "汽水搜索")?;

    let tracks = parse_search_tracks(&root);
    Ok(tracks.into_iter().map(|track| track.to_song()).collect())
}

/// Android 搜索网关的参数集。
///
/// 这批参数是「客户端身份」，网关会据此决定返回什么形态的结果。
/// 与 libresoda 一致：固定成一套能工作的值，只把 `q`/`cursor`/`count` 动态化。
fn android_search_params(keyword: &str, cursor: u32, count: u32) -> Vec<(&'static str, String)> {
    vec![
        ("device_platform", "android".to_string()),
        ("os", "android".to_string()),
        ("ssmix", "a".to_string()),
        ("channel", "xiaomi_8478_64".to_string()),
        ("aid", "8478".to_string()),
        ("app_name", "luna".to_string()),
        ("version_code", "100198030".to_string()),
        ("version_name", "19.8.0".to_string()),
        ("manifest_version_code", "100198030".to_string()),
        ("resolution", "1080*1920".to_string()),
        ("dpi", "480".to_string()),
        ("device_type", "ABR-AL80".to_string()),
        ("device_brand", "HUAWEI".to_string()),
        ("language", "zh".to_string()),
        ("os_api", "35".to_string()),
        ("os_version", "15".to_string()),
        ("ac", "wifi".to_string()),
        ("device_model", "ABR-AL80".to_string()),
        ("package", "com.luna.music".to_string()),
        ("luna_apk_type", "normal_apk".to_string()),
        ("charge", "0".to_string()),
        ("is_teen_mode", "0".to_string()),
        ("sim_region", "cn".to_string()),
        ("tz_name", "Asia/Shanghai".to_string()),
        ("tz_offset", "28800".to_string()),
        ("iid", "2204957404569386".to_string()),
        ("device_id", "2204957404565290".to_string()),
        ("aid", "386088".to_string()),
        ("q", keyword.to_string()),
        ("cursor", cursor.to_string()),
        ("count", count.to_string()),
        ("_rticket", crate::util::now_unix_millis().to_string()),
    ]
}

/// 从综合搜索响应里取出所有曲目并按 id 去重。
///
/// 搜索响应是「多个结果分组」的形状：每组各有自己的 `data[]`，
/// 同一个 id 可能在多组里出现（单曲组与「相关推荐」组常常重复）。
fn parse_search_tracks(root: &Value) -> Vec<Track> {
    let mut tracks: Vec<Track> = Vec::new();
    let mut seen: Vec<String> = Vec::new();

    let groups = match root.get("result_groups").and_then(Value::as_array) {
        Some(groups) => groups,
        // 兜底：整体扫描，找任何能解出曲目的对象数组
        None => return Vec::new(),
    };

    for group in groups {
        let Some(items) = group.get("data").and_then(Value::as_array) else {
            continue;
        };
        for item in items {
            // 每条可能是 { entity: { track: {...} } }，也可能直接就是 track
            let candidate = item
                .get("entity")
                .and_then(|entity| entity.get("track"))
                .unwrap_or(item);
            let Some(track) = Track::from_json(candidate) else {
                continue;
            };
            if seen.contains(&track.id) {
                continue;
            }
            seen.push(track.id.clone());
            tracks.push(track);
        }
    }
    tracks
}

// ============================================================================
// 歌曲详情
// ============================================================================

/// 一次详情请求的产物。
///
/// # 为什么把曲目与候选流放在一起
///
/// 二者来自**同一个响应**：App / Web 端点的 `track_v2` 里既有曲目主体，
/// 也有 `track_player`（播放流信息）。合成一个返回值，取链时就不必
/// 「查一次详情、再查一次取流」——那会把 App 端点的 POST 打两次，
/// 而那个端点对频率敏感（见 [`client`] 的限流说明）。
struct TrackResponse {
    /// 曲目主体（时长、会员标记、封面…）。
    track: Track,
    /// 响应里内嵌的候选流。
    candidates: Vec<StreamCandidate>,
    /// 需要**再发一次请求**才拿得到内容的取流信息地址。
    ///
    /// 不在这里就地请求：取歌词/取封面只需要 `track`，把 `url_player_info`
    /// 也跟着请求一遍属于白白多打一次接口。
    follow_up: Vec<String>,
}

/// 取单曲详情（只要曲目主体，不追 `url_player_info`）。
///
/// 三级降级：App 端点 → Web 端点 → H5 分享页。逐级放宽，代价是能拿到的
/// 字段越来越少（App 端点最全，分享页最少），但至少不会「查不到这首歌」。
pub async fn fetch_track(client: &SodaClient, track_id: &str) -> Result<Track> {
    fetch_track_response(client, track_id)
        .await
        .map(|response| response.track)
}

/// 同 [`fetch_track`]，但连同候选流与待跟进地址一起返回（取链路径用）。
async fn fetch_track_response(client: &SodaClient, track_id: &str) -> Result<TrackResponse> {
    let mut last_error = None;

    // App 端点要登录态（否则回的是「未登录」而不是曲目），没 cookie 就别试了——
    // 白白多打一次请求，还会把后面 Web 端点的成功率统计搅浑。
    if client.credentials().has_device_fingerprint() && client.has_cookie() {
        match fetch_pc_track(client, track_id).await {
            Ok(response) => return Ok(response),
            Err(error) => {
                tlog!(LEVEL_DEBUG, "汽水 App 端点详情失败，退到 Web 端点：{error}");
                last_error = Some(error);
            }
        }
    }

    match fetch_web_track(client, track_id).await {
        Ok(response) => Ok(response),
        Err(web_error) => {
            tlog!(
                LEVEL_DEBUG,
                "汽水 Web 端点详情失败，退到分享页：{web_error}"
            );
            match fetch_seo_track(client, track_id).await {
                Ok(response) => Ok(response),
                // 三级都失败时，报**第一级**的错：那是信息最全的一条
                // （App 端点的「缺签名头」比分享页的「查不到」有用得多）。
                Err(seo_error) => Err(last_error.unwrap_or(seo_error)),
            }
        }
    }
}

/// `GET /luna/pc/track_v2`（`device_platform=web`）。免签名，但只有试听流。
async fn fetch_web_track(client: &SodaClient, track_id: &str) -> Result<TrackResponse> {
    let url = format!(
        "{PC_BASE}/luna/pc/track_v2?{}",
        encode_query(&[
            ("track_id", track_id.to_string()),
            ("media_type", "track".to_string()),
            ("aid", "386088".to_string()),
            ("device_platform", "web".to_string()),
            ("channel", "pc_web".to_string()),
        ])
    );
    let root = fetch_json(client, &url, WEB_USER_AGENT, &[], "汽水单曲详情", None).await?;
    check_status(&root, "汽水单曲详情")?;
    build_track_response(&root, track_id, "web")
}

/// `POST /luna/pc/track_v2`。**整曲流走这里**，需要应用签名头。
async fn fetch_pc_track(client: &SodaClient, track_id: &str) -> Result<TrackResponse> {
    let payload = serde_json::json!({
        "track_id": track_id,
        "media_type": "track",
        "queue_type": "favorite_track_playlist",
        "scene_name": "library",
    });
    let body = serde_json::to_vec(&payload)
        .map_err(|error| AppError::Other(format!("汽水请求体序列化失败：{error}")))?;

    let mut params = pc_app_params(client.credentials());
    params.push(("device_platform", "pc".to_string()));
    let url = format!("{PC_BASE}/luna/pc/track_v2?{}", encode_query(&params));

    let mut headers: Vec<(&str, &str)> = client.credentials().signature_headers();
    // App 端点还要这几个「前台请求」标记，缺了会被当成后台任务降级处理。
    headers.push(("x-luna-background-type", "foreground"));
    headers.push(("x-luna-is-background-req", "0"));
    headers.push(("x-luna-is-local-user", "1"));
    headers.push(("Content-Type", "application/json; charset=utf-8"));

    let root = fetch_json(
        client,
        &url,
        client.credentials().user_agent_or_default(),
        &headers,
        "汽水单曲详情（App）",
        Some(&body),
    )
    .await?;
    check_status(&root, "汽水单曲详情（App）")?;
    build_track_response(&root, track_id, "pc")
}

/// `GET /luna/h5/seo_track`。分享页兜底，字段最少但可用率最高。
async fn fetch_seo_track(client: &SodaClient, track_id: &str) -> Result<TrackResponse> {
    let url = format!(
        "{SEO_BASE}?{}",
        encode_query(&[
            ("track_id", track_id.to_string()),
            ("device_platform", "web".to_string()),
        ])
    );
    let root = fetch_json(client, &url, WEB_USER_AGENT, &[], "汽水分享页详情", None).await?;
    check_status(&root, "汽水分享页详情")?;
    build_track_response(&root, track_id, "seo")
}

/// 把一个 `track_v2` / `seo_track` 响应拆成「曲目 + 候选流 + 待跟进地址」。
///
/// 分享页把内容裹在 `seo_track` 里，`track_player` 也随之嵌套一层，
/// 所以这里统一在「裹完之后」的对象上取流。
fn build_track_response(
    root: &Value,
    track_id: &str,
    origin: &'static str,
) -> Result<TrackResponse> {
    // 分享页：seo_track.track；两个 track_v2 端点：track.primary_track / track
    let body = root.get("seo_track").unwrap_or(root);

    let track = track_from_v2(body, track_id)?;
    let (mut candidates, follow_up) = types::extract_candidates(body);
    for candidate in candidates.iter_mut() {
        candidate.origin = origin;
    }

    Ok(TrackResponse {
        track,
        candidates,
        follow_up,
    })
}

/// 从 `track_v2` 响应里取出曲目主体。
fn track_from_v2(root: &Value, track_id: &str) -> Result<Track> {
    // 主体可能在 track.primary_track，也可能在根下的 track
    let candidates = [
        root.get("track").and_then(|t| t.get("primary_track")),
        root.get("track"),
        root.get("primary_track"),
    ];

    for candidate in candidates.into_iter().flatten() {
        if let Some(track) = Track::from_json(candidate) {
            return Ok(track);
        }
    }

    Err(AppError::NotFound(format!(
        "汽水未返回《{track_id}》的详情（可能已下架）"
    )))
}

/// 业务错误码校验。汽水的错误在 `status_code` / `status_info` 里。
fn check_status(root: &Value, what: &str) -> Result<()> {
    let code = types::status_code(root);
    if code == 0 {
        return Ok(());
    }
    let message = types::status_message(root);
    Err(AppError::Api {
        path: what.to_string(),
        code,
        message,
    })
}

// ============================================================================
// 播放链接
// ============================================================================

/// 取播放链接。
///
/// # 与其它音源最大的不同：这里返回的可能是**已解密落盘**的文件
///
/// 汽水的流是加密的，`play_auth` 里藏着 AES 密钥，不解密 rodio 解不出来。
/// 但 [`crate::api::catalog::StreamUrl`] 只有 `url` 一个字段——它没法表达
/// 「这个 URL 需要先下载解密」。
///
/// 所以这里把「下载 + 解密 + 落盘」整件事做完，返回一个 `file://` 路径。
/// 上层的下载器会照常处理它（见 [`crate::source::sodam`] 的模块说明与
/// `audio/download.rs` 里对 `file://` 的支持）。
pub async fn song_stream_url(
    client: &SodaClient,
    song: &Song,
    quality: &str,
    scratch_dir: &std::path::Path,
) -> Result<crate::api::catalog::StreamUrl> {
    let track_id = song.hash.trim();
    if track_id.is_empty() {
        return Err(AppError::NotFound("这首歌没有汽水曲目 id".to_string()));
    }

    // 1) 一次请求拿到「曲目主体 + 候选流」。
    //    `fetch_track_response` 内部已按 App → Web → 分享页降级，
    //    所以这里不用再关心「打到了哪个端点」。
    let response = fetch_track_response(client, track_id).await?;
    let track = response.track;
    let full_duration_ms = if track.duration_ms > 0 {
        track.duration_ms
    } else {
        song.duration_ms
    };

    // 2) 补齐 `url_player_info` 指向的那些流。它们不在上面的响应里，
    //    要各自再发一次请求——只在响应确实带了这个字段时才发。
    let mut candidates = response.candidates;
    for url in response.follow_up {
        collect_from_player_info(client, &url, &mut candidates).await;
    }

    if candidates.is_empty() {
        return Err(AppError::NotFound(format!(
            "《{}》没有可用的播放地址（{}）",
            song.name,
            if track.label.is_vip() {
                "需要汽水会员"
            } else {
                "可能已下架"
            }
        )));
    }

    // 3) 择优：先保证完整，再在完整的前提下选音质
    let selected = select_candidate(&candidates, quality, full_duration_ms);
    let is_trial = selected.is_preview(full_duration_ms);

    // 4) 加密流必须解密后才能播
    if !selected.is_encrypted() {
        return Ok(crate::api::catalog::StreamUrl {
            url: selected.url.clone(),
            is_trial,
            reason: optional_reason(is_trial, &track, Some(selected)),
        });
    }

    let target = scratch_path(scratch_dir, track_id, selected);
    download_and_decrypt(client, selected, &target).await?;

    Ok(crate::api::catalog::StreamUrl {
        url: format!("file://{}", target.display()),
        is_trial,
        reason: optional_reason(is_trial, &track, Some(selected)),
    })
}

/// 在候选里挑一个：满足音质门槛的前提下，优先完整、其次音质高。
///
/// # 降级策略
///
/// 用户设了 flac 但只拿得到 128k 的**整曲**时，选整曲——完整播放比音质更重要。
/// 但如果连 128k 都不满足门槛（比如只拿到 30 秒试听），那门槛就没意义了：
/// 试听总比没有好，此时标记 `is_trial` 让界面如实提示。
fn select_candidate<'a>(
    candidates: &'a [StreamCandidate],
    quality: &str,
    full_duration_ms: u64,
) -> &'a StreamCandidate {
    let min_rank = types::min_rank_for(quality);

    let mut best: Option<&StreamCandidate> = None;

    // 第一轮：要求完整 + 达到音质门槛
    if let Some(min_rank) = min_rank {
        for candidate in candidates {
            if candidate.is_preview(full_duration_ms) {
                continue;
            }
            if types::quality_rank(&candidate.quality, &candidate.format, candidate.bitrate)
                < min_rank
            {
                continue;
            }
            best = match best {
                Some(current) if !types::better_candidate(candidate, current, full_duration_ms) => {
                    Some(current)
                }
                _ => Some(candidate),
            };
        }
        if let Some(found) = best {
            return found;
        }
    }

    // 第二轮：只要完整的（不限音质）
    for candidate in candidates {
        if candidate.is_preview(full_duration_ms) {
            continue;
        }
        best = match best {
            Some(current) if !types::better_candidate(candidate, current, full_duration_ms) => {
                Some(current)
            }
            _ => Some(candidate),
        };
    }
    if let Some(found) = best {
        return found;
    }

    // 第三轮：退到试听——并在候选里挑最好的那个试听。
    // 刻意用择优而不是 `.first()`：多个试听的码率往往不同，
    // 随手拿第一个可能拿到 64k 的那个。
    let mut best_trial: Option<&StreamCandidate> = None;
    for candidate in candidates {
        best_trial = match best_trial {
            Some(current) if !types::better_candidate(candidate, current, full_duration_ms) => {
                Some(current)
            }
            _ => Some(candidate),
        };
    }
    best_trial.expect("调用方已保证候选非空")
}

/// 组装解密后落盘的文件路径。
///
/// 用 `hash`（曲目 id）+ 音质档位命名，避免同一首歌的多个候选互相覆盖。
fn scratch_path(
    dir: &std::path::Path,
    track_id: &str,
    candidate: &StreamCandidate,
) -> std::path::PathBuf {
    let extension = if candidate.format.to_ascii_lowercase().contains("flac") {
        "flac"
    } else {
        "m4a"
    };
    let tag = &candidate.quality.replace(['/', '\\', ' '], "_");
    let stem = format!("{track_id}-{tag}");
    let stem: String = stem.chars().take(80).collect();
    dir.join(format!("{stem}.{extension}"))
}

/// 下载整首并解密落盘。
///
/// # 为什么必须整首下完
///
/// MP4/CENC 的样本表（`stsz`）与加密信息（`senc`）通常在文件尾部的 `moov` 里，
/// 而 `moov` 在**流式下载过程中拿不到**——中途只有 `mdat` 的密文。
/// 不整首下完就无从解密，这也是汽水不能边下边播的根因。
async fn download_and_decrypt(
    client: &SodaClient,
    candidate: &StreamCandidate,
    target: &std::path::Path,
) -> Result<()> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| AppError::io_at(parent.display().to_string(), error))?;
    }

    // 已经下过并解密好了就别重复走网络（换歌再回来时会命中）
    if target.is_file() && target.metadata().is_ok_and(|meta| meta.len() > 0) {
        tlog!(LEVEL_DEBUG, "汽水解密缓存命中：{}", target.display());
        return Ok(());
    }

    tlog!(
        LEVEL_INFO,
        "汽水整曲取流开始（{}）：{}",
        candidate.quality,
        candidate.url
    );

    let encrypted = client
        .get_bytes(&candidate.url, WEB_USER_AGENT, &[])
        .await
        .map_err(|error| AppError::Other(format!("汽水音频下载失败：{error}")))?;

    if encrypted.is_empty() {
        return Err(AppError::Other("汽水音频下载为空".to_string()));
    }

    // 先写临时文件再改名：中途失败不会留下一个「看起来完整」的坏文件
    // 被下次播放当成缓存命中。
    let temp = target.with_extension("part");
    // 直接走 `decrypt_audio`（内部含 play_auth → 密钥 → 逐样本解密），
    // 错误原样上抛：它已经能区分「密钥解不出」与「MP4 结构不对」，
    // 包一层反而会把有用的原因糊掉。
    let decrypted = crypto::decrypt_audio(&encrypted, &candidate.play_auth)?;

    std::fs::write(&temp, &decrypted)
        .map_err(|error| AppError::io_at(temp.display().to_string(), error))?;
    std::fs::rename(&temp, target)
        .map_err(|error| AppError::io_at(target.display().to_string(), error))?;

    tlog!(
        LEVEL_INFO,
        "汽水解密完成：{}（{} 字节）",
        target.display(),
        decrypted.len()
    );
    Ok(())
}

/// 跟随 `url_player_info` 再取一次流信息。
///
/// 端点返回的是**完整绝对地址**（每次都不同，可能是不同 CDN），
/// 所以这里不能走「base + path」的拼法。
async fn collect_from_player_info(client: &SodaClient, url: &str, out: &mut Vec<StreamCandidate>) {
    match fetch_json(client, url, WEB_USER_AGENT, &[], "汽水取流信息", None).await {
        Ok(root) => {
            if types::status_code(&root) != 0 {
                tlog!(
                    LEVEL_DEBUG,
                    "汽水取流信息返回业务错误 {}：{}",
                    types::status_code(&root),
                    types::status_message(&root)
                );
                return;
            }
            out.extend(types::parse_player_info(&root));
        }
        Err(error) => {
            tlog!(LEVEL_DEBUG, "汽水取流信息失败：{error}");
        }
    }
}

/// 组装给用户看的降级说明，空字符串视为「没有要说的」返回 `None`。
///
/// 之所以多包一层：`StreamUrl::reason` 是 `Option`，而这里算出来的文案
/// 在「一切正常」时是空串——直接返回空串会让上层显示一条空的提示信息。
fn optional_reason(
    is_trial: bool,
    track: &Track,
    candidate: Option<&StreamCandidate>,
) -> Option<String> {
    let reason = describe_reason(is_trial, track, candidate);
    if reason.is_empty() {
        None
    } else {
        Some(reason)
    }
}

/// 组装给用户看的降级说明。
///
/// # 为什么要区分「整首要 VIP」与「这一档要 VIP」
///
/// 汽水的会员限制常常是**按音质档位**的：一首歌 128k 免费整曲可听，
/// 无损要会员。这两种情况的处置完全不同——前者只能换歌或开会员，
/// 后者把音质调到 128 就能听。把它们混成一句「需要会员」的话，
/// 用户会以为整首都听不了，白放弃一个本来能解决的问题。
fn describe_reason(is_trial: bool, track: &Track, candidate: Option<&StreamCandidate>) -> String {
    if is_trial {
        let mut reason = "只有试听片段".to_string();
        if track.label.is_vip() {
            reason.push_str("：该曲需要汽水会员");
        }
        if let Some(candidate) = candidate
            && candidate.bitrate > 0
        {
            reason.push_str(&format!("（已下到 {} kbps）", candidate.bitrate / 1000));
        }
        return reason;
    }

    // 整曲拿到了，但音质可能被降级。如实说出来，别让界面标着 flac
    // 而耳朵听的是 128k——那种「查不出来哪里不对」的状态最费时间。
    let Some(candidate) = candidate else {
        return String::new();
    };
    let actual = candidate.quality.trim();
    if actual.is_empty() {
        return String::new();
    }

    // 「整曲要会员」和「只有某些音质档要会员」是两回事：后者把音质降一档
    // 就能听。查一下这一档本身是否被标了会员要求，据此给不同的提示。
    if track.label.quality_needs_vip(actual) {
        format!("实际音质 {actual}（该音质需要会员）")
    } else if track.label.is_vip() {
        format!("实际音质 {actual}（该曲部分音质需要会员）")
    } else {
        format!("实际音质 {actual}")
    }
}

// ============================================================================
// 歌词
// ============================================================================

/// 取歌词。
///
/// 汽水的「逐字歌词」格式是 `[起始毫秒,时长]歌词<逐字时间>词</...>`，
/// 与酷狗 KRC、网易云 LRC 都不同——需要转成标准 LRC 才能被
/// [`crate::api::lyric::parse_lrc`] 解析（见 [`parse_soda_lyric`]）。
pub async fn fetch_lyric(client: &SodaClient, song: &Song) -> Result<Lyric> {
    let track_id = song.hash.trim();
    if track_id.is_empty() {
        return Err(AppError::NotFound("这首歌没有汽水曲目 id".to_string()));
    }

    // 详情响应里通常已经带歌词了，先看它；没有再单独查一次。
    if let Ok(track) = fetch_track(client, track_id).await
        && !track.lyric_raw.trim().is_empty()
    {
        return Ok(crate::api::lyric::parse_lrc(&parse_soda_lyric(
            &track.lyric_raw,
        )));
    }

    Err(AppError::NotFound(format!("未找到《{}》的歌词", song.name)))
}

/// 汽水逐字歌词 → 标准 LRC。
///
/// 输入形如 `[12345,6789]唱<1000>词<2000>字`，输出 `[00:12.34]唱词字`。
///
/// `<...>` 里的逐字时间被丢掉：现有歌词渲染按行插值，保留它们反而会让
/// 行内出现多余的标记。要真正的逐字渲染需要改渲染层，那是另一件事——
/// 这里先保证内容正确、格式统一。
pub fn parse_soda_lyric(raw: &str) -> String {
    let mut out = String::new();

    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // 格式是 [start,duration]内容
        let Some(rest) = line.strip_prefix('[') else {
            continue;
        };
        let Some((start, rest)) = rest.split_once(',') else {
            continue;
        };
        let Some((_, content)) = rest.split_once(']') else {
            continue;
        };
        let Ok(start_ms) = start.trim().parse::<i64>() else {
            continue;
        };

        let clean = strip_angle_tags(content);
        if clean.trim().is_empty() {
            continue;
        }

        let minutes = start_ms / 60_000;
        let seconds = (start_ms % 60_000) / 1000;
        // 厘秒（1/10 秒），与 LRC 标准一致
        let centis = (start_ms % 1000) / 10;
        out.push_str(&format!("[{minutes:02}:{seconds:02}.{centis:02}]{clean}\n"));
    }

    out
}

/// 去掉 `<...>` 逐字标记。
fn strip_angle_tags(content: &str) -> String {
    let mut out = String::with_capacity(content.len());
    let mut inside = false;
    for ch in content.chars() {
        match ch {
            '<' if !inside => inside = true,
            '>' if inside => inside = false,
            _ if !inside => out.push(ch),
            _ => {}
        }
    }
    out
}

// ============================================================================
// 封面
// ============================================================================

/// 取封面地址。
///
/// 搜索结果自带封面（`urls` 或 `uri` 形态），直接用；
/// 没有的话再查一次详情——那里给的是另一种形态。
pub async fn cover_url(client: &SodaClient, song: &Song) -> Result<Option<String>> {
    if let Some(cover) = song.cover.as_deref().filter(|url| !url.trim().is_empty()) {
        return Ok(Some(cover.to_string()));
    }
    if song.hash.trim().is_empty() {
        return Ok(None);
    }
    match fetch_track(client, song.hash.trim()).await {
        Ok(track) => Ok(track.cover),
        Err(error) => {
            tlog!(LEVEL_DEBUG, "汽水封面查询失败：{error}");
            Ok(None)
        }
    }
}

// ============================================================================
// 目录类能力：不支持
// ============================================================================
//
// 汽水的歌单 / 榜单 / 歌手要么不存在、要么同样要应用签名，且本项目用不上
// 它们的写入能力。分派层里每个相关方法都直接返回 [`unsupported`]，
// 这里不提供对应的实现——写了也不会被调用。
//
// 之所以**明确报错**而不是返回空列表：空列表会被用户读成「网络问题」
// 或「筛选条件不对」，绕一圈才发现这个音源根本没这个功能。

/// 「本音源不支持该能力」的标准错误。
pub fn unsupported(what: &str) -> AppError {
    AppError::Other(format!("汽水音乐暂不支持{what}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use client::AppCredentials;
    use serde_json::json;

    #[test]
    fn soda_lyric_converts_to_standard_lrc() {
        let raw = "[0,3000]第一行\n[65000,4000]<100>唱<200>词<300>字\n[invalid,x]跳过\n\
                   [70000,2000]\n[not-a-timestamp]也跳过";
        let lrc = parse_soda_lyric(raw);
        assert!(lrc.contains("[00:00.00]第一行"), "实际：{lrc}");
        // 65 秒 → 01:05.00（厘秒 = 毫秒/10）
        assert!(lrc.contains("[01:05.00]唱词字"), "逐字标记应被剥掉：{lrc}");
        // 格式不合法的行要跳过，不能 panic
        assert!(!lrc.contains("跳过"));
        assert!(!lrc.contains("not-a-timestamp"));
    }

    #[test]
    fn soda_lyric_handles_empty_input() {
        assert!(parse_soda_lyric("").is_empty());
        assert!(parse_soda_lyric("\n\n\n").is_empty());
    }

    #[test]
    fn soda_lyric_keeps_long_timestamps() {
        // 超过一小时的曲目不能算错
        let lrc = parse_soda_lyric("[3725000,1000]很长");
        assert!(lrc.contains("[62:05.00]很长"), "实际：{lrc}");
    }

    #[test]
    fn strip_angle_tags_removes_nested_markers() {
        assert_eq!(strip_angle_tags("a<1>b<2>c"), "abc");
        assert_eq!(strip_angle_tags("no tags"), "no tags");
        // 未闭合的标记也要处理干净，不能把后半截吞掉
        assert_eq!(strip_angle_tags("a<1>b"), "ab");
    }

    #[test]
    fn check_status_accepts_zero_and_rejects_others() {
        assert!(check_status(&json!({"status_code": 0}), "x").is_ok());
        let err = check_status(
            &json!({"status_code": 8, "status_info": {"status_msg": "需要登录"}}),
            "汽水测试",
        )
        .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("汽水测试"), "实际：{text}");
        assert!(text.contains("需要登录"), "实际：{text}");
    }

    /// 业务错误码要被认成「需要登录」，界面才能弹登录引导而不是普通报错。
    #[test]
    fn check_status_produces_auth_recognizable_errors() {
        let err = check_status(&json!({"status_code": 8}), "汽水测试").unwrap_err();
        assert!(!err.is_auth_related(), "目前汽水没有登录引导，先确认现状");
    }

    #[test]
    fn empty_body_error_distinguishes_missing_vs_expired_credentials() {
        let missing = empty_body_error(
            &SodaClient::new(None, AppCredentials::default(), None).unwrap(),
            "汽水取流",
        );
        assert!(
            missing.to_string().contains("缺少应用级签名头"),
            "实际：{missing}"
        );

        let expired = empty_body_error(
            &SodaClient::new(
                None,
                AppCredentials {
                    device_id: "d".into(),
                    x_helios: "h".into(),
                    x_medusa: "m".into(),
                    ..Default::default()
                },
                None,
            )
            .unwrap(),
            "汽水取流",
        );
        assert!(expired.to_string().contains("已过期"), "实际：{expired}");
    }

    #[test]
    fn search_params_include_dynamic_fields() {
        let params = android_search_params("周杰伦", 40, 20);
        let find = |key: &str| {
            params
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };
        assert_eq!(find("q"), "周杰伦");
        assert_eq!(find("cursor"), "40");
        assert_eq!(find("count"), "20");
        assert!(
            !find("_rticket").is_empty(),
            "时间戳要带上，否则命中服务端缓存"
        );
    }

    #[test]
    fn parse_search_tracks_dedupes_across_groups() {
        let root = json!({
            "result_groups": [
                {"data": [
                    {"entity": {"track": {"id": "1", "name": "歌一"}}},
                    {"entity": {"track": {"id": "2", "name": "歌二"}}}
                ]},
                {"data": [
                    {"entity": {"track": {"id": "1", "name": "歌一（重复）"}}},
                    {"entity": {"track": {"id": "3", "name": "歌三"}}}
                ]}
            ]
        });
        let tracks = parse_search_tracks(&root);
        assert_eq!(tracks.len(), 3, "重复的 id 要被去掉：{tracks:?}");
        assert_eq!(tracks[0].name, "歌一", "保留首次出现的");
    }

    #[test]
    fn parse_search_tracks_accepts_flat_shape() {
        // 有的条目可能直接是 track 而非 { entity: { track } }
        let root = json!({
            "result_groups": [{"data": [{"id": "9", "name": "扁平结构"}]}]
        });
        let tracks = parse_search_tracks(&root);
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].id, "9");
    }

    #[test]
    fn parse_search_tracks_on_unexpected_shape_is_empty() {
        assert!(parse_search_tracks(&json!({})).is_empty());
        assert!(parse_search_tracks(&json!({"result_groups": []})).is_empty());
    }

    #[test]
    fn track_from_v2_finds_primary_track() {
        let root = json!({
            "track": {"primary_track": {"id": "abc", "name": "主体"}}
        });
        let track = track_from_v2(&root, "abc").unwrap();
        assert_eq!(track.name, "主体");
    }

    #[test]
    fn track_from_v2_reports_missing_track() {
        let err = track_from_v2(&json!({"track": {}}), "xyz").unwrap_err();
        assert!(
            err.to_string().contains("xyz"),
            "错误信息要带上曲目 id：{err}"
        );
    }

    fn candidate(url: &str, duration: u64, bitrate: u32, quality: &str) -> StreamCandidate {
        StreamCandidate {
            url: url.to_string(),
            duration_ms: duration,
            bitrate,
            quality: quality.to_string(),
            format: "m4a".to_string(),
            play_auth: "AUTH".to_string(),
            size: 0,
            origin: "web",
        }
    }

    /// 用户设了 flac 但只有 128k 整曲时，选整曲而不是回退到高码率试听。
    #[test]
    fn select_candidate_prefers_complete_over_bitrate_when_quality_unreachable() {
        let candidates = [
            candidate("full-128", 240_000, 128_000, "standard"),
            candidate("preview-320", 30_000, 320_000, "higher"),
        ];
        let picked = select_candidate(&candidates, "flac", 240_000);
        assert_eq!(picked.url, "full-128", "应选完整的那条");
    }

    /// 门槛内有多条完整流时，选音质最高的。
    #[test]
    fn select_candidate_picks_best_among_complete_when_gate_met() {
        let candidates = [
            candidate("128", 240_000, 128_000, "standard"),
            candidate("flac", 240_000, 1_000_000, "lossless"),
        ];
        let picked = select_candidate(&candidates, "flac", 240_000);
        assert_eq!(picked.url, "flac");
    }

    /// 只有试听时也要给一条（并由上层标记 is_trial 提示用户）。
    #[test]
    fn select_candidate_falls_back_to_preview_rather_than_nothing() {
        let candidates = [
            candidate("preview-a", 30_000, 128_000, "standard"),
            candidate("preview-b", 60_000, 320_000, "higher"),
        ];
        let picked = select_candidate(&candidates, "flac", 240_000);
        // 两条都是试听，取更好的那条
        assert_eq!(picked.url, "preview-b");
    }

    #[test]
    fn scratch_path_sanitizes_and_limits_length() {
        let dir = std::path::Path::new("/tmp");
        let path = scratch_path(dir, "7304719759323564095", &candidate("u", 0, 0, "a/b c"));
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        assert!(!name.contains('/'), "路径分隔符不能进文件名：{name}");
        assert!(!name.contains(' '), "空格也要换掉：{name}");
        assert!(name.ends_with(".m4a"), "实际：{name}");
        assert!(name.len() <= 90, "文件名要限长：{name}");
    }

    #[test]
    fn scratch_path_uses_flac_extension_for_lossless() {
        let dir = std::path::Path::new("/tmp");
        let mut flac = candidate("u", 0, 0, "lossless");
        flac.format = "flac".into();
        let path = scratch_path(dir, "id", &flac);
        assert!(path.to_string_lossy().ends_with(".flac"));
    }

    #[test]
    fn describe_reason_mentions_vip_for_trial_of_member_track() {
        let mut track = Track {
            id: "x".into(),
            name: "会员歌".into(),
            ..Default::default()
        };
        track.label.only_vip_playable = true;

        let reason = describe_reason(true, &track, None);
        assert!(reason.contains("试听"), "实际：{reason}");
        assert!(reason.contains("会员"), "实际：{reason}");
    }

    #[test]
    fn describe_reason_reports_actual_quality_when_downgraded() {
        let track = Track {
            id: "x".into(),
            name: "歌".into(),
            ..Default::default()
        };
        let picked = candidate("u", 240_000, 128_000, "standard");
        let reason = describe_reason(false, &track, Some(&picked));
        assert!(reason.contains("standard"), "音质降级要如实说明：{reason}");
    }

    #[test]
    fn unsupported_capabilities_say_so_explicitly() {
        // 返回空列表会被误读成「网络问题」，所以必须明确说不支持
        let message = unsupported("歌单浏览").to_string();
        assert!(message.contains("不支持"), "实际：{message}");
        assert!(message.contains("歌单浏览"), "要说清是哪项能力：{message}");
    }
}
