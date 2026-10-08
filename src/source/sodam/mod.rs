//! 汽水音乐（Soda Music）音源。
//!
//! # 数据面直接用 libresoda
//!
//! 本模块是**薄封装**：登录、搜索、取流、解密、歌单、歌手、专辑、会员状态
//! 全部委托给 [`libresoda`]——汽水官方客户端 SodaM
//! （<https://github.com/sodahub-org/sodam>）用的同一套库。
//!
//! 这里曾经有一套自写实现（登录参数拼装、MP4/CENC 逐样本解密、响应解析，
//! 约 4300 行）。它能把歌唱出来，但缺了一整片能力：歌单、歌手、专辑、会员状态
//! 全都没接。原因是这些和 VIP 整曲卡在**同一道门槛**上——`/luna/pc/` 这一族
//! 要求逐请求的应用级签名。libresoda 已经解决了那道门槛（签名服务客户端 +
//! 内置 CDP 签名页），自己再实现一遍只会继续把这些能力挡在门外。
//!
//! # 与另三个音源的差别仍然成立
//!
//! * **直连公网**，不跑本机接口服务。`bootstrap` 不为它准备服务，首次使用
//!   不需要 `--api-start`；但**扫码登录**会按需拉起一个本机 Chromium
//!   （libresoda 的 CDP 签名页），那是另一回事。
//! * **音频是加密的**，取链要把「下载 + 解密」做完才能交给播放器，
//!   所以不能边下边播。这一层现在是 libresoda 的 `download`。
//!
//! # 阻塞
//!
//! libresoda 内部是阻塞式 `ureq`，所有调用都经 [`blocking`] 搬到阻塞线程池，
//! 不占 tokio 的工作线程。

pub mod client;

use std::collections::HashMap;
use std::sync::Mutex;

use serde::Deserialize;

use crate::api::catalog::StreamUrl;
use crate::api::ApiClient;
use crate::api::model::{Lyric, Song};
use crate::error::{AppError, Result};
use crate::logger::{LEVEL_DEBUG, LEVEL_INFO, tlog};

pub use client::AppCredentials;

// ============================================================================
// 进程级配置槽位
// ============================================================================

/// 进程级汽水配置（对应 `[sources.sodam_app]`）。
///
/// # 为什么是全局的
///
/// 分派层的方法签名统一只收 `&ApiClient`，而汽水的设备指纹与签名服务地址来自
/// **用户配置**，不是 HTTP 层能表达的东西（既不是 cookie，也不是 URL 参数）。
/// 为了让那十几个调用点不必各自多带一个参数，这里用全局槽位承载——但**只有一处
/// 会写它**：[`crate::app`] 的 `client_for()`，也就是「按音源造客户端」的唯一入口。
///
/// 可接受的理由：这是「这台机器上这个用户的汽水身份」，本来就是进程级单例。
static ACTIVE_CREDENTIALS: Mutex<AppCredentials> = Mutex::new(AppCredentials {
    device_id: String::new(),
    iid: String::new(),
    fp: String::new(),
    x_helios: String::new(),
    x_medusa: String::new(),
    user_agent: String::new(),
    signer_url: String::new(),
    signer_token: String::new(),
});

/// 取当前生效的汽水配置。
pub fn active_credentials() -> AppCredentials {
    ACTIVE_CREDENTIALS
        .lock()
        .map(|slot| slot.clone())
        .unwrap_or_default()
}

/// 覆盖当前生效的汽水配置。只在 `client_for()` 一处调用。
///
/// 锁被毒化时保持原值而不是 panic——那意味着别的线程写它时崩了，
/// 而这里只是拿不到配置，不该让整个程序挂掉。
pub fn set_active_credentials(credentials: AppCredentials) {
    if let Ok(mut slot) = ACTIVE_CREDENTIALS.lock() {
        *slot = credentials;
    }
}

// ============================================================================
// 阻塞桥
// ============================================================================

/// 把一次阻塞调用搬到 tokio 的阻塞线程池。
///
/// libresoda 全程用 `ureq`（同步）。直接在异步上下文里调它会**占住**一个
/// tokio 工作线程直到网络返回——并发几首歌就能让界面卡住。`spawn_blocking`
/// 把它挪到专门的阻塞池，工作线程立刻可以去做别的事。
async fn blocking<T, F>(what: &str, work: F) -> Result<T>
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(work).await {
        Ok(result) => result,
        // 任务 panic 或被取消。把 what 带出来，否则用户只看到一句
        // 「join error」，完全定位不到是哪一步炸的。
        Err(error) => Err(AppError::Other(format!(
            "汽水{what}的后台任务失败：{error}"
        ))),
    }
}

/// libresoda 的错误 → 本项目的错误。
///
/// `NotFound` 单独映射：界面靠它区分「这首歌确实没有」与「接口挂了」，
/// 前者不该提示用户重试。
fn convert(error: libresoda::SodaError) -> AppError {
    let text = error.to_string();
    match error {
        libresoda::SodaError::NotFound(_) => AppError::NotFound(format!("汽水：{text}")),
        // 其余原样保留文案：libresoda 的错误信息里有「缺哪个签名头」这类
        // 可直接照做的提示，包一层就把它糊掉了。
        _ => AppError::Other(format!("汽水：{text}")),
    }
}

// ============================================================================
// 领域模型转换
// ============================================================================

/// libresoda 的 `Song` → 本项目的 `Song`。
///
/// # 字段对应
///
/// * `id` → `hash`：本项目把「取链与取歌词的主键」通称为 hash（酷狗是
///   FileHash、网易云是数字 id），汽水这里就是曲目 id。
/// * `duration` 是**秒**，本项目统一用毫秒，要乘 1000。
/// * `artist` 是拼好的字符串，反拆成 `Vec<Singer>` 只为复用现有的渲染逻辑。
/// * `is_vip` → `privilege`：借用为「需要会员」的标记，让播放前的预警逻辑
///   不必为每个音源各写一套。
fn to_song(source: &libresoda::Song) -> Song {
    Song {
        name: source.name.clone(),
        hash: source.id.clone(),
        album_id: source.album_id.clone(),
        album_audio_id: 0,
        album_name: source.album.clone(),
        singers: split_artists(&source.artist),
        duration_ms: (source.duration.max(0) as u64).saturating_mul(1000),
        cover: Some(source.cover.clone()).filter(|url| !url.trim().is_empty()),
        privilege: source.is_vip.then_some(1),
        file_id: None,
        extra_hashes: Default::default(),
        source: crate::source::SourceKind::Sodam,
    }
}

/// 把「甲、乙」这样的歌手串拆成列表。
///
/// 汽水用中文顿号分隔，也见过分号与斜杠；三种都认。单个名字时返回一个元素。
fn split_artists(joined: &str) -> Vec<crate::api::model::Singer> {
    let trimmed = joined.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    trimmed
        .split(['、', ';', '/'])
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .enumerate()
        .map(|(index, name)| crate::api::model::Singer {
            // libresoda 只给拼好的字符串，没有单独 id。这里给**负数序号**而不是 0：
            // 0 在若干接口里表示「缺省/全部」，给 0 会让「按歌手筛选」意外命中全部。
            // 负数不可能与真实 id 冲突。
            id: -(index as i64) - 1,
            name: name.to_string(),
        })
        .collect()
}

/// 本项目的音质档位 → libresoda 的偏好键。
///
/// 两边的档位表不是一套：本项目沿用的是酷狗的（`super` / `viper_*`），
/// libresoda 认汽水的（`lossless` / `highest` / `320k` / `128k`…）。
pub fn quality_preference(quality: &str) -> &'static str {
    match quality.trim().to_ascii_lowercase().as_str() {
        // 酷狗的档位名 → 汽水的偏好键
        "128" => "128k",
        "320" => "320k",
        "flac" => "lossless",
        "high" => "high",
        "super" => "highest",
        // `viper_*` 是酷狗的 VIP 音效档位，汽水没有对应概念——落到「不限」，
        // 让 libresoda 自己挑能拿到的最高档，而不是硬套一个不存在的档位。
        _ => "best",
    }
}

/// 结合**账号权益**决定最终的音质偏好。
///
/// 照 SodaM 的 `Session::auto_quality_for(account.vip)`：
///
/// * 会员 → 用映射后的档位（无损封顶；单曲没有无损时由服务端往下匹配）
/// * 非会员 → `"auto"`，让服务端给**免费档里实际可用的最高一档**
///
/// 为什么非会员不能沿用用户设的档位：那等于向服务端**索取会员档位**
/// （`flac` → `lossless`），而官方客户端在这种情况明确要 `"auto"`。
/// 这是本模块此前与官方客户端**唯一**的实质差异。
fn preference_for_account(vip: bool, quality: &str) -> &'static str {
    if vip {
        quality_preference(quality)
    } else {
        "auto"
    }
}

// ============================================================================
// 搜索
// ============================================================================

/// 搜索结果的信封结构。
///
/// 与 libresoda `search.rs` 内部那套**逐字段对齐**（那边是 `pub(crate)`，用不了）。
/// 只复制「怎么走进 JSON」这一层；曲目本身仍用它的
/// [`libresoda::soda::types::Track`] 与公开的 `build_song_from_track`，
/// 所以字段语义不会漂。
#[derive(Debug, Deserialize)]
struct SearchResponse {
    #[serde(default)]
    result_groups: Vec<SearchGroup>,
}

#[derive(Debug, Deserialize)]
struct SearchGroup {
    #[serde(default)]
    data: Vec<SearchItem>,
}

#[derive(Debug, Deserialize)]
struct SearchItem {
    #[serde(default)]
    entity: SearchEntity,
}

#[derive(Debug, Default, Deserialize)]
struct SearchEntity {
    #[serde(default)]
    track: libresoda::soda::types::Track,
}

/// 单曲搜索。
///
/// 走 Android 网关，**不需要签名**，所以没配签名服务时也能用。
/// 分页自己拼：libresoda 的 `song::search` 写死了第 1 页，而这里要支持
/// 「加载更多」。
pub async fn search_songs(
    api: &ApiClient,
    keyword: &str,
    page: u32,
    page_size: u32,
) -> Result<Vec<Song>> {
    let soda = client::configure(api);
    let keyword = keyword.to_string();
    let page = i64::from(page.max(1));
    let page_size = i64::from(if page_size == 0 { 20 } else { page_size });

    blocking("搜索", move || {
        let body = libresoda::soda::search::fetch_search_all_body(soda, &keyword, page, page_size)
            .map_err(convert)?;
        let parsed: SearchResponse = serde_json::from_slice(&body)
            .map_err(|error| AppError::Other(format!("汽水搜索响应解析失败：{error}")))?;

        // 同一个 id 可能在多个结果分组里重复（单曲组与「相关推荐」组），去重。
        let mut seen: Vec<String> = Vec::new();
        let mut out = Vec::new();
        for group in parsed.result_groups {
            for item in group.data {
                let track = item.entity.track;
                if track.id.is_empty() || seen.contains(&track.id) {
                    continue;
                }
                seen.push(track.id.clone());
                out.push(to_song(&libresoda::soda::track::build_song_from_track(
                    &track,
                )));
            }
        }
        Ok(out)
    })
    .await
}

// ============================================================================
// 取流
// ============================================================================

/// 取播放地址。
///
/// # 返回的是**本地文件**，不是 HTTP 直链
///
/// 汽水的音频是加密的，`play_auth` 里藏着 AES 密钥。libresoda 的 `download`
/// 把「下载 → 密钥还原 → 按 `senc` 逐样本解密 → 落盘」整件事做完，所以我们
/// 交回去的是一个 `file://` 路径，由下载器复制进音频缓存（见
/// `audio/download.rs` 对 `file://` 的支持）。
///
/// 代价是**不能边下边播**：样本表在文件尾部的 `moov` 里，流式下载过程中拿不到。
/// 这是加密方案决定的，不是实现取巧。
pub async fn song_stream_url(
    api: &ApiClient,
    song: &Song,
    quality: &str,
    scratch_dir: &std::path::Path,
) -> Result<StreamUrl> {
    if song.hash.trim().is_empty() {
        return Err(AppError::NotFound("这首歌没有汽水曲目 id".to_string()));
    }

    let soda = client::configure(api);
    let track_id = song.hash.trim().to_string();
    let name = song.name.clone();
    let preference = quality_preference(quality);
    let scratch = scratch_dir.to_path_buf();

    blocking("取流", move || {
        // 音质偏好要**按账号权益**决定，不能无脑用客户端的档位设置。
        //
        // 照 SodaM 的 \`Session::auto_quality_for(account.vip)\`：
        //   VIP   → 无损封顶（单曲没有无损时由服务端往下匹配）
        //   非 VIP → "auto"，让服务端给**免费档里实际可用的最高一档**
        //
        // 差别很关键：非会员若按 \`flac\` 映射成 lossless 去要，那是在向服务端
        // **索取会员档位**——上游的做法是明确要 "auto"，让服务端在免费档里挑。
        // 这条是本模块此前与官方客户端唯一的实质差异。
        //
        // 查询失败时退回用户设置（\`unwrap_or(true)\`）：宁可对非会员多要一档
        // （服务端会自己降级），也不要让真会员被莫名降到免费档。
        let preference = preference_for_account(
            libresoda::soda::account::is_vip_account(soda).unwrap_or(true),
            preference,
        );
        soda.set_quality_preference(preference);

        // 时长与 VIP 标记要用于「这是不是试听片段」的判断。
        let detail = libresoda::soda::track::fetch_song_detail(soda, &track_id).map_err(convert)?;
        let full_seconds = detail.duration.max(0);

        std::fs::create_dir_all(&scratch)
            .map_err(|error| AppError::io_at(scratch.display().to_string(), error))?;
        let target = scratch.join(scratch_file_name(&track_id, preference));

        // 已经下好并解密过就直接用（换歌再回来会命中）
        if target.is_file() && target.metadata().is_ok_and(|meta| meta.len() > 0) {
            tlog!(LEVEL_DEBUG, "汽水解密缓存命中：{}", target.display());
            return Ok(StreamUrl {
                url: format!("file://{}", target.display()),
                is_trial: false,
                reason: None,
            });
        }

        let info = libresoda::soda::download::download_with_info(soda, &detail, &target).map_err(
            |error| {
                // 拿不到整曲时最常见的原因是缺签名。「空响应」这个现象很反直觉
                // （HTTP 200 但 body 为空），把下一步动作直接写出来。
                let text = error.to_string();
                if text.contains("empty body") || text.contains("签名") {
                    AppError::Other(format!(
                        "《{name}》取流失败：接口返回空响应，通常是应用签名不可用。\
                         汽水默认会用一个公共签名服务；也可以在 \
                         [sources.sodam_app].signer_url 里换成自己的。原始错误：{text}"
                    ))
                } else {
                    convert(error)
                }
            },
        )?;

        let is_trial = info.is_preview || libresoda::soda::quality::is_preview(&info, full_seconds);

        // 只拿到试听时，用 libresoda 的专用诊断**问清原因**。
        //
        // 它的 `StreamAccessReport` 正是为这件事设计的：`hint` 是可直接展示的
        // 人话结论，`app_error` 是 App 端点失败时的原始原因，另有三个标志说明
        // 「差哪一环」（cookie / 设备指纹 / 签名服务）。只回一句「只有试听片段」
        // 等于把排查工作全推给用户——而这里有现成的答案。
        let report = if is_trial {
            libresoda::soda::stream::check_stream_access(soda, &track_id).ok()
        } else {
            None
        };
        let reason = describe_reason(is_trial, &info, &detail, report.as_ref());
        tlog!(
            LEVEL_INFO,
            "汽水取流完成：{}（{} {}）",
            target.display(),
            info.format,
            info.quality
        );

        Ok(StreamUrl {
            url: format!("file://{}", target.display()),
            is_trial,
            reason,
        })
    })
    .await
}

/// 解密产物在临时目录里的文件名。
///
/// 用「曲目 id + 音质档位」命名：同一首歌的不同档位不能互相覆盖，
/// 否则用户切了音质却听到上一档的缓存。
fn scratch_file_name(track_id: &str, preference: &str) -> String {
    let tag: String = preference
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
        .collect();
    // 限长：文件名过长在若干文件系统上直接 ENAMETOOLONG
    let stem: String = format!("{track_id}-{tag}").chars().take(80).collect();
    // 扩展名只影响观感：播放器与缓存都按内容探测（rodio 走 header 嗅探）。
    format!("{stem}.m4a")
}

/// 组装给用户看的降级说明；没话说时返回 `None`。
fn describe_reason(
    is_trial: bool,
    info: &libresoda::soda::types::DownloadInfo,
    detail: &libresoda::Song,
    report: Option<&libresoda::soda::stream::StreamAccessReport>,
) -> Option<String> {
    if is_trial {
        let mut reason = "只有试听片段".to_string();

        // 优先用 libresoda 的结论：它已经判断过「缺 cookie / 缺设备指纹 /
        // 缺签名服务 / 端点回空 body」这些情况，比我们自己猜准。
        if let Some(report) = report {
            if !report.hint.trim().is_empty() {
                reason.push_str(&format!("：{}", report.hint.trim()));
            }
            // 差哪一环，逐项列出来——用户据此才知道下一步做什么。
            let mut blockers: Vec<&str> = Vec::new();
            if !report.has_cookie {
                blockers.push("未登录");
            }
            if !report.has_app_credentials {
                blockers.push("未配设备指纹");
            }
            if !report.has_signature_provider {
                blockers.push("未配签名服务");
            }
            if !blockers.is_empty() {
                reason.push_str(&format!("〔{}〕", blockers.join("、")));
            }
            // **决定性判据**：流来自哪个端点。
            // `web` = 走的是降级路径（App 端点被拒后回退），问题在客户端这一侧；
            // `pc`  = 服务端在**带签名的 App 端点**上就只给了试听，问题在服务端
            //         的权益判定，客户端无从补救。没有这一项就无法区分两者，
            //         后面的排查会一直在错误的方向上打转。
            if !report.source.trim().is_empty() {
                reason.push_str(&format!("〔端点 {}〕", report.source.trim()));
            }
            // App 端点被拒的原始原因。有它才能区分「没签名」与「签名过期」。
            if !report.app_error.trim().is_empty() {
                reason.push_str(&format!("（App 端点：{}）", report.app_error.trim()));
            }
        } else if detail.is_vip {
            reason.push_str("：该曲需要汽水会员");
        }

        if info.bitrate > 0 {
            reason.push_str(&format!("（已下到 {} kbps）", info.bitrate / 1000));
        }
        return Some(reason);
    }

    // 整曲拿到了，但音质可能被降级。如实说出来，别让界面标着 flac
    // 而耳朵听的是 128k——那种「查不出来哪里不对」的状态最费时间。
    let actual = info.quality.trim();
    if actual.is_empty() {
        return None;
    }
    Some(format!("实际音质 {actual}"))
}

// ============================================================================
// 歌词 / 封面
// ============================================================================

/// 取歌词。走免签名的 Web 端点。
pub async fn fetch_lyric(api: &ApiClient, song: &Song) -> Result<Lyric> {
    if song.hash.trim().is_empty() {
        return Err(AppError::NotFound("这首歌没有汽水曲目 id".to_string()));
    }
    let soda = client::configure(api);
    let track_id = song.hash.trim().to_string();
    let name = song.name.clone();

    blocking("取歌词", move || {
        let detail = libresoda::soda::track::fetch_song_detail(soda, &track_id).map_err(convert)?;
        let raw = libresoda::soda::lyric::get_lyrics(soda, &detail).map_err(convert)?;
        if raw.trim().is_empty() {
            return Err(AppError::NotFound(format!("未找到《{name}》的歌词")));
        }
        // libresoda 给的是逐字格式，它自带转标准 LRC 的解析器。
        Ok(crate::api::lyric::parse_lrc(
            &libresoda::soda::lyric::parse_soda_lyric(&raw),
        ))
    })
    .await
}

/// 取封面地址。
///
/// 搜索结果通常已带封面，先用它；没有才查一次详情。
pub async fn cover_url(api: &ApiClient, song: &Song) -> Result<Option<String>> {
    if let Some(cover) = song.cover.as_deref().filter(|url| !url.trim().is_empty()) {
        return Ok(Some(cover.to_string()));
    }
    if song.hash.trim().is_empty() {
        return Ok(None);
    }

    let soda = client::configure(api);
    let track_id = song.hash.trim().to_string();
    blocking("取封面", move || {
        match libresoda::soda::track::fetch_song_detail(soda, &track_id) {
            Ok(detail) => Ok(Some(detail.cover).filter(|url| !url.trim().is_empty())),
            // 封面拿不到不该让整首歌失败——界面退回占位图即可。
            Err(error) => {
                tlog!(LEVEL_DEBUG, "汽水封面查询失败：{error}");
                Ok(None)
            }
        }
    })
    .await
}

// ============================================================================
// 扫码登录
// ============================================================================

/// 会话键 → 二维码内容。
///
/// libresoda 的 `check_qr` 只认它下发的 `token`，而分派层「取二维码内容」是
/// 独立一步、只拿到会话键。所以这里按 token 记一份扫码地址。
static SCAN_URLS: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

fn remember_scan_url(token: &str, scan_url: &str) {
    if scan_url.trim().is_empty() {
        return;
    }
    if let Ok(mut slot) = SCAN_URLS.lock() {
        slot.get_or_insert_with(HashMap::new)
            .insert(token.to_string(), scan_url.to_string());
    }
}

/// 第 1 步：创建二维码，返回会话键（就是 libresoda 的 `token`）。
///
/// 前提是 libresoda 装好了浏览器签名页——[`client::configure`] 里已经调了
/// `enable_cdp_signer`。它直控本机 Chromium，**不需要 Node**。
pub async fn create_qr_session(api: &ApiClient) -> Result<String> {
    let soda = client::configure(api);
    blocking("创建二维码", move || {
        let created = libresoda::soda::qr_login::create_qr(soda).map_err(convert)?;
        remember_scan_url(&created.token, &created.scan_url);
        tlog!(LEVEL_DEBUG, "汽水二维码已创建");
        Ok(created.token)
    })
    .await
}

/// 第 2 步：取二维码内容。
pub fn scan_url_for(token: &str) -> Result<String> {
    SCAN_URLS
        .lock()
        .ok()
        .and_then(|slot| slot.as_ref().and_then(|map| map.get(token).cloned()))
        .filter(|url| !url.is_empty())
        .ok_or_else(|| AppError::NotFound("汽水二维码会话已过期，请重新按 L".to_string()))
}

/// 第 3 步：轮询一次。
pub async fn check_qr_session(api: &ApiClient, token: &str) -> Result<crate::api::cloud::QrCheck> {
    use crate::api::cloud::{QrCheck, QrStatus};

    let soda = client::configure(api);
    let token_owned = token.to_string();

    let result = blocking("查询扫码状态", move || {
        libresoda::soda::qr_login::check_qr(soda, &token_owned).map_err(convert)
    })
    .await?;

    // 登录成功后扫码地址就没用了，清掉免得越攒越多。
    if matches!(result.status, libresoda::model::QRLoginStatus::Success)
        && let Ok(mut slot) = SCAN_URLS.lock()
        && let Some(map) = slot.as_mut()
    {
        map.remove(token);
    }

    let status = match result.status {
        libresoda::model::QRLoginStatus::Waiting => QrStatus::Waiting,
        // 已扫码待确认（也可能在等短信验证）——界面提示「请在手机上确认」。
        libresoda::model::QRLoginStatus::Scanned => QrStatus::Pending,
        libresoda::model::QRLoginStatus::Success => QrStatus::Success,
        libresoda::model::QRLoginStatus::Expired => QrStatus::Expired,
        // Failed 归到 Expired：对用户来说都是「这张码不能用了，重来」，
        // 而重来是同一个动作。具体原因在 result.message 里。
        libresoda::model::QRLoginStatus::Failed => QrStatus::Expired,
    };

    if matches!(status, QrStatus::Expired) && !result.message.trim().is_empty() {
        tlog!(LEVEL_INFO, "汽水扫码结束：{}", result.message);
    }

    Ok(QrCheck {
        status,
        // 汽水拿不到 token+userid，登录态是服务端下发的**会话 cookie**。
        // 这正是它对 `Capability::client_token` 说「假」的原因。
        token: None,
        userid: None,
        cookie: Some(result.cookie).filter(|cookie| !cookie.trim().is_empty()),
    })
}

// ============================================================================
// 云端歌单 / 歌单曲目 / 歌单广场 / 用户资料 / 会员
// ============================================================================
//
// 这几项走 `/luna/pc/` 一族，**要求应用签名**——这正是它们此前没接的原因，
// 而不是「汽水没有这些接口」。签名服务接上之后就都能用了（见 `client`）。

/// libresoda 的歌单 → 本项目的歌单。
///
/// `list_id` 单独填：本项目的云端增删歌曲要的是一个**数字 id**，而汽水自建
/// 歌单的 id 本来就是数字串。解析失败就留 `None`——那说明这不是一个能写的
/// 歌单，界面据此收起写操作入口。
fn to_playlist(source: &libresoda::Playlist, is_own: bool) -> crate::api::model::Playlist {
    crate::api::model::Playlist {
        id: source.id.clone(),
        list_id: source.id.trim().parse::<i64>().ok(),
        name: source.name.clone(),
        cover: Some(source.cover.clone()).filter(|url| !url.trim().is_empty()),
        song_count: source.track_count.max(0) as u32,
        creator: Some(source.creator.clone()).filter(|name| !name.trim().is_empty()),
        description: Some(source.description.clone()).filter(|text| !text.trim().is_empty()),
        is_own,
    }
}

/// 云端「我的歌单」。
///
/// 需要登录（cookie）；未登录时服务端会拒绝。
///
/// 用官方 PC 客户端走的那条路（`GET /luna/pc/me/playlist`）。注意：
/// 这个接口是 libresoda **较新**的提交才补上的（此前只有按页的老版本），
/// 所以依赖 rev 要跟上。
pub async fn user_playlists(api: &ApiClient) -> Result<Vec<crate::api::model::Playlist>> {
    let soda = client::configure(api);
    blocking("取云端歌单", move || {
        // 一次取 100 条（上游默认 50）：界面另有「加载更多」，但先给足
        // 一次刷完的量，省得来回。`has_more` 非空说明还有下一页。
        let page =
            libresoda::soda::user_playlist::get_my_playlists(soda, "", 100).map_err(convert)?;
        if page.has_more {
            tlog!(LEVEL_DEBUG, "汽水云端歌单还有更多（游标未空）");
        }
        Ok(page
            .playlists
            .iter()
            // 这个端点只回自己创建的歌单，所以 is_own 恒为真
            .map(|playlist| to_playlist(playlist, true))
            .collect())
    })
    .await
}

/// 歌单广场。
///
/// 汽水的「广场」只有一份**推荐歌单**（`GET /luna/me/playlist/recommend`），
/// 没有分类维度，所以忽略 `category_id` 与分页参数——界面切换分类时会一直看到
/// 同一份内容。这比报错更符合预期：用户按的是「换个分类看看」，不是「我要报错」。
///
/// ⚠️ 依赖 rev：这个端点在 libresoda 的旧提交里是 `Err(Unsupported)` 空壳，
/// 升级后才真正可用。所以「歌单广场能不能用」取决于 `Cargo.toml` 里锁的 rev。
pub async fn plaza_playlists(
    api: &ApiClient,
    category_id: i64,
    page: u32,
    page_size: u32,
) -> Result<Vec<crate::api::model::Playlist>> {
    let _ = (category_id, page, page_size);
    let soda = client::configure(api);
    blocking("取推荐歌单", move || {
        let playlists =
            libresoda::soda::playlist::get_recommend_playlists(soda).map_err(convert)?;
        Ok(playlists
            .iter()
            .map(|playlist| to_playlist(playlist, false))
            .collect())
    })
    .await
}

pub async fn playlist_tracks(api: &ApiClient, playlist_id: &str) -> Result<Vec<Song>> {
    let soda = client::configure(api);
    let playlist_id = playlist_id.trim().to_string();
    blocking("取歌单曲目", move || {
        let songs =
            libresoda::soda::playlist::get_playlist_songs(soda, &playlist_id).map_err(convert)?;
        Ok(songs.iter().map(to_song).collect())
    })
    .await
}

/// 取一个歌单的**一页**曲目。
///
/// # 为什么和 [`playlist_tracks`] 是同一个实现
///
/// 汽水的歌单详情是**游标式**的，而 libresoda 暴露的分页入口
/// （`fetch_playlist_detail_page`）回的是**未归一化的原始 `Value`**——它的形状
/// 属于内部结构，照抄一遍就是猜。所以这里直接调公开的 `get_playlist_songs`
/// 一次取全，把 `cursor` 忽略掉。
///
/// 代价是首屏多等一会儿（歌单很大时），换来的是**没有猜出来的解析逻辑**。
/// 界面的「加载更多」在汽水下因此不会真正分页——但也不会重复追加，
/// 因为每次拿到的都是完整列表。
pub async fn playlist_tracks_page(
    api: &ApiClient,
    playlist_id: &str,
    _cursor: &str,
    _count: u32,
) -> Result<Vec<Song>> {
    playlist_tracks(api, playlist_id).await
}

/// 取当前账号的资料（昵称、头像、会员标记）。
pub async fn user_detail(api: &ApiClient) -> Result<crate::api::cloud::UserInfo> {
    let soda = client::configure(api);
    blocking("取用户资料", move || {
        let me = libresoda::soda::user_playlist::fetch_pc_me(soda).map_err(convert)?;
        let info = me.my_info;
        Ok(crate::api::cloud::UserInfo {
            nickname: if info.nickname.trim().is_empty() {
                info.public_name.clone()
            } else {
                info.nickname.clone()
            },
            pic: Some(libresoda::soda::types::build_image_url(
                &info.larger_avatar_url,
                "",
            ))
            .filter(|url| !url.trim().is_empty()),
            // 汽水没有「用户等级」这个概念，也没有累计听歌时长。
            grade: None,
            duration_min: None,
        })
    })
    .await
}

/// 当前账号是不是会员。
///
/// 官方 `/luna/pc/me` 直接给 `is_vip`，比「探测能不能拿整曲」可靠得多——
/// 后者会被签名可用性干扰，把非会员误判成「拿不到整曲」。
pub async fn is_vip_account(api: &ApiClient) -> Result<bool> {
    let soda = client::configure(api);
    blocking("查会员状态", move || {
        libresoda::soda::account::is_vip_account(soda).map_err(convert)
    })
    .await
}

// ============================================================================
// 不支持的能力
// ============================================================================

/// 「本音源暂不支持该能力」的标准错误。
///
/// 明确报错而不是返回空列表：空列表会被读成「网络问题」或「筛选条件不对」，
/// 用户绕一圈才发现根本没这功能。
pub fn unsupported(what: &str) -> AppError {
    AppError::Other(format!("汽水音乐暂不支持{what}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_artists_handles_separators_and_single_names() {
        let singers = split_artists("甲、乙、丙");
        assert_eq!(singers.len(), 3);
        assert_eq!(singers[0].name, "甲");
        assert_eq!(singers[2].name, "丙");

        // 分号与斜杠也认（不同接口给过不同分隔）
        assert_eq!(split_artists("甲; 乙").len(), 2);
        assert_eq!(split_artists("甲 / 乙").len(), 2);

        let one = split_artists("独唱");
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].name, "独唱");

        // 空串与纯分隔符不能产出空元素
        assert!(split_artists("").is_empty());
        assert!(split_artists("   ").is_empty());
        assert!(split_artists("、、").is_empty());
    }

    /// 合成的歌手 id 必须是**负数**：0 在若干接口里表示「缺省/全部」，
    /// 给 0 会让「按歌手筛选」意外命中全部。
    #[test]
    fn synthesized_artist_ids_are_never_zero() {
        for count in 1..5usize {
            let joined = (0..count)
                .map(|index| format!("歌手{index}"))
                .collect::<Vec<_>>()
                .join("、");
            for singer in split_artists(&joined) {
                assert!(singer.id < 0, "占位 id 必须是负数，实际 {}", singer.id);
            }
        }
    }

    /// libresoda 的时长是**秒**，本项目统一用毫秒。
    ///
    /// 回归：不做换算会让「3 分 40 秒」显示成 3 秒，进度条直接失效。
    #[test]
    fn duration_is_converted_from_seconds_to_milliseconds() {
        let source = libresoda::Song {
            id: "t1".to_string(),
            name: "测试".to_string(),
            artist: "甲、乙".to_string(),
            album: "专辑".to_string(),
            duration: 220,
            cover: "https://example.com/c.jpg".to_string(),
            is_vip: true,
            ..Default::default()
        };
        let song = to_song(&source);
        assert_eq!(song.duration_ms, 220_000, "220 秒 = 220000 毫秒");
        assert_eq!(song.hash, "t1");
        assert_eq!(song.name, "测试");
        assert_eq!(song.album_name, "专辑");
        assert_eq!(song.singers.len(), 2);
        assert_eq!(song.privilege, Some(1), "VIP 曲目要打上标记");
        assert_eq!(song.source, crate::source::SourceKind::Sodam);
    }

    /// 负数 / 缺时长不能溢出成天文数字。
    #[test]
    fn negative_duration_saturates_to_zero() {
        let source = libresoda::Song {
            duration: -5,
            ..Default::default()
        };
        assert_eq!(to_song(&source).duration_ms, 0);
    }

    #[test]
    fn empty_cover_becomes_none() {
        let source = libresoda::Song {
            cover: "   ".to_string(),
            ..Default::default()
        };
        assert!(to_song(&source).cover.is_none());
    }

    #[test]
    fn non_vip_song_has_no_privilege_marker() {
        let source = libresoda::Song {
            is_vip: false,
            ..Default::default()
        };
        assert_eq!(to_song(&source).privilege, None);
    }

    /// 两个档位表要对上：本项目是酷狗的名字，libresoda 认汽水的键。
    #[test]
    fn quality_names_map_onto_libresoda_preferences() {
        assert_eq!(quality_preference("128"), "128k");
        assert_eq!(quality_preference("320"), "320k");
        assert_eq!(quality_preference("flac"), "lossless");
        assert_eq!(quality_preference("high"), "high");
        assert_eq!(quality_preference("super"), "highest");
        assert_eq!(quality_preference("FLAC"), "lossless", "大小写不敏感");
        // 酷狗特有的 VIP 音效档位在汽水没有对应概念 → 不限制
        for quality in ["viper_clear", "viper_atmos", "viper_tape", "未知档位"] {
            assert_eq!(quality_preference(quality), "best", "实际：{quality}");
        }
    }

    /// 映射出来的键必须被 libresoda 认得，否则偏好会**静默失效**
    /// （`preference_rank` 对未知键返回 None，等于不限制）。
    #[test]
    fn mapped_preferences_are_recognized_by_libresoda() {
        for quality in crate::config::SUPPORTED_QUALITIES {
            let preference = quality_preference(quality);
            if preference == "best" {
                continue; // "best" 就是「不限制」的合法写法
            }
            assert!(
                libresoda::soda::quality::preference_rank(preference).is_some(),
                "{quality} → {preference} 没有被 libresoda 识别"
            );
        }
    }

    /// 非会员不能按「用户设的档位」去要——那是在索取会员档位。
    /// 这条对齐 SodaM 的 `auto_quality_for`。
    #[test]
    fn non_vip_always_asks_for_auto() {
        for quality in crate::config::SUPPORTED_QUALITIES {
            assert_eq!(
                preference_for_account(false, quality),
                "auto",
                "非会员时 {quality} 也应落到 auto"
            );
        }
    }

    /// 会员才用用户设的档位（无损封顶）。
    #[test]
    fn vip_uses_the_configured_quality() {
        assert_eq!(preference_for_account(true, "flac"), "lossless");
        assert_eq!(preference_for_account(true, "128"), "128k");
        assert_eq!(preference_for_account(true, "super"), "highest");
        // 酷狗特有的档位在汽水没有对应概念 → 不限制
        assert_eq!(preference_for_account(true, "viper_atmos"), "best");
    }

    #[test]
    fn scratch_file_name_is_safe_and_bounded() {
        let name = scratch_file_name("7304719759323564095", "lossless");
        assert!(name.ends_with(".m4a"), "实际：{name}");
        assert!(!name.contains('/') && !name.contains(' '), "实际：{name}");
        assert!(name.len() <= 90, "文件名要限长：{name}");

        let weird = scratch_file_name("id", "a/b c");
        assert!(
            !weird.contains('/') && !weird.contains(' '),
            "实际：{weird}"
        );

        // 不同档位不能撞名，否则切音质会听到上一档的缓存
        assert_ne!(
            scratch_file_name("t", "lossless"),
            scratch_file_name("t", "high")
        );
    }

    #[test]
    fn describe_reason_mentions_vip_for_a_member_track() {
        let info = libresoda::soda::types::DownloadInfo {
            quality: "standard".to_string(),
            bitrate: 128_000,
            is_preview: true,
            ..Default::default()
        };
        let detail = libresoda::Song {
            is_vip: true,
            ..Default::default()
        };
        let reason = describe_reason(true, &info, &detail, None).unwrap();
        assert!(reason.contains("试听"), "实际：{reason}");
        assert!(reason.contains("会员"), "实际：{reason}");
    }

    #[test]
    fn describe_reason_names_the_actual_quality_when_downgraded() {
        let info = libresoda::soda::types::DownloadInfo {
            quality: "standard".to_string(),
            ..Default::default()
        };
        let reason = describe_reason(false, &info, &libresoda::Song::default(), None).unwrap();
        assert!(reason.contains("standard"), "音质降级要如实说明：{reason}");
    }

    /// 一切正常时不该产生提示——否则界面会显示一条空的状态栏消息。
    #[test]
    fn describe_reason_is_none_when_there_is_nothing_to_say() {
        let info = libresoda::soda::types::DownloadInfo::default();
        assert!(describe_reason(false, &info, &libresoda::Song::default(), None).is_none());
    }

    #[test]
    fn unsupported_says_which_capability_is_missing() {
        let message = unsupported("排行榜").to_string();
        assert!(message.contains("不支持"), "实际：{message}");
        assert!(message.contains("排行榜"), "要说清是哪项：{message}");
    }

    /// 会话表按 token 存扫码地址；取不到要给可操作的提示。
    #[test]
    fn scan_url_lookup_explains_expiry() {
        let error = scan_url_for("从未存在过的会话").unwrap_err().to_string();
        assert!(error.contains("过期"), "实际：{error}");
    }

    #[test]
    fn scan_url_round_trips_through_the_session_table() {
        remember_scan_url("tok-1", "https://bff-pc.qishui.com/x?token=1");
        assert_eq!(
            scan_url_for("tok-1").unwrap(),
            "https://bff-pc.qishui.com/x?token=1"
        );
        // 空地址不算记住（否则界面会去渲染一个空二维码）
        remember_scan_url("tok-2", "   ");
        assert!(scan_url_for("tok-2").is_err(), "空扫码地址不能被当成有值");
    }

    /// 搜索信封：重复 id 要去重，缺字段不能 panic。
    #[test]
    fn search_envelope_dedupes_across_groups() {
        let body = r#"{
            "result_groups": [
                {"data": [
                    {"entity": {"track": {"id": "1", "name": "甲"}}},
                    {"entity": {"track": {"id": "2", "name": "乙"}}}
                ]},
                {"data": [
                    {"entity": {"track": {"id": "1", "name": "甲（重复）"}}},
                    {"entity": {"track": {"id": "3", "name": "丙"}}}
                ]}
            ]
        }"#;
        let parsed: SearchResponse = serde_json::from_slice(body.as_bytes()).unwrap();

        let mut seen: Vec<String> = Vec::new();
        let mut names: Vec<String> = Vec::new();
        for group in parsed.result_groups {
            for item in group.data {
                let track = item.entity.track;
                if track.id.is_empty() || seen.contains(&track.id) {
                    continue;
                }
                seen.push(track.id.clone());
                names.push(track.name);
            }
        }
        assert_eq!(names.len(), 3, "重复 id 要被去掉：{names:?}");
        assert_eq!(names[0], "甲", "保留首次出现的");
    }

    #[test]
    fn search_envelope_tolerates_missing_fields() {
        let parsed: SearchResponse = serde_json::from_slice(r#"{}"#.as_bytes()).unwrap();
        assert!(parsed.result_groups.is_empty());

        let parsed: SearchResponse =
            serde_json::from_slice(r#"{"result_groups":[{}]}"#.as_bytes()).unwrap();
        assert!(parsed.result_groups[0].data.is_empty());

        let parsed: SearchResponse =
            serde_json::from_slice(r#"{"result_groups":[{"data":[{}]}]}"#.as_bytes()).unwrap();
        assert!(parsed.result_groups[0].data[0].entity.track.id.is_empty());
    }
}
