//! 分享链接 / id 解析（对应上游 `sodaExtract*ID*`、`isSodaDigits`）。

use crate::util::{is_digits, query_unescape};

/// 等价 `sodaExtractAlbumID`。
pub fn extract_album_id(link: &str) -> String {
    if let Some(value) = find_param_digits(link, "album_id") {
        return value;
    }
    if let Some(value) = find_param_digits(link, "id") {
        return value;
    }
    if let Some(value) = find_after_marker(link, "album/") {
        return value;
    }

    let trimmed = link.trim();
    if trimmed.len() > 10 && !trimmed.contains('/') {
        return trimmed.to_string();
    }
    String::new()
}

/// 等价 `sodaExtractPlaylistIDFromText`。
pub fn extract_playlist_id(text: &str) -> String {
    let text = text.trim();
    if !text.is_empty() && is_digits(text) && !text.contains('/') {
        return text.to_string();
    }

    let mut candidates = vec![text.to_string()];
    if let Some(decoded) = query_unescape(text) {
        if decoded != text {
            candidates.push(decoded);
        }
    }

    for candidate in &candidates {
        for key in ["playlist_id", "playlistId"] {
            if let Some(value) = find_param_digits(candidate, key) {
                return value;
            }
            if let Some(value) = find_json_digits(candidate, key) {
                return value;
            }
        }
        for marker in [
            "playlist/",
            "/playlist/",
            "%2fplaylist%2f",
            "%2Fplaylist%2F",
        ] {
            if let Some(value) = find_after_marker_ci(candidate, marker) {
                return value;
            }
        }
    }
    String::new()
}

/// 等价 `sodaExtractTrackIDFromText`（汽水 track id 至少 10 位数字）。
pub fn extract_track_id(text: &str) -> String {
    let text = text.trim();
    if text.len() > 10 && is_digits(text) && !text.contains('/') {
        return text.to_string();
    }

    let mut candidates = vec![text.to_string()];
    if let Some(decoded) = query_unescape(text) {
        if decoded != text {
            candidates.push(decoded);
        }
    }

    for candidate in &candidates {
        if let Some(value) = find_param_digits_min(candidate, "track_id", 10) {
            return value;
        }
        if let Some(value) = find_json_digits_min(candidate, "track_id", 10) {
            return value;
        }
        for marker in [
            "/track/",
            "/song/",
            "%2Ftrack%2F",
            "%2Fsong%2F",
            "%2ftrack%2f",
            "%2fsong%2f",
        ] {
            if let Some(value) = find_after_marker_min(candidate, marker, 10) {
                return value;
            }
        }
        for marker in ["track/", "song/"] {
            if let Some(value) = find_after_marker_min(candidate, marker, 10) {
                return value;
            }
        }
    }
    String::new()
}

fn digits_at(value: &str, start: usize, min_len: usize) -> Option<String> {
    let bytes = value.as_bytes();
    let mut end = start;
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    if end - start >= min_len {
        Some(value[start..end].to_string())
    } else {
        None
    }
}

/// 找 `key=digits`（要求 `key` 位于串首或前面是 `?`/`&`）。
fn find_param_digits_min(text: &str, key: &str, min_len: usize) -> Option<String> {
    let mut search_from = 0usize;
    while let Some(index) = text[search_from..].find(key) {
        let absolute = search_from + index;
        let prefix_ok = absolute == 0 || matches!(text.as_bytes()[absolute - 1], b'?' | b'&');
        let after = absolute + key.len();
        if prefix_ok && text.as_bytes().get(after) == Some(&b'=') {
            if let Some(value) = digits_at(text, after + 1, min_len) {
                return Some(value);
            }
        }
        search_from = absolute + key.len();
    }
    None
}

fn find_param_digits(text: &str, key: &str) -> Option<String> {
    find_param_digits_min(text, key, 1)
}

/// 找 `"key": "digits"` 或 `"key":"digits"`。
fn find_json_digits_min(text: &str, key: &str, min_len: usize) -> Option<String> {
    let needle = format!("\"{key}\"");
    let mut search_from = 0usize;
    while let Some(index) = text[search_from..].find(&needle) {
        let absolute = search_from + index;
        let mut cursor = absolute + needle.len();
        let bytes = text.as_bytes();
        while cursor < bytes.len() && (bytes[cursor] == b' ' || bytes[cursor] == b'\t') {
            cursor += 1;
        }
        if bytes.get(cursor) == Some(&b':') {
            cursor += 1;
            while cursor < bytes.len() && (bytes[cursor] == b' ' || bytes[cursor] == b'\t') {
                cursor += 1;
            }
            if bytes.get(cursor) == Some(&b'"') {
                cursor += 1;
            }
            if let Some(value) = digits_at(text, cursor, min_len) {
                return Some(value);
            }
        }
        search_from = absolute + needle.len();
    }
    None
}

fn find_json_digits(text: &str, key: &str) -> Option<String> {
    find_json_digits_min(text, key, 1)
}

fn find_after_marker_min(text: &str, marker: &str, min_len: usize) -> Option<String> {
    let index = text.find(marker)?;
    digits_at(text, index + marker.len(), min_len)
}

fn find_after_marker(text: &str, marker: &str) -> Option<String> {
    find_after_marker_min(text, marker, 1)
}

fn find_after_marker_ci(text: &str, marker: &str) -> Option<String> {
    let lower_text = text.to_lowercase();
    let lower_marker = marker.to_lowercase();
    let index = lower_text.find(&lower_marker)?;
    digits_at(text, index + marker.len(), 1)
}
