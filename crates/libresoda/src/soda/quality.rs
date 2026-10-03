//! 音质档位排序与"候选流择优"逻辑（逐行对应上游 `sodaQualityRank`、
//! `normalizeSodaBitrate`、`sodaBetterStreamCandidate`、`sodaDownloadInfoIsPreview` 等）。

use super::types::{DownloadInfo, PlayerInfo, TrackPlayInfo};
use crate::util::normalize_token;

/// 等价 `normalizeSodaBitrate`：>1000 视为 bps，换算成 kbps。
pub fn normalize_bitrate(bitrate: i64) -> i64 {
    if bitrate > 1000 {
        bitrate / 1000
    } else {
        bitrate
    }
}

/// 等价 `normalizeSodaDuration`：>1000 视为毫秒，换算成秒。
pub fn normalize_duration(duration: f64) -> f64 {
    if duration > 1000.0 {
        duration / 1000.0
    } else {
        duration
    }
}

/// 音质档位评分：无损 100、Hi-Res 且无损 110、杜比 88 …
///
/// 数值越大越好；>=100 视为无损，>=60 视为"够好，不必再走 PC 接口"。
pub fn quality_rank(quality: &str, format: &str, bitrate: i64) -> i64 {
    let q = normalize_token(quality);
    let f = format.trim().to_lowercase();
    let br = normalize_bitrate(bitrate);

    let is_lossless_format = f.contains("flac") || f.contains("alac") || f.contains("wav");
    let is_lossless_label =
        q.contains("lossless") || q.contains("flac") || q.contains("sq") || q.contains("svip");
    let is_hires_label = q.contains("hires") || q.contains("master");

    if is_hires_label && (is_lossless_format || br >= 900) {
        return 110;
    }
    if is_lossless_label || is_lossless_format || br >= 900 {
        return 100;
    }
    if is_hires_label {
        return 90;
    }
    if q.contains("atmos") || q.contains("dolby") || q.contains("spatial") {
        return 88;
    }
    if q.contains("highest")
        || q.contains("excellent")
        || q.contains("superhigh")
        || q.contains("hq")
    {
        return 80;
    }
    if q.contains("higher") || q == "high" || q.contains("320") {
        return 70;
    }
    if q.contains("standard") || q.contains("medium") || q.contains("normal") || q.contains("128") {
        return 50;
    }
    if q.contains("low") || q.contains("preview") {
        return 10;
    }

    match br {
        value if value >= 900 => 100,
        value if value >= 320 => 70,
        value if value >= 256 => 65,
        value if value >= 192 => 55,
        value if value >= 128 => 50,
        value if value > 0 => 20,
        _ => 0,
    }
}

/// 流候选比较：先比时长（谁更完整），再比音质档位、码率、体积。
///
/// 等价 `sodaBetterStreamCandidate`，返回 `true` 表示 `a` 优于 `b`。
#[allow(clippy::too_many_arguments)]
pub fn better_stream_candidate(
    a_duration: f64,
    a_quality: &str,
    a_format: &str,
    a_bitrate: i64,
    a_size: i64,
    b_duration: f64,
    b_quality: &str,
    b_format: &str,
    b_bitrate: i64,
    b_size: i64,
) -> bool {
    if a_duration > 0.0 || b_duration > 0.0 {
        if a_duration > b_duration + 1.0 {
            return true;
        }
        if b_duration > a_duration + 1.0 {
            return false;
        }
    }

    let a_rank = quality_rank(a_quality, a_format, a_bitrate);
    let b_rank = quality_rank(b_quality, b_format, b_bitrate);
    if a_rank != b_rank {
        return a_rank > b_rank;
    }

    let a_br = normalize_bitrate(a_bitrate);
    let b_br = normalize_bitrate(b_bitrate);
    if a_br != b_br {
        return a_br > b_br;
    }
    if a_size != b_size {
        return a_size > b_size;
    }
    a_quality.trim() > b_quality.trim()
}

/// 音质档位偏好：把客户端里的 gear key 映射成「允许的最高档位分数」。
///
/// | 偏好 | 语义 | 允许的最高分 |
/// | --- | --- | --- |
/// | `best` / `auto` / 空 | 永远选最优（默认，等价旧行为） | 不限 |
/// | `hires` | 只到 Hi-Res | 110 |
/// | `lossless` | 无损及以下 | 100 |
/// | `dolby` | 杜比及以下 | 88 |
/// | `highest` | 极高（HQ）及以下 | 80 |
/// | `medium` | 标准音质及以下 | 70 |
/// | `low` | 省流 | 50 |
///
/// 对应客户端 `src/services/player/qualityDefaults.ts` 的 gear key 与
/// `renderer/compositions/quality.ts` 的设置项。
pub fn preference_rank(preference: &str) -> Option<i64> {
    match preference.trim().to_ascii_lowercase().as_str() {
        "" | "best" | "auto" => None,
        "hires" | "hi_res" | "hi-res" => Some(110),
        "lossless" | "flac" => Some(100),
        "dolby" | "dolby_atmos" => Some(88),
        "highest" | "hq" | "high" => Some(80),
        "medium" | "standard" | "320k" => Some(70),
        "low" | "128k" | "saver" => Some(50),
        // 未知档位当作不限制，避免把用户的设置变成"拿不到流"
        _ => None,
    }
}

/// 某条候选流是否落在偏好档位内（偏好为空/`best` 时恒为 `true`）。
pub fn within_preference(quality: &str, format: &str, bitrate: i64, preference: &str) -> bool {
    match preference_rank(preference) {
        None => true,
        Some(cap) => quality_rank(quality, format, bitrate) <= cap,
    }
}

/// 支持音质选择的 `DownloadInfo` 列表筛选：先按偏好过滤，过滤后为空则返回原列表
///（例如用户选"标准音质"但该曲只有无损时，仍然要给得出流）。
pub fn filter_by_preference<T, F>(candidates: &[T], preference: &str, score: F) -> Vec<usize>
where
    F: Fn(&T) -> i64,
{
    let cap = match preference_rank(preference) {
        None => return (0..candidates.len()).collect(),
        Some(cap) => cap,
    };
    let picked: Vec<usize> = candidates
        .iter()
        .enumerate()
        .filter(|(_, item)| score(item) <= cap)
        .map(|(index, _)| index)
        .collect();
    if picked.is_empty() {
        (0..candidates.len()).collect()
    } else {
        picked
    }
}

/// 等价 `sodaDownloadInfoIsLossless`。
pub fn is_lossless(info: &DownloadInfo) -> bool {
    quality_rank(&info.quality, &info.format, info.bitrate) >= 100
}

/// 等价 `sodaDownloadInfoIsPreview`：明显短于整曲时长时视为试听片段。
pub fn is_preview(info: &DownloadInfo, full_duration_seconds: i64) -> bool {
    if info.duration <= 0.0 || full_duration_seconds <= 0 {
        return false;
    }
    info.duration + 5.0 < full_duration_seconds as f64
}

/// 等价 `sodaBestTrackPlayInfo`。
pub fn best_track_play_info(list: &[TrackPlayInfo]) -> Option<TrackPlayInfo> {
    let mut best: Option<TrackPlayInfo> = None;
    for info in list {
        if info.main_play_url.trim().is_empty() && info.backup_play_url.trim().is_empty() {
            continue;
        }
        let replace = match &best {
            None => true,
            Some(current) => better_stream_candidate(
                info.duration as f64,
                &info.quality,
                &info.format,
                info.bitrate,
                info.size,
                current.duration as f64,
                &current.quality,
                &current.format,
                current.bitrate,
                current.size,
            ),
        };
        if replace {
            best = Some(info.clone());
        }
    }
    best
}

/// 等价 `sodaBestPlayerInfo`。
pub fn best_player_info(list: &[PlayerInfo]) -> Option<PlayerInfo> {
    let mut best: Option<PlayerInfo> = None;
    for info in list {
        if info.main_play_url.trim().is_empty() && info.backup_play_url.trim().is_empty() {
            continue;
        }
        let replace = match &best {
            None => true,
            Some(current) => better_stream_candidate(
                info.duration,
                &info.quality,
                &info.format,
                info.bitrate,
                info.size,
                current.duration,
                &current.quality,
                &current.format,
                current.bitrate,
                current.size,
            ),
        };
        if replace {
            best = Some(info.clone());
        }
    }
    best
}

/// 等价 `sodaTrackDurationSeconds`：毫秒值自动换算成秒。
pub fn track_duration_seconds(duration: i64) -> i64 {
    if duration > 1000 {
        duration / 1000
    } else {
        duration
    }
}

/// 等价 `sodaVideoModelQualityHint`：从 key 名中猜音质标签。
pub fn quality_hint(key: &str) -> String {
    let key = key.trim();
    if key.is_empty() {
        return String::new();
    }
    let normalized = normalize_token(key);
    for token in [
        "hires", "lossless", "sq", "flac", "highest", "higher", "standard", "normal",
    ] {
        if normalized.contains(token) {
            return token.to_string();
        }
    }
    String::new()
}

#[cfg(test)]
mod preference_tests {
    use super::*;

    #[test]
    fn preference_rank_maps_client_gears() {
        assert_eq!(preference_rank(""), None);
        assert_eq!(preference_rank("best"), None);
        assert_eq!(preference_rank("lossless"), Some(100));
        assert_eq!(preference_rank("highest"), Some(80));
        assert_eq!(preference_rank("medium"), Some(70));
        assert_eq!(preference_rank("low"), Some(50));
        // 未知档位不限制，避免把用户设置变成"拿不到流"
        assert_eq!(preference_rank("whatever"), None);
    }

    #[test]
    fn within_preference_respects_cap() {
        assert!(within_preference("lossless", "flac", 900_000, "lossless"));
        assert!(!within_preference("lossless", "flac", 900_000, "highest"));
        assert!(within_preference("320k", "mp3", 320_000, "medium"));
        assert!(within_preference("lossless", "flac", 900_000, ""));
    }

    #[test]
    fn filter_by_preference_falls_back_when_nothing_matches() {
        let scores = [110_i64, 100];
        // 只允许 <=80，没有任何候选 → 回退成全量
        assert_eq!(filter_by_preference(&scores, "highest", |s| *s), vec![0, 1]);
        // 允许 <=100 → 只留 100
        assert_eq!(filter_by_preference(&scores, "lossless", |s| *s), vec![1]);
        // 不限制 → 全量
        assert_eq!(filter_by_preference(&scores, "", |s| *s), vec![0, 1]);
    }
}
