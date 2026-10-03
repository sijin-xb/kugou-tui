//! 歌词：汽水"逐字歌词"转标准 LRC（对应上游 `soda/lyric.go` + `parseSodaLyric`）。

use super::track::fetch_web_track_v2;
use super::Soda;
use crate::error::{Result, SodaError};
use crate::model::{Song, SOURCE_SODA};

/// 等价 `GetLyrics`。
pub fn get_lyrics(soda: &Soda, song: &Song) -> Result<String> {
    if !song.source.is_empty() && song.source != SOURCE_SODA {
        return Err(SodaError::invalid_input("source mismatch"));
    }

    let track_id = song
        .extra_get("track_id")
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(song.id.as_str());

    let response = fetch_web_track_v2(soda, track_id)?;
    if response.lyric.content.trim().is_empty() {
        return Ok(String::new());
    }
    Ok(parse_soda_lyric(&response.lyric.content))
}

/// 等价 `parseSodaLyric`：`[起始毫秒,时长]歌词<逐字时间>词</...>` → `[mm:ss.cc]歌词`。
pub fn parse_soda_lyric(raw: &str) -> String {
    let mut out = String::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((start_time, content)) = parse_time_tag(line) else {
            continue;
        };
        let clean_content = strip_angle_tags(&content);
        let minutes = start_time / 60_000;
        let seconds = (start_time % 60_000) / 1000;
        let centis = (start_time % 1000) / 10;
        out.push_str(&format!(
            "[{:02}:{:02}.{:02}]{}\n",
            minutes, seconds, centis, clean_content
        ));
    }
    out
}

/// 解析 `[start,duration]内容`，返回 (start_ms, 内容)。
fn parse_time_tag(line: &str) -> Option<(i64, String)> {
    let rest = line.strip_prefix('[')?;
    let (start, rest) = rest.split_once(',')?;
    let (_, content) = rest.split_once(']')?;
    let start: i64 = start.trim().parse().ok()?;
    Some((start, content.to_string()))
}

/// 去掉 `<...>` 逐字标记（保留纯歌词文本）。
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

impl Soda {
    /// 等价 `(*Soda).GetLyrics`。
    pub fn get_lyrics(&self, song: &Song) -> Result<String> {
        get_lyrics(self, song)
    }
}
