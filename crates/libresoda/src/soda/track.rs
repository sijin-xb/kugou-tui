//! 单曲详情、播放地址解析与视频流（video_model）择优。
//!
//! 对应上游 `soda/soda.go` 中 `sodaWebTrackV2URL`、`fetchWebTrackV2`、
//! `fetchSeoTrackData`、`fetchPCTrackV2`、`fetchPlayerInfo`、`fetchSongDetail`、
//! `sodaBuildSongFromTrack`、`applySodaDownloadInfo`、`sodaBestFromVideoModel` 等。

use super::quality::{
    best_player_info, better_stream_candidate, normalize_bitrate, quality_hint,
    track_duration_seconds,
};
use super::types::{
    build_image_url, join_track_artists, max_bitrate_size, track_extra, track_link, DownloadInfo,
    PlayerInfoResponse, SeoTrackResponse, Track, TrackV2Response, USER_AGENT,
};
use super::Soda;
use crate::error::{Result, SodaError};
use crate::http::{self, RequestOption};
use crate::model::Song;
use crate::util::{json_first_string, json_float, json_int, json_object, json_string};
use serde_json::{Map, Value};

/// 等价 `sodaWebTrackV2URL`。
pub fn web_track_v2_url(track_id: &str) -> String {
    let mut params = crate::util::Params::new();
    params.set("track_id", track_id);
    params.set("media_type", "track");
    params.set("aid", "386088");
    params.set("device_platform", "web");
    params.set("channel", "pc_web");
    format!(
        "https://api.qishui.com/luna/pc/track_v2?{}",
        params.encode()
    )
}

/// 等价 `sodaSeoTrackURL`。
pub fn seo_track_url(track_id: &str) -> String {
    let mut params = crate::util::Params::new();
    params.set("track_id", track_id);
    params.set("device_platform", "web");
    format!("{SODA_SEO_BASE}?{}", params.encode())
}

// 常量在 types 模块里定义，这里做局部别名，保持函数体与上游命名一致。
use super::types::SODA_SEO_BASE;

/// 等价 `parseSodaTrackV2Response`。
pub fn parse_track_v2_response(body: &[u8]) -> Result<TrackV2Response> {
    let response: TrackV2Response = serde_json::from_slice(body)
        .map_err(|err| SodaError::json(format!("soda track_v2 json parse error: {err}")))?;
    if response.status_code != 0 {
        return Err(SodaError::Api {
            status_code: response.status_code,
            status_msg: if response.status_info.status_msg.trim().is_empty() {
                "unknown error".to_string()
            } else {
                response.status_info.status_msg.clone()
            },
        });
    }
    Ok(response)
}

/// 等价 `fetchSeoTrackData`。
pub fn fetch_seo_track_data(soda: &Soda, track_id: &str) -> Result<TrackV2Response> {
    let body = http::get(
        &seo_track_url(track_id),
        &[
            RequestOption::new().header("User-Agent", USER_AGENT),
            RequestOption::new().cookie(&soda.cookie()),
        ],
    )?;

    let seo: SeoTrackResponse = serde_json::from_slice(&body)
        .map_err(|err| SodaError::json(format!("soda seo_track json parse error: {err}")))?;
    if seo.status_code != 0 {
        return Err(SodaError::Api {
            status_code: seo.status_code,
            status_msg: if seo.status_info.status_msg.trim().is_empty() {
                "unknown error".to_string()
            } else {
                seo.status_info.status_msg.clone()
            },
        });
    }

    let track = seo.seo_track.track.clone();
    let lyric_content = if !seo.seo_track.lyric.content.trim().is_empty() {
        seo.seo_track.lyric.content.clone()
    } else {
        seo.lyric.content.clone()
    };

    let response = TrackV2Response {
        status_code: 0,
        status_info: Default::default(),
        track: track.clone(),
        track_info: track,
        track_player: seo.track_player.clone(),
        lyric: super::types::LyricBody {
            content: lyric_content,
        },
    };
    if response.track.id.trim().is_empty() {
        return Err(SodaError::not_found("soda seo_track missing track id"));
    }
    Ok(response)
}

/// 等价 `fetchWebTrackV2`（失败或解析失败时回退到 SEO 接口）。
pub fn fetch_web_track_v2(soda: &Soda, track_id: &str) -> Result<TrackV2Response> {
    let body = http::get(
        &web_track_v2_url(track_id),
        &[
            RequestOption::new().header("User-Agent", USER_AGENT),
            RequestOption::new().cookie(&soda.cookie()),
        ],
    );

    match body {
        Ok(body) => match parse_track_v2_response(&body) {
            Ok(response) => Ok(response),
            Err(parse_err) => fetch_seo_track_data(soda, track_id).map_err(|seo_err| {
                SodaError::http(format!(
                    "soda track_v2 parse failed: {parse_err} (seo fallback: {seo_err})"
                ))
            }),
        },
        Err(http_err) => fetch_seo_track_data(soda, track_id).map_err(|seo_err| {
            SodaError::http(format!(
                "soda track_v2 failed: {http_err} (seo fallback: {seo_err})"
            ))
        }),
    }
}

/// 等价 `fetchPCTrackV2`（需要 Cookie）。
pub fn fetch_pc_track_v2(soda: &Soda, track_id: &str) -> Result<TrackV2Response> {
    if !soda.has_cookie() {
        return Err(SodaError::invalid_input("soda pc track_v2 requires cookie"));
    }

    let payload = serde_json::json!({
        "track_id": track_id,
        "media_type": "track",
        "queue_type": "favorite_track_playlist",
        "scene_name": "library",
    });
    let body = serde_json::to_vec(&payload)
        .map_err(|err| SodaError::json(format!("soda pc track_v2 json encode error: {err}")))?;

    let credentials = soda.app_credentials();
    let mut options = super::pc_request_options(soda);
    options.push(
        RequestOption::new()
            .header("Content-Type", "application/json; charset=utf-8")
            // 客户端会给每个 App 请求带上 body 的 MD5（大写）；应用签名覆盖它，
            // 所以要和 body 一起算，并且一起发出去。
            .header("X-SS-STUB", super::qr_login::md5_hex_upper(&body)),
    );
    let mut url = super::pc_track_v2_url_with(credentials.as_ref());

    // 整曲端点要求**逐请求**的应用签名头（x-helios / x-medusa）：抓包实测同一对
    // 签名换 body（甚至只改键顺序）就会被判空响应。因此这里把真实签名交给
    // SignatureProvider —— Windows/macOS 上的桥接器算完回填即可。
    let body_text = String::from_utf8_lossy(&body).to_string();
    if let Some(signature) =
        super::signature::apply_stream_signature(soda, &url, &body_text, &mut options)
    {
        url = signature;
    }

    let response = http::post_json(&url, &body, &options)?;
    if response.is_empty() {
        // 服务端对"缺少应用级签名头"的请求就是这么回的：HTTP 200 + 0 字节。
        // 裸调看起来像接口下线，其实是风控把 body 挖空了。
        return Err(SodaError::http(
            if credentials
                .as_ref()
                .map(|value| value.is_complete())
                .unwrap_or(false)
            {
                "soda pc track_v2 returned empty body: 应用签名凭证可能已过期，请重新抓包更新 x-helios / x-medusa"
            } else {
                "soda pc track_v2 returned empty body: 缺少应用级签名头（x-helios / x-medusa），整曲取流需要 set_app_credentials()；见 docs/FULL-QUALITY-STREAM.md"
            },
        ));
    }
    parse_track_v2_response(&response)
}

/// 等价 `fetchPlayerInfo`。
pub fn fetch_player_info(soda: &Soda, player_info_url: &str) -> Result<DownloadInfo> {
    let body = http::get(
        player_info_url,
        &[
            RequestOption::new().header("User-Agent", USER_AGENT),
            RequestOption::new().cookie(&soda.cookie()),
        ],
    )?;

    let parsed: PlayerInfoResponse = serde_json::from_slice(&body)
        .map_err(|err| SodaError::json(format!("parse play info response error: {err}")))?;

    let list = parsed.result.data.play_info_list;
    if list.is_empty() {
        let message = parsed.response_metadata.error.message;
        if !message.trim().is_empty() {
            return Err(SodaError::not_found(message));
        }
        return Err(SodaError::not_found("no audio stream found"));
    }

    let best =
        best_player_info(&list).ok_or_else(|| SodaError::not_found("invalid download url"))?;
    let mut download_url = best.main_play_url.clone();
    if download_url.trim().is_empty() {
        download_url = best.backup_play_url.clone();
    }
    if download_url.trim().is_empty() {
        return Err(SodaError::not_found("invalid download url"));
    }

    Ok(DownloadInfo {
        url: download_url,
        play_auth: best.play_auth,
        format: best.format,
        size: best.size,
        duration: best.duration,
        bitrate: best.bitrate,
        quality: best.quality,
        ..Default::default()
    })
}

/// 等价 `fetchSongDetail`。
pub fn fetch_song_detail(soda: &Soda, track_id: &str) -> Result<Song> {
    let response = fetch_web_track_v2(soda, track_id)?;
    let track = response.primary_track();
    if track.id.is_empty() {
        return Err(SodaError::not_found("track info not found"));
    }

    let mut song = build_song_from_track(&track);
    if let Ok(info) = super::download::resolve_download_info(soda, &track.id, Some(&response)) {
        apply_download_info(&mut song, &info);
    }
    Ok(song)
}

/// 等价 `sodaBuildSongFromTrack`。
pub fn build_song_from_track(track: &Track) -> Song {
    let mut display_size = max_bitrate_size(&track.bit_rates);
    let preview_size = max_bitrate_size(&track.preview.bit_rates);
    if preview_size > display_size {
        display_size = preview_size;
    }

    let duration = track_duration_seconds(track.duration);
    let bitrate = if duration > 0 && display_size > 0 {
        display_size * 8 / 1000 / duration
    } else {
        0
    };

    let album_id = track.album.id.trim().to_string();
    let artist = join_track_artists(&track.artists);
    let artist_id = track
        .artists
        .first()
        .map(|a| a.id.trim())
        .unwrap_or_default()
        .to_string();

    let mut song = Song {
        source: crate::model::SOURCE_SODA.to_string(),
        id: track.id.clone(),
        name: track.name.clone(),
        artist,
        album: track.album.name.clone(),
        album_id: album_id.clone(),
        duration,
        size: display_size,
        bitrate,
        cover: build_image_url(&track.album.url_cover, "~c5_375x375.jpg"),
        link: track_link(&track.id),
        extra: track_extra(
            &track.id,
            &track.label_info,
            &[
                ("album_id", album_id.as_str()),
                ("artist_id", artist_id.as_str()),
            ],
        ),
        is_vip: track.label_info.is_vip(),
        ..Default::default()
    };

    if let Some(best) = super::quality::best_track_play_info(&track.audio_info.play_info_list) {
        let mut download_url = best.main_play_url.trim().to_string();
        if download_url.is_empty() {
            download_url = best.backup_play_url.trim().to_string();
        }
        if !download_url.is_empty() {
            song.url = download_url;
            if !best.play_auth.trim().is_empty() {
                song.url.push_str(&format!(
                    "#auth={}",
                    crate::util::query_escape(&best.play_auth)
                ));
            }
            if best.size > song.size {
                song.size = best.size;
            }
            if !best.format.trim().is_empty() {
                song.ext = best.format.clone();
            }
            if best.bitrate > 0 {
                song.bitrate = normalize_bitrate(best.bitrate);
            }
            if !best.quality.trim().is_empty() {
                song.extra_set("quality", best.quality.trim().to_string());
            }
        }
    }

    song
}

/// 等价 `applySodaDownloadInfo`。
pub fn apply_download_info(song: &mut Song, info: &DownloadInfo) {
    let download_url = info.full_url();
    if !download_url.is_empty() {
        song.url = download_url;
    }
    if info.size > 0 {
        song.size = info.size;
    }
    if !info.format.trim().is_empty() {
        song.ext = info.format.clone();
    }
    if info.bitrate > 0 {
        song.bitrate = normalize_bitrate(info.bitrate);
    } else if song.duration > 0 && info.size > 0 {
        song.bitrate = info.size * 8 / 1000 / song.duration;
    }
    if info.duration > 0.0 && song.duration == 0 {
        song.duration = (info.duration + 0.5) as i64;
    }
    if !info.quality.trim().is_empty() {
        song.extra_set("quality", info.quality.trim().to_string());
        song.extra_set("download_quality", info.quality.trim().to_string());
    }
}

/// video_model 里的单条流描述（等价 `sodaVideoModelEntry`）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VideoModelEntry {
    pub main_play_url: String,
    pub backup_play_url: String,
    pub play_auth: String,
    pub size: i64,
    pub format: String,
    pub bitrate: i64,
    pub quality: String,
    pub duration: f64,
}

/// 等价 `sodaBestFromVideoModel`。
pub fn best_from_video_model(raw: &Value) -> Option<DownloadInfo> {
    best_from_video_model_with_preference(raw, "")
}

/// 带音质偏好的版本：只在「不超过偏好档位」的候选里挑最优；
/// 该档位没有候选时回退到整体最优（等价上游行为）。
pub fn best_from_video_model_with_preference(
    raw: &Value,
    preference: &str,
) -> Option<DownloadInfo> {
    let mut value = raw.clone();
    // 上游会把「被再编码成字符串的 JSON」连续解 3 层
    for _ in 0..3 {
        match &value {
            Value::String(text) => match serde_json::from_str::<Value>(text) {
                Ok(parsed) => value = parsed,
                Err(_) => break,
            },
            _ => break,
        }
    }
    if value.is_null() {
        return None;
    }

    let mut entries: Vec<VideoModelEntry> = Vec::new();
    collect_video_model_entries(&value, "", "", 0.0, &mut entries);

    // 先剔除没有播放地址的条目，再按音质偏好筛选（偏好为空 = 不限制）。
    let usable: Vec<VideoModelEntry> = entries
        .into_iter()
        .filter(|entry| {
            !(entry.main_play_url.trim().is_empty() && entry.backup_play_url.trim().is_empty())
        })
        .collect();
    let allowed: Vec<usize> = super::quality::filter_by_preference(&usable, preference, |entry| {
        super::quality::quality_rank(&entry.quality, &entry.format, entry.bitrate)
    });
    let candidates: Vec<VideoModelEntry> = allowed
        .into_iter()
        .filter_map(|index| usable.get(index).cloned())
        .collect();

    let mut best: Option<VideoModelEntry> = None;
    for entry in candidates {
        let replace = match &best {
            None => true,
            Some(current) => better_stream_candidate(
                entry.duration,
                &entry.quality,
                &entry.format,
                entry.bitrate,
                entry.size,
                current.duration,
                &current.quality,
                &current.format,
                current.bitrate,
                current.size,
            ),
        };
        if replace {
            best = Some(entry);
        }
    }

    let best = best?;
    let mut download_url = best.main_play_url.trim().to_string();
    if download_url.is_empty() {
        download_url = best.backup_play_url.trim().to_string();
    }
    if download_url.is_empty() {
        return None;
    }

    Some(DownloadInfo {
        url: download_url,
        play_auth: best.play_auth.trim().to_string(),
        format: best.format.trim().to_string(),
        size: best.size,
        duration: best.duration,
        bitrate: best.bitrate,
        quality: best.quality.trim().to_string(),
        ..Default::default()
    })
}

/// 等价 `sodaCollectVideoModelEntries`。
pub fn collect_video_model_entries(
    value: &Value,
    key_hint: &str,
    inherited_auth: &str,
    inherited_duration: f64,
    entries: &mut Vec<VideoModelEntry>,
) {
    match value {
        Value::Object(map) => {
            let mut auth = inherited_auth.trim().to_string();
            let own_auth = video_model_play_auth(map);
            if !own_auth.is_empty() {
                auth = own_auth;
            }

            let mut duration = inherited_duration;
            let own_duration = json_float(map, &["video_duration", "duration", "Duration"]);
            if own_duration > 0.0 {
                duration = super::quality::normalize_duration(own_duration);
            }

            if let Some(entry) = video_model_entry_from_map(map, key_hint, &auth, duration) {
                entries.push(entry);
            }
            for (key, child) in map {
                collect_video_model_entries(child, key, &auth, duration, entries);
            }
        }
        Value::Array(items) => {
            for child in items {
                collect_video_model_entries(
                    child,
                    key_hint,
                    inherited_auth,
                    inherited_duration,
                    entries,
                );
            }
        }
        _ => {}
    }
}

fn video_model_entry_from_map(
    values: &Map<String, Value>,
    key_hint: &str,
    inherited_auth: &str,
    inherited_duration: f64,
) -> Option<VideoModelEntry> {
    let mut entry = VideoModelEntry {
        main_play_url: json_string(
            values,
            &[
                "main_play_url",
                "MainPlayUrl",
                "main_url",
                "MainUrl",
                "url",
                "URL",
                "play_url",
                "PlayURL",
            ],
        ),
        backup_play_url: json_string(
            values,
            &[
                "backup_play_url",
                "BackupPlayUrl",
                "backup_url",
                "BackupUrl",
                "backup_url_1",
                "backup_url_2",
                "backup_url_3",
            ],
        ),
        play_auth: json_string(values, &["play_auth", "PlayAuth"]),
        size: crate::util::json_int(
            values,
            &[
                "size",
                "Size",
                "file_size",
                "FileSize",
                "data_size",
                "DataSize",
            ],
        ),
        format: json_string(
            values,
            &[
                "format",
                "Format",
                "vtype",
                "VType",
                "file_format",
                "FileFormat",
            ],
        ),
        bitrate: json_int(
            values,
            &["bitrate", "Bitrate", "br", "BR", "bit_rate", "BitRate"],
        ),
        quality: json_string(
            values,
            &[
                "quality",
                "Quality",
                "definition",
                "Definition",
                "quality_type",
                "QualityType",
            ],
        ),
        duration: json_float(values, &["duration", "Duration"]),
    };

    if let Some(meta) = json_object(values, &["video_meta"]) {
        if entry.size == 0 {
            entry.size = crate::util::json_int(meta, &["size", "Size", "file_size", "FileSize"]);
        }
        if entry.format.is_empty() {
            entry.format = json_string(
                meta,
                &[
                    "format",
                    "Format",
                    "vtype",
                    "VType",
                    "codec_type",
                    "CodecType",
                ],
            );
        }
        if entry.bitrate == 0 {
            entry.bitrate = json_int(
                meta,
                &[
                    "bitrate",
                    "Bitrate",
                    "real_bitrate",
                    "RealBitrate",
                    "bit_rate",
                    "BitRate",
                ],
            );
        }
        if entry.quality.is_empty() {
            entry.quality = json_string(
                meta,
                &[
                    "quality",
                    "Quality",
                    "definition",
                    "Definition",
                    "quality_type",
                    "QualityType",
                ],
            );
        }
        if entry.duration == 0.0 {
            entry.duration = json_float(meta, &["duration", "Duration"]);
        }
    }

    if entry.backup_play_url.is_empty() {
        entry.backup_play_url = json_first_string(
            values,
            &["backup_urls", "backupUrls", "url_list", "UrlList"],
        );
    }
    if entry.play_auth.is_empty() {
        entry.play_auth = video_model_play_auth(values);
    }
    if entry.play_auth.is_empty() {
        entry.play_auth = inherited_auth.trim().to_string();
    }
    if entry.quality.is_empty() {
        entry.quality = quality_hint(&json_string(values, &["gear_des_key", "GearDesKey"]));
    }
    if entry.quality.is_empty() {
        entry.quality = quality_hint(key_hint);
    }
    if entry.duration == 0.0 {
        entry.duration = inherited_duration;
    }

    if entry.main_play_url.trim().is_empty() && entry.backup_play_url.trim().is_empty() {
        return None;
    }
    Some(entry)
}

fn video_model_play_auth(values: &Map<String, Value>) -> String {
    for key in ["encrypt_info", "EncryptInfo", "encryptInfo"] {
        let Some(child) = json_object(values, &[key]) else {
            continue;
        };
        let auth = json_string(
            child,
            &["spade_a", "SpadeA", "spadeA", "play_auth", "PlayAuth"],
        );
        if !auth.is_empty() {
            return auth;
        }
    }
    String::new()
}
