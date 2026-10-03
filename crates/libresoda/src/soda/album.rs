//! 专辑：搜索 / 分享页解析（对应上游 `soda/album.go` + `fetchAlbumDetail`）。

use super::search::{fetch_android_search, parse_album_search};
use super::types::{
    album_link, build_image_url, join_track_artists, max_bitrate_size, track_extra, track_link,
    ShareAlbumPage, SODA_ANDROID_SEARCH_PAGE_SIZE, USER_AGENT,
};
use super::Soda;
use crate::error::{Result, SodaError};
use crate::http::{self, RequestOption};
use crate::model::{Playlist, Song, SOURCE_SODA};
use crate::soda::link::extract_album_id;

/// PC 端专辑详情（`GET /luna/pc/albums/{album_id}`；上游没有，照客户端 IDL 实现）。
pub const PC_ALBUM_PATH: &str = "/luna/pc/albums";

/// PC 端专辑详情原始回包（含曲目、艺人 id 等字段）。
pub fn fetch_pc_album_detail(soda: &Soda, album_id: &str) -> Result<serde_json::Value> {
    let id = album_id.trim();
    if id.is_empty() {
        return Err(SodaError::invalid_input(
            "soda pc album detail requires album_id",
        ));
    }
    super::pc_get_json(soda, &format!("{PC_ALBUM_PATH}/{id}"), &[])
}

/// 等价 `SearchAlbum`。
pub fn search_album(soda: &Soda, keyword: &str) -> Result<Vec<Playlist>> {
    let body = fetch_android_search(soda, "album", keyword, 1, SODA_ANDROID_SEARCH_PAGE_SIZE)?;
    let albums = parse_album_search(&body)?;

    Ok(albums
        .into_iter()
        .filter(|album| !album.id.is_empty())
        .map(|album| {
            let mut extra = std::collections::BTreeMap::new();
            extra.insert("album_id".to_string(), album.id.clone());
            if album.release_date > 0 {
                extra.insert("release_date".to_string(), album.release_date.to_string());
            }
            Playlist {
                source: SOURCE_SODA.to_string(),
                id: album.id.clone(),
                name: album.name.clone(),
                cover: build_image_url(&album.url_cover, "~c5_300x300.jpg"),
                track_count: album.count_tracks,
                creator: join_track_artists(&album.artists),
                description: album.company.trim().to_string(),
                link: album_link(&album.id),
                extra,
                ..Default::default()
            }
        })
        .collect())
}

/// 等价 `fetchAlbumDetail`：读取专辑分享页里的 `_ROUTER_DATA`。
pub fn fetch_album_detail(soda: &Soda, id: &str) -> Result<(Playlist, Vec<Song>)> {
    let body = http::get(
        &album_link(id),
        &[
            RequestOption::new().header("User-Agent", USER_AGENT),
            RequestOption::new().cookie(&soda.cookie()),
        ],
    )?;

    let page = parse_share_album_page(&body)?;
    let info = page.loader_data.album_page.album_info.clone();
    if info.id.is_empty() {
        return Err(SodaError::not_found("album not found"));
    }

    let mut description = info.pc_lines.join(" ").trim().to_string();
    if description.is_empty() {
        description = info.company.trim().to_string();
    }

    let mut extra = std::collections::BTreeMap::new();
    extra.insert("album_id".to_string(), info.id.clone());
    if info.release_date > 0 {
        extra.insert("release_date".to_string(), info.release_date.to_string());
    }

    let mut album = Playlist {
        source: SOURCE_SODA.to_string(),
        id: info.id.clone(),
        name: info.name.clone(),
        cover: build_image_url(&info.url_cover, "~c5_300x300.jpg"),
        track_count: info.count_tracks,
        creator: join_track_artists(&info.artists),
        description,
        link: album_link(&info.id),
        extra,
        ..Default::default()
    };
    if album.track_count == 0 {
        album.track_count = page.loader_data.album_page.track_list.len() as i64;
    }

    let mut songs: Vec<Song> = Vec::new();
    for track in &page.loader_data.album_page.track_list {
        if track.id.is_empty() {
            continue;
        }

        let mut display_size = max_bitrate_size(&track.bit_rates);
        let preview_size = max_bitrate_size(&track.preview.bit_rates);
        if preview_size > display_size {
            display_size = preview_size;
        }

        let artist_id = track
            .artists
            .first()
            .map(|a| a.id.trim())
            .unwrap_or_default()
            .to_string();
        let mut artist = join_track_artists(&track.artists);
        if artist.is_empty() {
            artist = album.creator.clone();
        }

        let mut cover = build_image_url(&track.album.url_cover, "~c5_375x375.jpg");
        if cover.is_empty() {
            cover = build_image_url(&info.url_cover, "~c5_375x375.jpg");
        }

        let album_id = if track.album.id.is_empty() {
            info.id.clone()
        } else {
            track.album.id.clone()
        };
        let mut album_name = track.album.name.trim().to_string();
        if album_name.is_empty() {
            album_name = info.name.clone();
        }

        let duration = track.duration / 1000;
        let bitrate = if duration > 0 && display_size > 0 {
            display_size * 8 / 1000 / duration
        } else {
            0
        };

        songs.push(Song {
            source: SOURCE_SODA.to_string(),
            id: track.id.clone(),
            name: track.name.clone(),
            artist,
            album: album_name,
            album_id: album_id.clone(),
            duration,
            size: display_size,
            bitrate,
            cover,
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
        });
    }

    if songs.is_empty() {
        return Err(SodaError::not_found("album has no songs"));
    }
    Ok((album, songs))
}

/// 等价 `ParseAlbum`。
pub fn parse_album(soda: &Soda, link: &str) -> Result<(Playlist, Vec<Song>)> {
    let album_id = extract_album_id(link);
    if album_id.is_empty() {
        return Err(SodaError::invalid_input("invalid soda album link"));
    }
    fetch_album_detail(soda, &album_id)
}

impl Soda {
    /// PC 端专辑详情（原始回包）。
    pub fn fetch_pc_album_detail(&self, album_id: &str) -> Result<serde_json::Value> {
        fetch_pc_album_detail(self, album_id)
    }

    /// 等价 `(*Soda).SearchAlbum`。
    pub fn search_album(&self, keyword: &str) -> Result<Vec<Playlist>> {
        search_album(self, keyword)
    }

    /// 等价 `(*Soda).GetAlbumSongs`。
    pub fn get_album_songs(&self, id: &str) -> Result<Vec<Song>> {
        Ok(fetch_album_detail(self, id)?.1)
    }

    /// 等价 `(*Soda).ParseAlbum`。
    pub fn parse_album(&self, link: &str) -> Result<(Playlist, Vec<Song>)> {
        parse_album(self, link)
    }
}

/// 等价 `parseSodaShareAlbumPage`：先取出 `_ROUTER_DATA = {...}` 再解析。
pub fn parse_share_album_page(body: &[u8]) -> Result<ShareAlbumPage> {
    let page = String::from_utf8_lossy(body);
    let json = extract_json_block(&page, "_ROUTER_DATA = ")?;
    serde_json::from_str(&json)
        .map_err(|err| SodaError::json(format!("soda album page json error: {err}")))
}

/// 等价 `extractSodaJSONBlock`：按括号配平取出一段 JSON（忽略字符串内的括号）。
pub fn extract_json_block(page: &str, marker: &str) -> Result<String> {
    let Some(start_index) = page.find(marker) else {
        return Err(SodaError::not_found("soda router data not found"));
    };
    let bytes = page.as_bytes();
    let start = start_index + marker.len();

    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    let mut started = false;

    for index in start..bytes.len() {
        let ch = bytes[index];
        if in_string {
            if escaped {
                escaped = false;
                continue;
            }
            match ch {
                b'\\' => escaped = true,
                b'"' => in_string = false,
                _ => {}
            }
            continue;
        }

        match ch {
            b'"' => in_string = true,
            b'{' => {
                depth += 1;
                started = true;
            }
            b'}' => {
                depth -= 1;
                if started && depth == 0 {
                    return Ok(page[start..=index].to_string());
                }
            }
            _ => {}
        }
    }

    Err(SodaError::not_found("soda router data is incomplete"))
}
