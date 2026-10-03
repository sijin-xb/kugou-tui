//! 搜索与链接解析入口（对应上游 `soda/song.go`）。

use super::search::{fetch_android_search, parse_track_search};
use super::track::{build_song_from_track, fetch_song_detail};
use super::types::{SODA_ANDROID_SEARCH_PAGE_SIZE, USER_AGENT};
use super::Soda;
use crate::error::{Result, SodaError};
use crate::http::{self, RequestOption};
use crate::model::Song;
use crate::soda::link::extract_track_id;

/// 等价 `(*Soda).Search`。
pub fn search(soda: &Soda, keyword: &str) -> Result<Vec<Song>> {
    let body = fetch_android_search(soda, "track", keyword, 1, SODA_ANDROID_SEARCH_PAGE_SIZE)?;
    let tracks = parse_track_search(&body)?;
    Ok(tracks.iter().map(build_song_from_track).collect())
}

/// 等价 `(*Soda).Parse`：先本地解析，再退回分享页抓取。
pub fn parse(soda: &Soda, link: &str) -> Result<Song> {
    if let Some(track_id) = non_empty(extract_track_id(link)) {
        return fetch_song_detail(soda, &track_id);
    }

    let response = http::get_full(
        link,
        &[
            RequestOption::new().header("User-Agent", USER_AGENT),
            RequestOption::new().cookie(&soda.cookie()),
        ],
    )?;
    if let Some(track_id) = non_empty(extract_track_id(&response.final_url)) {
        return fetch_song_detail(soda, &track_id);
    }
    let body_text = response.body_text();
    if let Some(track_id) = non_empty(extract_track_id(&body_text)) {
        return fetch_song_detail(soda, &track_id);
    }
    Err(SodaError::not_found("soda track id not found"))
}

fn non_empty(value: String) -> Option<String> {
    if value.trim().is_empty() {
        None
    } else {
        Some(value)
    }
}

impl Soda {
    /// 等价 `(*Soda).Search`。
    pub fn search(&self, keyword: &str) -> Result<Vec<Song>> {
        search(self, keyword)
    }

    /// 等价 `(*Soda).Parse`。
    pub fn parse(&self, link: &str) -> Result<Song> {
        parse(self, link)
    }
}
