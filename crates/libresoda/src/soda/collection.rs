//! 收藏（「我喜欢的音乐」、收藏歌单/专辑/艺人）。
//!
//! 上游 `music-lib` 没有这些写接口；本模块照官方客户端
//!（`src/services/collect/collect.ts` + `src/idl/goapi/main.ts`）实现：
//!
//! | 能力 | 收藏 | 取消收藏 |
//! | --- | --- | --- |
//! | 单曲（我喜欢） | `POST /luna/pc/me/collection/media` | `POST /luna/pc/me/collection/media/delete` |
//! | 歌单 | `POST /luna/pc/me/collection/playlist` | `.../playlist/delete` |
//! | 专辑 | `POST /luna/pc/me/collection/album` | `.../album/delete` |
//! | 艺人 | `POST /luna/pc/me/collection/artist` | `.../artist/delete` |
//!
//! 客户端的请求体（实测自 `collect.ts`）：
//!
//! ```json
//! { "scene": "", "media": [{ "type": "track", "id": "7501674235158431760" }] }
//! ```
//!
//! ⚠️ 这些路径在客户端 `src/libs/bdticket/config.ts` 的「零信任加签」名单里
//!（`session_guard` 由 `bdticket.node` 生成 `bd-ticket-guard-*` 头）。当前只带
//! 应用级签名（`x-helios` / `x-medusa`）；若服务端开始强制校验 ticket guard，
//! 需要再补一个 bdticket 桥接提供者（接口形状不用改）。

use super::media_ref::{media_array, MediaRef};
use super::Soda;
use crate::error::{Result, SodaError};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const COLLECT_MEDIA_PATH: &str = "/luna/pc/me/collection/media";
pub const UNCOLLECT_MEDIA_PATH: &str = "/luna/pc/me/collection/media/delete";
pub const COLLECT_PLAYLIST_PATH: &str = "/luna/pc/me/collection/playlist";
pub const UNCOLLECT_PLAYLIST_PATH: &str = "/luna/pc/me/collection/playlist/delete";
pub const COLLECT_ALBUM_PATH: &str = "/luna/pc/me/collection/album";
pub const UNCOLLECT_ALBUM_PATH: &str = "/luna/pc/me/collection/album/delete";
pub const COLLECT_ARTIST_PATH: &str = "/luna/pc/me/collection/artist";
pub const UNCOLLECT_ARTIST_PATH: &str = "/luna/pc/me/collection/artist/delete";
/// 「我收藏的」混合列表（单曲/歌单/专辑/艺人）。
///
/// 客户端 IDL 同时登记了 PC / 非 PC 两条路径；实测（2026-09-15）
/// `/luna/pc/me/collection/mixed` 只回 `status_info` 空信封，故这里指向非 PC 路径，
/// 由 [`collected_mixed`] 的调用方按需调整。
pub const COLLECTED_MIXED_PATH: &str = "/luna/me/collection/mixed";
/// 我收藏的艺人（客户端 `GetArtistCollection`）。
pub const ARTIST_COLLECTION_PATH: &str = "/luna/me/collection/artist";
/// 指定用户的收藏混合列表（客户端 `GetUserMixedCollections`）。
pub const USER_MIXED_COLLECTION_PATH: &str = "/luna/pc/user/collection/mixed";
/// 已购/收藏的数字专辑（客户端 `GetDigitalAlbums`）。
pub const DIGITAL_ALBUMS_PATH: &str = "/luna/pc/me/assets/albums";

fn require_login(soda: &Soda, what: &str) -> Result<()> {
    if !soda.has_cookie() {
        return Err(SodaError::invalid_input(format!("{what} requires cookie")));
    }
    Ok(())
}

/// 收藏单曲的请求体（`scene` 客户端固定传空串）。
pub fn collect_media_body(media: &[MediaRef]) -> serde_json::Value {
    serde_json::json!({
        "scene": "",
        "media": media_array(media),
    })
}

/// 取消收藏单曲的请求体（没有 `scene` 字段）。
pub fn uncollect_media_body(media: &[MediaRef]) -> serde_json::Value {
    serde_json::json!({ "media": media_array(media) })
}

fn ensure_media(media: &[MediaRef], what: &str) -> Result<()> {
    if media.iter().filter(|item| !item.is_empty()).count() == 0 {
        return Err(SodaError::invalid_input(format!("{what} requires media")));
    }
    Ok(())
}

fn ensure_ids(ids: &[String], what: &str) -> Result<Vec<String>> {
    let cleaned: Vec<String> = ids
        .iter()
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty())
        .collect();
    if cleaned.is_empty() {
        return Err(SodaError::invalid_input(format!("{what} requires ids")));
    }
    Ok(cleaned)
}

/// 喜欢一首或多首单曲（加入「我喜欢的音乐」）。
pub fn collect_media(soda: &Soda, media: &[MediaRef]) -> Result<serde_json::Value> {
    require_login(soda, "soda collect media")?;
    ensure_media(media, "soda collect media")?;
    super::pc_post_json(soda, COLLECT_MEDIA_PATH, &collect_media_body(media))
}

/// 取消喜欢。
pub fn uncollect_media(soda: &Soda, media: &[MediaRef]) -> Result<serde_json::Value> {
    require_login(soda, "soda uncollect media")?;
    ensure_media(media, "soda uncollect media")?;
    super::pc_post_json(soda, UNCOLLECT_MEDIA_PATH, &uncollect_media_body(media))
}

/// 收藏歌单。
pub fn collect_playlists(soda: &Soda, playlist_ids: &[String]) -> Result<serde_json::Value> {
    require_login(soda, "soda collect playlist")?;
    let ids = ensure_ids(playlist_ids, "soda collect playlist")?;
    super::pc_post_json(
        soda,
        COLLECT_PLAYLIST_PATH,
        &serde_json::json!({ "playlist_ids": ids }),
    )
}

/// 取消收藏歌单。
pub fn uncollect_playlists(soda: &Soda, playlist_ids: &[String]) -> Result<serde_json::Value> {
    require_login(soda, "soda uncollect playlist")?;
    let ids = ensure_ids(playlist_ids, "soda uncollect playlist")?;
    super::pc_post_json(
        soda,
        UNCOLLECT_PLAYLIST_PATH,
        &serde_json::json!({ "playlist_ids": ids }),
    )
}

/// 收藏专辑。
pub fn collect_albums(soda: &Soda, album_ids: &[String]) -> Result<serde_json::Value> {
    require_login(soda, "soda collect album")?;
    let ids = ensure_ids(album_ids, "soda collect album")?;
    super::pc_post_json(
        soda,
        COLLECT_ALBUM_PATH,
        &serde_json::json!({ "album_ids": ids }),
    )
}

/// 取消收藏专辑。
pub fn uncollect_albums(soda: &Soda, album_ids: &[String]) -> Result<serde_json::Value> {
    require_login(soda, "soda uncollect album")?;
    let ids = ensure_ids(album_ids, "soda uncollect album")?;
    super::pc_post_json(
        soda,
        UNCOLLECT_ALBUM_PATH,
        &serde_json::json!({ "album_ids": ids }),
    )
}

/// 收藏艺人。
pub fn collect_artists(soda: &Soda, artist_ids: &[String]) -> Result<serde_json::Value> {
    require_login(soda, "soda collect artist")?;
    let ids = ensure_ids(artist_ids, "soda collect artist")?;
    super::pc_post_json(
        soda,
        COLLECT_ARTIST_PATH,
        &serde_json::json!({ "artist_ids": ids }),
    )
}

/// 取消收藏艺人。
pub fn uncollect_artists(soda: &Soda, artist_ids: &[String]) -> Result<serde_json::Value> {
    require_login(soda, "soda uncollect artist")?;
    let ids = ensure_ids(artist_ids, "soda uncollect artist")?;
    super::pc_post_json(
        soda,
        UNCOLLECT_ARTIST_PATH,
        &serde_json::json!({ "artist_ids": ids }),
    )
}

/// 我收藏的内容（混合列表）；`item_types` 可传 `["playable","playlist","album","artist"]` 过滤。
///
/// ⚠️ 服务端在**空列表**时只回 `{"status_info":…}`，不返回 `mixed_collections` 字段；
/// 用 [`parse_mixed_collections`] 解析可避免把"没有收藏"误判成失败。
pub fn collected_mixed(
    soda: &Soda,
    cursor: &str,
    count: i64,
    item_types: &[&str],
) -> Result<serde_json::Value> {
    require_login(soda, "soda collected mixed")?;
    let count = if count <= 0 { 20 } else { count.min(100) };
    let mut params: Vec<(&str, String)> = vec![
        ("cursor", cursor.trim().to_string()),
        ("count", count.to_string()),
    ];
    // 客户端把数组编成重复 key：item_types=a&item_types=b
    for item_type in item_types {
        params.push(("item_types", item_type.trim().to_string()));
    }
    super::pc_get_json(soda, COLLECTED_MIXED_PATH, &params)
}

/// 「我收藏的」列表项（单曲/歌单/专辑/艺人之一）。
///
/// 服务端每个 item 只会填充其中一种类型，未建模的类型保留原始 JSON。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MixedCollectionItem {
    pub playable: Option<serde_json::Value>,
    pub playlist: Option<serde_json::Value>,
    pub album: Option<serde_json::Value>,
    pub artist: Option<serde_json::Value>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl MixedCollectionItem {
    /// 类型名：`playable` / `playlist` / `album` / `artist`（都不匹配时为空串）。
    pub fn kind(&self) -> &'static str {
        if self.playlist.is_some() {
            "playlist"
        } else if self.playable.is_some() {
            "playable"
        } else if self.album.is_some() {
            "album"
        } else if self.artist.is_some() {
            "artist"
        } else {
            ""
        }
    }

    fn payload(&self) -> Option<&serde_json::Value> {
        self.playlist
            .as_ref()
            .or(self.playable.as_ref())
            .or(self.album.as_ref())
            .or(self.artist.as_ref())
    }

    /// 条目 id（服务端字段名统一是 `id`）。
    pub fn id(&self) -> String {
        self.payload()
            .and_then(|value| value.get("id"))
            .and_then(|id| id.as_str())
            .unwrap_or_default()
            .to_string()
    }

    /// 标题（`public_title` 优先，回落 `title` / `name`）。
    pub fn title(&self) -> String {
        let Some(payload) = self.payload() else {
            return String::new();
        };
        for key in ["public_title", "title", "name"] {
            if let Some(text) = payload.get(key).and_then(|v| v.as_str()) {
                if !text.trim().is_empty() {
                    return text.trim().to_string();
                }
            }
        }
        String::new()
    }

    /// 封面地址（服务端给 `url_cover` 前缀 + `uri`）。
    pub fn cover_url(&self) -> String {
        let Some(image) = self
            .payload()
            .and_then(|value| value.get("url_cover"))
            .cloned()
        else {
            return String::new();
        };
        match serde_json::from_value::<crate::soda::types::Image>(image) {
            Ok(image) => crate::soda::types::build_image_url(&image, ""),
            Err(_) => String::new(),
        }
    }
}

/// 解析「我收藏的」回包；**空列表返回空 Vec**（服务端此时不带该字段）。
pub fn parse_mixed_collections(value: &serde_json::Value) -> Vec<MixedCollectionItem> {
    value
        .get("mixed_collections")
        .and_then(|items| items.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| serde_json::from_value(item.clone()).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// 我收藏的内容（类型化）：直接给列表，空收藏返回空 Vec。
pub fn collected_items(
    soda: &Soda,
    cursor: &str,
    count: i64,
    item_types: &[&str],
) -> Result<Vec<MixedCollectionItem>> {
    let value = collected_mixed(soda, cursor, count, item_types)?;
    Ok(parse_mixed_collections(&value))
}

/// 我收藏的艺人（实测可用；`cursor`/`count` 与其它列表接口一致）。
pub fn collected_artists(soda: &Soda, cursor: &str, count: i64) -> Result<serde_json::Value> {
    require_login(soda, "soda collected artists")?;
    let count = if count <= 0 { 20 } else { count.min(100) };
    super::pc_get_json(
        soda,
        ARTIST_COLLECTION_PATH,
        &[
            ("cursor", cursor.trim().to_string()),
            ("count", count.to_string()),
        ],
    )
}

/// 指定用户的收藏混合列表（传自己的 user_id 就是"我收藏的"）。
///
/// 客户端 `GetUserMixedCollections` 走的就是这条 `/luna/pc/user/collection/mixed`；
/// 而 `/luna/pc/me/collection/mixed`（[`collected_mixed`]）在实测中只回空信封。
pub fn user_mixed_collections(
    soda: &Soda,
    user_id: &str,
    cursor: &str,
    count: i64,
    item_types: &[&str],
) -> Result<serde_json::Value> {
    require_login(soda, "soda user mixed collections")?;
    if user_id.trim().is_empty() {
        return Err(SodaError::invalid_input(
            "soda user mixed collections requires user_id",
        ));
    }
    let count = if count <= 0 { 20 } else { count.min(100) };
    let mut params: Vec<(&str, String)> = vec![
        ("user_id", user_id.trim().to_string()),
        ("cursor", cursor.trim().to_string()),
        ("count", count.to_string()),
    ];
    for item_type in item_types {
        params.push(("item_types", item_type.trim().to_string()));
    }
    super::pc_get_json(soda, USER_MIXED_COLLECTION_PATH, &params)
}

/// 已购/收藏的数字专辑列表。
pub fn digital_albums(soda: &Soda, cursor: &str, count: i64) -> Result<serde_json::Value> {
    require_login(soda, "soda digital albums")?;
    let count = if count <= 0 { 20 } else { count.min(100) };
    super::pc_get_json(
        soda,
        DIGITAL_ALBUMS_PATH,
        &[
            ("cursor", cursor.trim().to_string()),
            ("count", count.to_string()),
        ],
    )
}

impl Soda {
    /// 喜欢单曲（加入「我喜欢的音乐」）。
    pub fn collect_media(&self, media: &[MediaRef]) -> Result<serde_json::Value> {
        collect_media(self, media)
    }

    /// 取消喜欢单曲。
    pub fn uncollect_media(&self, media: &[MediaRef]) -> Result<serde_json::Value> {
        uncollect_media(self, media)
    }

    /// 收藏歌单。
    pub fn collect_playlists(&self, playlist_ids: &[String]) -> Result<serde_json::Value> {
        collect_playlists(self, playlist_ids)
    }

    /// 取消收藏歌单。
    pub fn uncollect_playlists(&self, playlist_ids: &[String]) -> Result<serde_json::Value> {
        uncollect_playlists(self, playlist_ids)
    }

    /// 收藏专辑。
    pub fn collect_albums(&self, album_ids: &[String]) -> Result<serde_json::Value> {
        collect_albums(self, album_ids)
    }

    /// 取消收藏专辑。
    pub fn uncollect_albums(&self, album_ids: &[String]) -> Result<serde_json::Value> {
        uncollect_albums(self, album_ids)
    }

    /// 收藏艺人。
    pub fn collect_artists(&self, artist_ids: &[String]) -> Result<serde_json::Value> {
        collect_artists(self, artist_ids)
    }

    /// 取消收藏艺人。
    pub fn uncollect_artists(&self, artist_ids: &[String]) -> Result<serde_json::Value> {
        uncollect_artists(self, artist_ids)
    }

    /// 我收藏的内容（混合列表）。
    pub fn collected_mixed(
        &self,
        cursor: &str,
        count: i64,
        item_types: &[&str],
    ) -> Result<serde_json::Value> {
        collected_mixed(self, cursor, count, item_types)
    }

    /// 我收藏的内容（类型化列表；空收藏返回空 Vec）。
    pub fn collected_items(
        &self,
        cursor: &str,
        count: i64,
        item_types: &[&str],
    ) -> Result<Vec<MixedCollectionItem>> {
        collected_items(self, cursor, count, item_types)
    }

    /// 我收藏的艺人。
    pub fn collected_artists(&self, cursor: &str, count: i64) -> Result<serde_json::Value> {
        collected_artists(self, cursor, count)
    }

    /// 指定用户的收藏混合列表（传自己的 user_id 即"我收藏的"）。
    pub fn user_mixed_collections(
        &self,
        user_id: &str,
        cursor: &str,
        count: i64,
        item_types: &[&str],
    ) -> Result<serde_json::Value> {
        user_mixed_collections(self, user_id, cursor, count, item_types)
    }

    /// 已购/收藏的数字专辑列表。
    pub fn digital_albums(&self, cursor: &str, count: i64) -> Result<serde_json::Value> {
        digital_albums(self, cursor, count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_body_matches_client_shape() {
        let body = collect_media_body(&[MediaRef::track("123")]);
        assert_eq!(body["scene"], "");
        assert_eq!(body["media"][0]["type"], "track");
        assert_eq!(body["media"][0]["id"], "123");
        assert!(body.get("collect_action").is_none());
    }

    #[test]
    fn uncollect_body_has_no_scene() {
        let body = uncollect_media_body(&[MediaRef::track("123")]);
        assert!(body.get("scene").is_none());
        assert_eq!(body["media"][0]["id"], "123");
    }

    #[test]
    fn ensure_ids_rejects_blank() {
        assert!(ensure_ids(&["  ".to_string()], "x").is_err());
        assert_eq!(ensure_ids(&[" a ".to_string()], "x").unwrap(), vec!["a"]);
    }
}

#[cfg(test)]
mod mixed_tests {
    use super::*;

    #[test]
    fn empty_response_yields_empty_list() {
        // 服务端在"没有收藏"时只回 status_info，不返回 mixed_collections
        let value = serde_json::json!({"status_info": {"now": 1789445945}});
        assert!(parse_mixed_collections(&value).is_empty());
    }

    #[test]
    fn playlist_item_is_parsed() {
        // 字段取自 2026-09-15 真机回包
        let value = serde_json::json!({
            "total_num": 1,
            "mixed_collections": [{
                "playlist": {
                    "id": "7306662862905147427",
                    "title": "华语",
                    "public_title": "华语热歌",
                    "url_cover": {
                        "template_prefix": "tplv-b829550vbb",
                        "uri": "ies-music/cover",
                        "urls": ["https://p3-luna.douyinpic.com/img/"]
                    }
                }
            }]
        });
        let items = parse_mixed_collections(&value);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind(), "playlist");
        assert_eq!(items[0].id(), "7306662862905147427");
        assert_eq!(items[0].title(), "华语热歌");
        assert!(items[0]
            .cover_url()
            .starts_with("https://p3-luna.douyinpic.com/img/"));
    }

    #[test]
    fn unknown_item_kind_keeps_raw_payload() {
        let value =
            serde_json::json!({"mixed_collections": [{"album": {"id": "999", "name": "专辑"}}]});
        let items = parse_mixed_collections(&value);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind(), "album");
        assert_eq!(items[0].id(), "999");
        assert_eq!(items[0].title(), "专辑");
    }
}
