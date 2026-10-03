//! Android 搜索接口（对应上游 `soda/search.go`）。

use super::types::{
    SODA_ANDROID_API_BASE, SODA_ANDROID_SEARCH_PAGE_SIZE, SODA_ANDROID_SEARCH_USER_AGENT,
};
use super::Soda;
use crate::error::{Result, SodaError};
use crate::http::{self, RequestOption};
use crate::util::Params;

/// 等价 `sodaAndroidSearchParams`。
pub fn android_search_params() -> Params {
    let mut params = Params::new();
    let values: [(&str, &str); 48] = [
        ("device_platform", "android"),
        ("os", "android"),
        ("ssmix", "a"),
        ("cdid", "46556f98-1720-4248-83da-62b74b60b46a"),
        ("channel", "xiaomi_8478_64"),
        ("aid", "8478"),
        ("app_name", "luna"),
        ("version_code", "100198030"),
        ("version_name", "19.8.0"),
        ("manifest_version_code", "100198030"),
        ("update_version_code", "100198030"),
        ("resolution", "1080*1920"),
        ("dpi", "480"),
        ("device_type", "ABR-AL80"),
        ("device_brand", "HUAWEI"),
        ("language", "zh"),
        ("os_api", "35"),
        ("os_version", "15"),
        ("ac", "wifi"),
        ("device_model", "ABR-AL80"),
        ("save_power", "0"),
        ("font_size", "1.00"),
        ("luna_first_launch_apk_type", "normal_apk"),
        ("diversion_channel_name", "xiaomi_8478_64"),
        ("is_car_play", "0"),
        ("battery", "0.99"),
        ("network_speed", "10156"),
        ("hybrid_version_code", "100198030"),
        ("tz_name", "Asia/Shanghai"),
        ("tz_offset", "28800"),
        ("luna_register_time", "1784311292"),
        (
            "diversion_category_level_two",
            "Xiaomi%E5%95%86%E5%BA%97-%E8%87%AA%E7%84%B6",
        ),
        ("package", "com.luna.music"),
        ("charge", "0"),
        ("luna_apk_type", "normal_apk"),
        ("output_device_type", "Phone"),
        ("volume", "1.00"),
        ("brightness", "0.08"),
        ("need_personal_recommend", "1"),
        ("is_teen_mode", "0"),
        ("sim_region", "cn"),
        (
            "diversion_category_level_one",
            "%E5%8E%82%E5%95%86%E5%95%86%E5%BA%97-%E8%87%AA%E7%84%B6",
        ),
        ("android_device_type", "default"),
        ("iid", "2204957404569386"),
        ("device_id", "2204957404565290"),
        ("_rticket", ""),
        ("aid", "8478"),
        ("os_version", "15"),
    ];
    for (key, value) in values {
        params.set(key, value);
    }
    params.set("_rticket", crate::util::now_millis().to_string());
    params
}

/// 等价 `sodaAndroidSearchURL`。
pub fn android_search_url(search_type: &str, keyword: &str, page: i64, page_size: i64) -> String {
    let page = if page < 1 { 1 } else { page };
    let page_size = if page_size <= 0 {
        SODA_ANDROID_SEARCH_PAGE_SIZE
    } else {
        page_size
    };

    let mut params = android_search_params();
    params.set("q", keyword);
    params.set("cursor", ((page - 1) * page_size).to_string());
    params.set("count", page_size.to_string());
    params.set("aid", "386088");

    format!(
        "{SODA_ANDROID_API_BASE}/search/{search_type}?{}",
        params.encode()
    )
}

/// 读取官方综合搜索原始回包（`/search/all`）。
///
/// 综合搜索会按 `top_results` / `playlists` / `artists` / `tracks` / `albums`
/// 返回多个 result group，客户端可直接按官方搜索页的分组渲染。
pub fn fetch_search_all_body(
    soda: &Soda,
    keyword: &str,
    page: i64,
    page_size: i64,
) -> Result<Vec<u8>> {
    fetch_android_search(soda, "all", keyword, page, page_size)
}

/// 等价 `androidSearchOptions`。
pub(crate) fn android_search_options(soda: &Soda) -> Vec<RequestOption> {
    vec![RequestOption::new()
        .header("User-Agent", SODA_ANDROID_SEARCH_USER_AGENT)
        .header("content-type", "application/json; charset=UTF-8")
        .cookie(&soda.cookie())]
}

/// 等价 `fetchAndroidSearch`。
pub(crate) fn fetch_android_search(
    soda: &Soda,
    search_type: &str,
    keyword: &str,
    page: i64,
    page_size: i64,
) -> Result<Vec<u8>> {
    let url = android_search_url(search_type, keyword, page, page_size);
    http::get(&url, &android_search_options(soda))
}

#[derive(Debug, serde::Deserialize)]
struct SearchResponse<T> {
    #[serde(default)]
    result_groups: Vec<SearchGroup<T>>,
}

#[derive(Debug, serde::Deserialize)]
struct SearchGroup<T> {
    #[serde(default)]
    data: Vec<SearchItem<T>>,
}

#[derive(Debug, serde::Deserialize)]
struct SearchItem<T> {
    entity: T,
}

#[derive(Debug, Default, serde::Deserialize)]
struct TrackEntity {
    #[serde(default)]
    track: super::types::Track,
}

#[derive(Debug, Default, serde::Deserialize)]
struct ArtistEntity {
    #[serde(default)]
    artist: super::types::Artist,
}

#[derive(Debug, Default, serde::Deserialize)]
struct AlbumEntity {
    #[serde(default)]
    album: super::types::Album,
}

pub(crate) fn parse_track_search(body: &[u8]) -> Result<Vec<super::types::Track>> {
    let response: SearchResponse<TrackEntity> = serde_json::from_slice(body)
        .map_err(|err| SodaError::json(format!("soda search json parse error: {err}")))?;
    let mut tracks = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for group in response.result_groups {
        for item in group.data {
            let track = item.entity.track;
            if track.id.is_empty() || seen.contains(&track.id) {
                continue;
            }
            seen.push(track.id.clone());
            tracks.push(track);
        }
    }
    Ok(tracks)
}

pub(crate) fn parse_artist_search(body: &[u8]) -> Result<Vec<super::types::Artist>> {
    let response: SearchResponse<ArtistEntity> = serde_json::from_slice(body)
        .map_err(|err| SodaError::json(format!("soda artist search json parse error: {err}")))?;
    let mut artists = Vec::new();
    for group in response.result_groups {
        for item in group.data {
            let artist = item.entity.artist;
            if artist.id.is_empty() {
                continue;
            }
            artists.push(artist);
        }
    }
    Ok(artists)
}

pub(crate) fn parse_album_search(body: &[u8]) -> Result<Vec<super::types::Album>> {
    let response: SearchResponse<AlbumEntity> = serde_json::from_slice(body)
        .map_err(|err| SodaError::json(format!("soda album search json parse error: {err}")))?;
    let mut albums = Vec::new();
    for group in response.result_groups {
        for item in group.data {
            let album = item.entity.album;
            if album.id.is_empty() {
                continue;
            }
            albums.push(album);
        }
    }
    Ok(albums)
}

#[derive(Debug, Default, serde::Deserialize)]
struct PlaylistEntity {
    #[serde(default)]
    playlist: super::types::UserPlaylistItem,
}

pub(crate) fn parse_playlist_search(body: &[u8]) -> Result<Vec<super::types::UserPlaylistItem>> {
    let response: SearchResponse<PlaylistEntity> = serde_json::from_slice(body)
        .map_err(|err| SodaError::json(format!("soda playlist json parse error: {err}")))?;
    let mut playlists = Vec::new();
    for group in response.result_groups {
        for item in group.data {
            let playlist = item.entity.playlist;
            if playlist.id.is_empty() {
                continue;
            }
            playlists.push(playlist);
        }
    }
    Ok(playlists)
}

/// 等价 `SearchArtist`。
pub fn search_artist(soda: &Soda, keyword: &str) -> Result<Vec<super::types::Artist>> {
    let body = fetch_android_search(soda, "artist", keyword, 1, SODA_ANDROID_SEARCH_PAGE_SIZE)?;
    parse_artist_search(&body)
}

impl Soda {
    /// 官方综合搜索原始回包；分页游标在各个 group 内。
    pub fn fetch_search_all_body(
        &self,
        keyword: &str,
        page: i64,
        page_size: i64,
    ) -> Result<Vec<u8>> {
        fetch_search_all_body(self, keyword, page, page_size)
    }

    /// 等价 `(*Soda).SearchArtist`。
    pub fn search_artist(&self, keyword: &str) -> Result<Vec<super::types::Artist>> {
        search_artist(self, keyword)
    }
}

// ---------------------------------------------------------------------------
// 搜索联想（上游无对应；照官方客户端 `Sug` / `SuggestWords` 实现）
// ---------------------------------------------------------------------------

/// `GET /luna/pc/sug`（客户端 `components/SearchBox.vue` 用 `sug_scene = "main"`）。
pub const SUG_PATH: &str = "/luna/pc/sug";

/// `GET /luna/suggest-words/{suggest_type}`（热搜/推荐搜索词）。
pub const SUGGEST_WORDS_PATH: &str = "/luna/suggest-words";

/// 联想词查询参数（`sug_search_id` 每次请求一个 v4 UUID，与客户端一致）。
pub fn sug_params(keyword: &str, sug_search_id: &str) -> Vec<(&'static str, String)> {
    let search_id = if sug_search_id.trim().is_empty() {
        crate::util::random_uuid_v4()
    } else {
        sug_search_id.trim().to_string()
    };
    vec![
        ("q", keyword.trim().to_string()),
        ("sug_scene", "main".to_string()),
        ("sug_search_id", search_id),
    ]
}

/// 搜索联想词，返回原始回包（`sugs` 数组）。
pub fn suggest(soda: &Soda, keyword: &str) -> Result<serde_json::Value> {
    if keyword.trim().is_empty() {
        return Err(SodaError::invalid_input("soda suggest requires keyword"));
    }
    let params = sug_params(keyword, "");
    super::pc_get_json(soda, SUG_PATH, &params)
}

/// 热搜/推荐搜索词（`suggest_type` 由服务端定义，例如 `default`/`hot`）。
pub fn suggest_words(soda: &Soda, suggest_type: &str) -> Result<serde_json::Value> {
    let suggest_type = if suggest_type.trim().is_empty() {
        "default"
    } else {
        suggest_type.trim()
    };
    let path = format!("{SUGGEST_WORDS_PATH}/{suggest_type}");
    super::pc_get_json(soda, &path, &[])
}

impl Soda {
    /// 搜索联想词。
    pub fn suggest(&self, keyword: &str) -> Result<serde_json::Value> {
        suggest(self, keyword)
    }

    /// 热搜/推荐搜索词。
    pub fn suggest_words(&self, suggest_type: &str) -> Result<serde_json::Value> {
        suggest_words(self, suggest_type)
    }
}

#[cfg(test)]
mod sug_tests {
    use super::*;

    #[test]
    fn sug_params_carry_scene_and_uuid() {
        let params = sug_params("周杰伦", "");
        assert_eq!(params[0], ("q", "周杰伦".to_string()));
        assert_eq!(params[1], ("sug_scene", "main".to_string()));
        assert_eq!(params[2].1.len(), 36);
        assert_eq!(params[2].1.matches('-').count(), 4);
    }

    #[test]
    fn sug_params_keep_given_search_id() {
        let params = sug_params(" 林俊杰 ", "fixed-id");
        assert_eq!(params[0], ("q", "林俊杰".to_string()));
        assert_eq!(params[2], ("sug_search_id", "fixed-id".to_string()));
    }
}
