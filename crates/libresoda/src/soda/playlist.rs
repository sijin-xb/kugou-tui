//! 歌单：搜索 / 分页详情 / web 兜底（对应上游 `soda/playlist.go` + `fetchPlaylistDetail*`）。

use super::search::{fetch_android_search, parse_playlist_search};
use super::types::{
    build_image_url, playlist_link, PlaylistDetailResponse, UserPlaylistItem,
    SODA_ANDROID_SEARCH_PAGE_SIZE, USER_AGENT,
};
use super::Soda;
use crate::error::{Result, SodaError};
use crate::http::{self, RequestOption};
use crate::model::{Playlist, PlaylistCategory, Song, SOURCE_SODA};
use crate::soda::link::extract_playlist_id;
use crate::soda::track::build_song_from_track;
use crate::util::{first_non_empty, query_escape};

/// 等价 `SearchPlaylist`。
pub fn search_playlist(soda: &Soda, keyword: &str) -> Result<Vec<Playlist>> {
    let body = fetch_android_search(soda, "playlist", keyword, 1, SODA_ANDROID_SEARCH_PAGE_SIZE)?;
    let items = parse_playlist_search(&body)?;

    Ok(items
        .into_iter()
        .filter(|item| !item.id.is_empty())
        .map(|item| Playlist {
            source: SOURCE_SODA.to_string(),
            id: item.id.clone(),
            name: item.title.clone(),
            cover: build_image_url(&item.url_cover, "~c5_300x300.jpg"),
            track_count: item.count_tracks,
            creator: first_non_empty(&[&item.owner.public_name, &item.owner.nickname]),
            description: item.desc.clone(),
            link: playlist_link(&item.id),
            ..Default::default()
        })
        .collect())
}

/// 等价 `sodaBuildPlaylistFromUserItem`。
pub fn build_playlist_from_user_item(
    item: &UserPlaylistItem,
    current_user_id: &str,
    current_nickname: &str,
) -> Playlist {
    let playlist_id = item.id.trim().to_string();
    if playlist_id.is_empty() {
        return Playlist::default();
    }

    let name = first_non_empty(&[&item.title, &item.public_title, &playlist_id]);
    let creator = first_non_empty(&[
        &item.owner.public_name,
        &item.owner.nickname,
        current_nickname,
        current_user_id,
    ]);
    let mut track_count = item.count_tracks;
    if track_count == 0 {
        track_count = item.resource_cnt.track_cnt;
    }
    let mut play_count = item.play_count;
    if play_count == 0 {
        play_count = item.stats.count_played;
    }

    let mut extra = std::collections::BTreeMap::new();
    extra.insert("user_id".to_string(), current_user_id.to_string());
    extra.insert("type".to_string(), item.playlist_type.to_string());
    let owner_id = item.owner.id.trim();
    if !owner_id.is_empty() {
        extra.insert("owner_id".to_string(), owner_id.to_string());
    }
    let public_title = item.public_title.trim();
    if !public_title.is_empty() {
        extra.insert("public_title".to_string(), public_title.to_string());
    }
    let review_status = item.review_status.trim();
    if !review_status.is_empty() {
        extra.insert("review_status".to_string(), review_status.to_string());
    }
    if item.stats.count_collected > 0 {
        extra.insert(
            "collect_count".to_string(),
            item.stats.count_collected.to_string(),
        );
    }
    // 隐私标记只有 `is_private = true` 时才写入，避免给所有歌单都塞一个 `false`
    // （与 `review_status` 等可选字段保持一致的「有才写」语义）。
    if item.is_private {
        extra.insert("is_private".to_string(), "true".to_string());
    }

    Playlist {
        source: SOURCE_SODA.to_string(),
        id: playlist_id.clone(),
        name,
        cover: build_image_url(&item.url_cover, "~c5_300x300.jpg"),
        track_count,
        play_count,
        creator,
        description: item.desc.trim().to_string(),
        link: playlist_link(&playlist_id),
        extra,
    }
}

/// 等价 `fetchPlaylistDetail`（分页，失败时回退 web 接口）。
pub fn fetch_playlist_detail(soda: &Soda, id: &str) -> Result<(Playlist, Vec<Song>)> {
    fetch_playlist_detail_paged(soda, id)
}

/// 等价 `fetchPlaylistDetailPaged`。
pub fn fetch_playlist_detail_paged(soda: &Soda, id: &str) -> Result<(Playlist, Vec<Song>)> {
    let playlist_id = id.trim();
    if playlist_id.is_empty() {
        return Err(SodaError::invalid_input("playlist id is empty"));
    }

    const PAGE_SIZE: i64 = 100;
    let mut cursor = String::new();
    let mut seen_cursors: Vec<String> = Vec::new();
    let mut seen_tracks: Vec<String> = Vec::new();
    let mut playlist: Option<Playlist> = None;
    let mut songs: Vec<Song> = Vec::new();

    for page in 0..20 {
        let response = match fetch_playlist_detail_page(soda, playlist_id, &cursor, PAGE_SIZE) {
            Ok(response) => response,
            Err(err) => {
                if page == 0 {
                    return fetch_playlist_detail_web(soda, playlist_id);
                }
                return Err(err);
            }
        };

        if playlist.is_none() {
            let mut built = build_playlist_from_user_item(&response.playlist, "", "");
            if built.id.is_empty() {
                built.id = playlist_id.to_string();
                built.source = SOURCE_SODA.to_string();
                built.link = playlist_link(playlist_id);
            }
            playlist = Some(built);
        }

        for item in &response.media_resources {
            if item.resource_type != "track" {
                continue;
            }
            let track = &item.entity.track_wrapper.track;
            if track.id.is_empty() || seen_tracks.contains(&track.id) {
                continue;
            }
            seen_tracks.push(track.id.clone());
            let song = build_song_from_track(track);
            // 注意：**不要**在单曲缺封面时退回「歌单封面」——歌单封面通常来自第一首，
            // 会让一批歌显示成同一张图（实测 6 首歌共用第一首的封面）。
            // 缺封面就留空，由客户端显示中性占位图。
            songs.push(song);
        }

        let next_cursor = response.next_cursor.trim().to_string();
        if next_cursor.is_empty() || next_cursor == cursor || seen_cursors.contains(&next_cursor) {
            break;
        }
        if !response.has_more && (response.media_resources.len() as i64) < PAGE_SIZE {
            break;
        }
        seen_cursors.push(next_cursor.clone());
        cursor = next_cursor;
    }

    let Some(mut playlist) = playlist else {
        return Err(SodaError::not_found("playlist not found"));
    };
    if playlist.id.is_empty() {
        return Err(SodaError::not_found("playlist not found"));
    }
    if playlist.track_count == 0 {
        playlist.track_count = songs.len() as i64;
    }
    Ok((playlist, songs))
}

/// 等价 `fetchPlaylistDetailPage`。
pub fn fetch_playlist_detail_page(
    soda: &Soda,
    playlist_id: &str,
    cursor: &str,
    count: i64,
) -> Result<PlaylistDetailResponse> {
    let url = super::pc_playlist_detail_url(playlist_id, cursor, count);
    let body = http::get(&url, &super::pc_request_options(soda))?;
    let response: PlaylistDetailResponse = serde_json::from_slice(&body)
        .map_err(|err| SodaError::json(format!("soda playlist detail json error: {err}")))?;
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

#[derive(Debug, serde::Deserialize)]
struct WebPlaylistResponse {
    #[serde(default)]
    playlist: WebPlaylist,
    #[serde(default)]
    media_resources: Vec<WebMediaResource>,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
struct WebPlaylist {
    id: String,
    title: String,
    desc: String,
    owner: WebOwner,
    count_tracks: i64,
    url_cover: WebImage,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
struct WebOwner {
    nickname: String,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
struct WebImage {
    urls: Vec<String>,
    uri: String,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
struct WebMediaResource {
    #[serde(rename = "type")]
    resource_type: String,
    entity: WebEntity,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
struct WebEntity {
    track_wrapper: WebTrackWrapper,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
struct WebTrackWrapper {
    track: WebTrack,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
struct WebTrack {
    id: String,
    name: String,
    duration: i64,
    artists: Vec<WebArtist>,
    album: WebAlbum,
    bit_rates: Vec<WebBitRate>,
    audio_info: WebAudioInfo,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
struct WebArtist {
    name: String,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
struct WebAlbum {
    name: String,
    url_cover: WebImage,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
struct WebBitRate {
    size: i64,
    quality: String,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
struct WebAudioInfo {
    play_info_list: Vec<WebPlayInfo>,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
struct WebPlayInfo {
    #[serde(rename = "main_play_url")]
    main_play_url: String,
    #[serde(rename = "play_auth")]
    play_auth: String,
    #[serde(rename = "size")]
    size: i64,
    #[serde(rename = "format")]
    format: String,
    #[serde(rename = "bitrate")]
    bitrate: i64,
    #[serde(rename = "quality")]
    quality: String,
}

/// 等价 `fetchPlaylistDetailWeb`（PC track_v2 下线后，这里是分页接口失败时的兜底）。
pub fn fetch_playlist_detail_web(soda: &Soda, id: &str) -> Result<(Playlist, Vec<Song>)> {
    let mut params = crate::util::Params::new();
    params.set("playlist_id", id);
    params.set("cursor", "0");
    params.set("cnt", "20");
    params.set("aid", "386088");
    params.set("device_platform", "web");
    params.set("channel", "pc_web");
    let url = format!(
        "https://api.qishui.com/luna/pc/playlist/detail?{}",
        params.encode()
    );

    let body = http::get(
        &url,
        &[
            RequestOption::new().header("User-Agent", USER_AGENT),
            RequestOption::new().cookie(&soda.cookie()),
        ],
    )?;
    let response: WebPlaylistResponse = serde_json::from_slice(&body)
        .map_err(|err| SodaError::json(format!("soda playlist detail json error: {err}")))?;

    let mut playlist = Playlist {
        source: SOURCE_SODA.to_string(),
        id: id.to_string(),
        name: response.playlist.title.clone(),
        creator: response.playlist.owner.nickname.clone(),
        description: response.playlist.desc.clone(),
        track_count: response.playlist.count_tracks,
        link: playlist_link(id),
        ..Default::default()
    };

    if let Some(first) = response.playlist.url_cover.urls.first() {
        let mut cover = first.clone();
        let uri = response.playlist.url_cover.uri.clone();
        if !uri.is_empty() && !cover.contains(&uri) {
            cover.push_str(&uri);
        }
        if !cover.contains('~') {
            cover.push_str("~c5_300x300.jpg");
        }
        playlist.cover = cover;
    }

    let mut songs: Vec<Song> = Vec::new();
    for item in &response.media_resources {
        if item.resource_type != "track" {
            continue;
        }
        let track = &item.entity.track_wrapper.track;
        if track.id.is_empty() {
            continue;
        }

        let mut display_size = track.bit_rates.iter().map(|br| br.size).max().unwrap_or(0);
        for play_info in &track.audio_info.play_info_list {
            if play_info.size > display_size {
                display_size = play_info.size;
            }
        }

        let artist_names: Vec<String> = track
            .artists
            .iter()
            .map(|artist| artist.name.clone())
            .collect();
        let mut cover = String::new();
        if let Some(domain) = track.album.url_cover.urls.first() {
            let uri = track.album.url_cover.uri.clone();
            if !domain.is_empty() && !uri.is_empty() && !domain.contains(&uri) {
                cover = format!("{domain}{uri}~c5_375x375.jpg");
            } else if !domain.is_empty() {
                cover = format!("{domain}~c5_375x375.jpg");
            }
        }

        let seconds = track.duration / 1000;
        let bitrate = if seconds > 0 && display_size > 0 {
            display_size * 8 / 1000 / seconds
        } else {
            0
        };

        let mut song = Song {
            source: SOURCE_SODA.to_string(),
            id: track.id.clone(),
            name: track.name.clone(),
            artist: artist_names.join("、"),
            album: track.album.name.clone(),
            duration: track.duration / 1000,
            size: display_size,
            bitrate,
            cover,
            link: crate::soda::types::track_link(&track.id),
            extra: crate::util::extra_from_pairs([("track_id", track.id.clone())]),
            ..Default::default()
        };

        if !track.audio_info.play_info_list.is_empty() {
            let mut best = &track.audio_info.play_info_list[0];
            for info in &track.audio_info.play_info_list {
                if info.size > best.size {
                    best = info;
                }
            }
            if !best.main_play_url.is_empty() {
                song.url = format!(
                    "{}#auth={}",
                    best.main_play_url,
                    query_escape(&best.play_auth)
                );
                if song.size == 0 {
                    song.size = best.size;
                }
                song.ext = best.format.clone();
                song.bitrate = crate::soda::quality::normalize_bitrate(best.bitrate);
            }
            if !best.quality.trim().is_empty() {
                song.extra_set("quality", best.quality.trim().to_string());
            }
        }

        songs.push(song);
    }

    Ok((playlist, songs))
}

/// 等价 `GetPlaylistSongs`。
pub fn get_playlist_songs(soda: &Soda, id: &str) -> Result<Vec<Song>> {
    Ok(fetch_playlist_detail(soda, id)?.1)
}

/// 等价 `ParsePlaylist`。
pub fn parse_playlist(soda: &Soda, link: &str) -> Result<(Playlist, Vec<Song>)> {
    let playlist_id = extract_playlist_id(link);
    if !playlist_id.is_empty() {
        return fetch_playlist_detail(soda, &playlist_id);
    }

    let response = http::get_full(
        link,
        &[
            RequestOption::new().header("User-Agent", USER_AGENT),
            RequestOption::new().cookie(&soda.cookie()),
        ],
    )?;
    if let Some(id) = non_empty(extract_playlist_id(&response.final_url)) {
        return fetch_playlist_detail(soda, &id);
    }
    let body_text = response.body_text();
    if let Some(id) = non_empty(extract_playlist_id(&body_text)) {
        return fetch_playlist_detail(soda, &id);
    }
    Err(SodaError::not_found("soda playlist id not found"))
}

/// 等价 `GetRecommendedPlaylists`（上游同样未实现）。
pub fn get_recommended_playlists(_soda: &Soda) -> Result<Vec<Playlist>> {
    Err(SodaError::unsupported(
        "soda daily recommendation not supported",
    ))
}

/// 「为你推荐歌单」的 PC 路径（客户端 `GetRecommendPlaylist`）。
pub const RECOMMEND_PLAYLIST_PATH: &str = "/luna/me/playlist/recommend";

/// 推荐歌单（客户端 `GetRecommendPlaylist`，`GET /luna/me/playlist/recommend`）。
///
/// `GET /luna/me/playlist/recommend` 没有查询参数，回包是 `{status_code, playlists:[…]}`。
pub fn get_recommend_playlists(soda: &Soda) -> Result<Vec<Playlist>> {
    let value = super::pc_get_json(soda, RECOMMEND_PLAYLIST_PATH, &[])?;
    Ok(parse_recommend_playlists(&value))
}

/// 解析推荐歌单回包（缺字段一律当空列表，不报错）。
pub fn parse_recommend_playlists(value: &serde_json::Value) -> Vec<Playlist> {
    let Some(list) = value.get("playlists").and_then(|list| list.as_array()) else {
        return Vec::new();
    };
    let mut out: Vec<Playlist> = Vec::new();
    for item in list {
        let Ok(parsed) = serde_json::from_value::<UserPlaylistItem>(item.clone()) else {
            continue;
        };
        let playlist = build_playlist_from_user_item(&parsed, "", "");
        if playlist.id.is_empty() {
            continue;
        }
        out.push(playlist);
    }
    out
}

/// 等价 `GetPlaylistCategories`。
pub fn get_playlist_categories(_soda: &Soda) -> Result<Vec<PlaylistCategory>> {
    Err(SodaError::unsupported("playlist categories not supported"))
}

/// 等价 `GetCategoryPlaylists`。
pub fn get_category_playlists(
    _soda: &Soda,
    _category_id: &str,
    _page: i64,
    _limit: i64,
) -> Result<Vec<Playlist>> {
    Err(SodaError::unsupported("playlist categories not supported"))
}

fn non_empty(value: String) -> Option<String> {
    if value.trim().is_empty() {
        None
    } else {
        Some(value)
    }
}

impl Soda {
    /// 推荐歌单（客户端 `GetRecommendPlaylist`）。
    pub fn get_recommend_playlists(&self) -> Result<Vec<Playlist>> {
        get_recommend_playlists(self)
    }

    /// 等价 `(*Soda).SearchPlaylist`。
    pub fn search_playlist(&self, keyword: &str) -> Result<Vec<Playlist>> {
        search_playlist(self, keyword)
    }

    /// 等价 `(*Soda).GetPlaylistSongs`。
    pub fn get_playlist_songs(&self, id: &str) -> Result<Vec<Song>> {
        get_playlist_songs(self, id)
    }

    /// 等价 `(*Soda).ParsePlaylist`。
    pub fn parse_playlist(&self, link: &str) -> Result<(Playlist, Vec<Song>)> {
        parse_playlist(self, link)
    }

    /// 等价 `(*Soda).GetRecommendedPlaylists`。
    pub fn get_recommended_playlists(&self) -> Result<Vec<Playlist>> {
        get_recommended_playlists(self)
    }

    /// 等价 `(*Soda).GetPlaylistCategories`。
    pub fn get_playlist_categories(&self) -> Result<Vec<PlaylistCategory>> {
        get_playlist_categories(self)
    }

    /// 等价 `(*Soda).GetCategoryPlaylists`。
    pub fn get_category_playlists(
        &self,
        category_id: &str,
        page: i64,
        limit: i64,
    ) -> Result<Vec<Playlist>> {
        get_category_playlists(self, category_id, page, limit)
    }
}
