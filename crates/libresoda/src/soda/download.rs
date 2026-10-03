//! 下载与播放流解析（对应上游 `soda/download.go`）。
//!
//! 关键点：VIP 曲目 / 试听片段 / 非无损时，会带上 Cookie 走 PC 接口拿更好的流；
//! 加密流（带 `play_auth`）在下载时本地解密。

use super::quality::{is_lossless, is_preview, quality_rank, track_duration_seconds};
use super::track::{
    best_from_video_model_with_preference, fetch_pc_track_v2, fetch_player_info, fetch_web_track_v2,
};
use super::types::DownloadInfo;
use super::Soda;
use crate::error::{Result, SodaError};
use crate::model::{Song, SOURCE_SODA};
use std::path::Path;

/// 等价 `GetDownloadInfo`。
pub fn get_download_info(soda: &Soda, song: &Song) -> Result<DownloadInfo> {
    if !song.source.is_empty() && song.source != SOURCE_SODA {
        return Err(SodaError::invalid_input("source mismatch"));
    }

    let track_id = song_track_id(song);
    let cached = cached_download_info(song);
    if !track_id.is_empty() {
        match resolve_download_info(soda, &track_id, None) {
            Ok(info) => return Ok(info),
            Err(err) => {
                if let Some(cached) = cached {
                    if !cached.url.is_empty() {
                        return Ok(cached);
                    }
                }
                return Err(err);
            }
        }
    }
    if let Some(cached) = cached {
        if !cached.url.is_empty() {
            return Ok(cached);
        }
    }
    Err(SodaError::invalid_input("track id is empty"))
}

/// 等价 `sodaCachedDownloadInfo`：从 `<url>#auth=<play_auth>` 恢复流量信息。
pub fn cached_download_info(song: &Song) -> Option<DownloadInfo> {
    let (url, auth) = song.url.split_once("#auth=")?;
    let play_auth = crate::util::query_unescape(auth).unwrap_or_else(|| auth.to_string());
    let quality = song
        .extra_get("quality")
        .map(|value| value.trim().to_string())
        .unwrap_or_default();
    Some(DownloadInfo {
        url: url.to_string(),
        play_auth,
        format: song.ext.clone(),
        size: song.size,
        duration: song.duration as f64,
        bitrate: song.bitrate,
        quality,
        ..Default::default()
    })
}

/// 等价 `sodaSongTrackID`。
pub fn song_track_id(song: &Song) -> String {
    if let Some(track_id) = song.extra_get("track_id") {
        let track_id = track_id.trim();
        if !track_id.is_empty() {
            return track_id.to_string();
        }
    }
    song.id.trim().to_string()
}

/// 等价 `resolveDownloadInfo`。
pub fn resolve_download_info(
    soda: &Soda,
    track_id: &str,
    web_response: Option<&super::types::TrackV2Response>,
) -> Result<DownloadInfo> {
    let track_id = track_id.trim();
    if track_id.is_empty() {
        return Err(SodaError::invalid_input("track id is empty"));
    }

    let owned_response;
    let response = match web_response {
        Some(response) => response,
        None => {
            owned_response = fetch_web_track_v2(soda, track_id)?;
            &owned_response
        }
    };

    let track = response.primary_track();
    let mut full_duration = track_duration_seconds(track.duration);
    let mut is_vip_track = track.label_info.is_vip();

    let mut last_err: Option<SodaError> = None;

    let preference = soda.quality_preference();
    let mut web_info: Option<DownloadInfo> = response
        .track_player
        .video_model
        .as_ref()
        .and_then(|model| best_from_video_model_with_preference(model, &preference));

    let web_info_is_short = web_info
        .as_ref()
        .map(|info| is_preview(info, full_duration))
        .unwrap_or(false);
    if (web_info.is_none() || web_info_is_short)
        && !response.track_player.url_player_info.is_empty()
    {
        match fetch_player_info(soda, &response.track_player.url_player_info) {
            Ok(info) => web_info = Some(info),
            Err(err) => last_err = Some(err),
        }
    }

    let web_is_preview = web_info
        .as_ref()
        .map(|info| is_preview(info, full_duration))
        .unwrap_or(false);

    let mut should_try_pc = soda.has_cookie()
        && (is_vip_track || web_is_preview || !web_info.as_ref().map(is_lossless).unwrap_or(false));

    if let Some(info) = &web_info {
        if !is_vip_track
            && !web_is_preview
            && quality_rank(&info.quality, &info.format, info.bitrate) >= 60
        {
            should_try_pc = false;
        }
    }

    if should_try_pc {
        match fetch_pc_track_v2(soda, track_id) {
            Ok(pc_response) => {
                let pc_track = pc_response.primary_track();
                if full_duration == 0 {
                    full_duration = track_duration_seconds(pc_track.duration);
                }
                if pc_track.label_info.is_vip() {
                    is_vip_track = true;
                }

                if let Some(info) = pc_response
                    .track_player
                    .video_model
                    .as_ref()
                    .and_then(|model| best_from_video_model_with_preference(model, &preference))
                {
                    if !is_preview(&info, full_duration) {
                        if is_vip_track {
                            soda.set_cached_vip(true);
                        }
                        return Ok(annotate(info, full_duration, ""));
                    }
                }

                if !pc_response.track_player.url_player_info.is_empty() {
                    match fetch_player_info(soda, &pc_response.track_player.url_player_info) {
                        Ok(info) => {
                            if !is_preview(&info, full_duration) {
                                if is_vip_track {
                                    soda.set_cached_vip(true);
                                }
                                return Ok(annotate(info, full_duration, ""));
                            }
                            last_err = Some(SodaError::not_found(
                                "soda pc track_v2 returned preview stream",
                            ));
                        }
                        Err(err) => last_err = Some(err),
                    }
                } else {
                    last_err = Some(SodaError::not_found(
                        "soda pc track_v2 missing player info url",
                    ));
                }
            }
            Err(err) => last_err = Some(err),
        }
    }

    if let Some(info) = web_info {
        if !info.url.is_empty() {
            if is_vip_track && web_is_preview {
                if soda.has_cookie() {
                    soda.set_cached_vip(false);
                }
                // 完整流（需签名或 VIP 账号）拿不到时，回退到平台允许的预览流。
                return Ok(annotate(
                    info,
                    full_duration,
                    &preview_note(soda, &last_err),
                ));
            }
            return Ok(annotate(info, full_duration, ""));
        }
    }

    if let Some(err) = last_err {
        return Err(err);
    }
    Err(SodaError::not_found("player info url not found"))
}

/// 标注试听/整曲，避免下游把 30 秒试听当成整曲写进缓存。
fn annotate(mut info: DownloadInfo, full_duration: i64, note: &str) -> DownloadInfo {
    info.is_preview = is_preview(&info, full_duration);
    if !note.trim().is_empty() {
        info.note = note.trim().to_string();
    } else if info.is_preview {
        info.note = "试听片段".to_string();
    }
    info
}

/// 试听片段的成因说明：优先说明"缺应用签名凭证"，其次说明凭证过期。
fn preview_note(soda: &Soda, last_err: &Option<SodaError>) -> String {
    let credentials_complete = soda
        .app_credentials()
        .map(|credentials| credentials.is_complete())
        .unwrap_or(false);
    let base = if credentials_complete {
        "试听片段：应用签名凭证可能已过期，或账号对该曲目没有整曲权益"
    } else {
        "试听片段：整曲需要应用签名凭证（x-helios / x-medusa），见 docs/FULL-QUALITY-STREAM.md"
    };
    match last_err {
        Some(err) => format!("{base}（App 端点：{err}）"),
        None => base.to_string(),
    }
}

/// 等价 `GetDownloadURL`。
pub fn get_download_url(soda: &Soda, song: &Song) -> Result<String> {
    let info = get_download_info(soda, song)?;
    Ok(info.full_url())
}

/// 等价 `Download`：拉取流 →（必要时）解密 → 写文件。
pub fn download(soda: &Soda, song: &Song, output_path: &Path) -> Result<()> {
    download_with_info(soda, song, output_path).map(|_| ())
}

/// 同 [`download`]，但把**这次实际拉取的流信息**（音质 / 格式 / 码率 / 是否试听）
/// 一并返回，方便客户端显示「当前实际是什么档位」。
pub fn download_with_info(soda: &Soda, song: &Song, output_path: &Path) -> Result<DownloadInfo> {
    let info = get_download_info(soda, song)?;
    let url = info.url.trim();
    if url.is_empty() {
        return Err(SodaError::invalid_input("invalid download url"));
    }

    let body = crate::http::get(
        url,
        &[crate::http::RequestOption::new().header("User-Agent", super::types::USER_AGENT)],
    )?;

    // SEO url_player_info 对免费歌曲返回明文 m4a（无 play_auth），无需解密；
    // Android 签名/加密流才有 play_auth，此时必须解密。
    let data = if info.play_auth.trim().is_empty() {
        body
    } else {
        super::crypto::decrypt_audio(&body, &info.play_auth)
            .map_err(|err| SodaError::crypto(format!("decrypt failed: {err}")))?
    };

    if let Some(parent) = output_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::write(output_path, data)?;
    Ok(info)
}

impl Soda {
    /// 等价 `(*Soda).GetDownloadInfo`。
    pub fn get_download_info(&self, song: &Song) -> Result<DownloadInfo> {
        get_download_info(self, song)
    }

    /// 等价 `(*Soda).GetDownloadURL`。
    pub fn get_download_url(&self, song: &Song) -> Result<String> {
        get_download_url(self, song)
    }

    /// 等价 `(*Soda).Download`。
    pub fn download(&self, song: &Song, output_path: &Path) -> Result<()> {
        download(self, song, output_path)
    }

    /// 同 [`Soda::download`]，额外返回实际拉取的流信息（音质/格式/码率）。
    pub fn download_with_info(&self, song: &Song, output_path: &Path) -> Result<DownloadInfo> {
        download_with_info(self, song, output_path)
    }
}
