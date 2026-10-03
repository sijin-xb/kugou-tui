//! 推荐流与「听歌模式」（场景模式）。
//!
//! 上游 `music-lib` 的 `GetRecommendedPlaylists` / `GetPlaylistCategories` 是空实现，
//! libresoda 之前也只能返回 `unsupported`。本模块照官方客户端的实际调用补齐：
//!
//! | 能力 | 接口 | 客户端调用点 |
//! | --- | --- | --- |
//! | 听歌模式（场景模式） | `POST /luna/pc/feed/mode` | `renderer/compositions/sceneMode.ts`（读 `feed_mode_block[].feed_mode[]`） |
//! | 场景模式内容流 | `POST /luna/pc/discover/mix` | 同上（`discover_playlist` / `discover_radio` / `discover_chart` / `discover_track_top_list`） |
//! | 发现页 | `POST /luna/pc/discover` | 发现页首屏 |
//!
//! 回包结构由服务端按 `feed_mode_block` / `data` 组织，字段随版本变动较大，所以这里
//! 统一返回原始 JSON（`serde_json::Value`），调用方按需取字段——与客户端一致。
//!
//! 场景名（客户端 `DiscoverSceneNameEnum`）：
//! `discovery_playlist`、`discovery_radio`、`discovery_chart`、`discover_track_top_list`。

use super::Soda;
use crate::error::{Result, SodaError};
use crate::soda::types::{build_image_url, Image};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const FEED_MODE_PATH: &str = "/luna/pc/feed/mode";
pub const FEED_SONG_TAB_PATH: &str = "/luna/pc/feed/song-tab";
pub const FEED_RADIO_TRACKS_PATH: &str = "/luna/pc/feed/radio/tracks";
pub const DISCOVER_MIX_PATH: &str = "/luna/pc/discover/mix";
pub const DISCOVER_PATH: &str = "/luna/pc/discover";

/// 官方客户端的场景名常量。
pub const SCENE_DISCOVERY_PLAYLIST: &str = "discovery_playlist";
pub const SCENE_DISCOVERY_RADIO: &str = "discovery_radio";
pub const SCENE_DISCOVERY_CHART: &str = "discovery_chart";
pub const SCENE_DISCOVERY_TRACK_TOP_LIST: &str = "discover_track_top_list";

// ---------------------------------------------------------------------------
// 类型化模型（字段取自 2026-09-15 的真实回包；未知字段用 `extra` 兜住）
// ---------------------------------------------------------------------------

/// `POST /luna/pc/feed/mode` 的回包。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FeedModeResponse {
    pub feed_mode_block: Vec<FeedModeBlock>,
    pub feed_mode_bar: serde_json::Value,
    pub status_code: i64,
    pub status_info: serde_json::Value,
}

/// `feed_mode_block[]`：一组听歌模式（每个 block 是一个场景分组）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FeedModeBlock {
    pub title: String,
    #[serde(rename = "type")]
    pub block_type: String,
    pub feed_mode: Vec<FeedModeEntry>,
}

/// `feed_mode[]`：单个听歌模式选项。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FeedModeEntry {
    #[serde(rename = "type")]
    pub entry_type: String,
    /// 展示文案（如「深夜 EMO」）。
    pub text: String,
    pub cutover_toast: String,
    pub entity: FeedModeEntity,
    pub url_info: Image,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// `entity`：模式对应的场景参数。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FeedModeEntity {
    pub feed_scene_mode: SceneMode,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// `feed_scene_mode`：调用 `discover/mix` 时要回填的参数。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SceneMode {
    pub scene_mode_id: i64,
    /// 如 `scene_mode_emo`，对应列表里的「emo 模式」。
    pub sub_queue_type: String,
}

/// `POST /luna/pc/discover/mix` 的回包。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DiscoverMixResponse {
    pub has_more: bool,
    pub inner_block: Vec<DiscoverBlock>,
    pub status_info: serde_json::Value,
}

/// `inner_block[]`：一个资源块（通常是一张歌单或一组曲目）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DiscoverBlock {
    pub inner_block_id: String,
    pub resources: Vec<DiscoverResource>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// `resources[]`：块里的条目。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DiscoverResource {
    pub entity: DiscoverEntity,
    pub fallback_type: String,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// `entity`：可能是歌单/曲目/专辑/电台，目前把歌单建模出来，其余保留原文。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DiscoverEntity {
    pub playlist: DiscoverPlaylist,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// 发现流里的歌单条目（字段名与 `playlist/detail` 一致）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DiscoverPlaylist {
    pub id: String,
    pub title: String,
    pub public_title: String,
    pub desc: String,
    pub count_tracks: i64,
    pub url_cover: Image,
    #[serde(rename = "type")]
    pub playlist_type: i64,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl DiscoverPlaylist {
    /// 封面完整地址（服务端只回 `urls` 前缀 + `uri`）。
    pub fn cover_url(&self) -> String {
        build_image_url(&self.url_cover, "")
    }

    /// 展示标题（`public_title` 优先，回落 `title`）。
    pub fn display_title(&self) -> &str {
        if self.public_title.trim().is_empty() {
            self.title.as_str()
        } else {
            self.public_title.as_str()
        }
    }
}

impl FeedModeResponse {
    /// 拍平成「可展示的听歌模式列表」：`(block_type, text, scene_mode_id, sub_queue_type, cover_url)`。
    pub fn scenes(&self) -> Vec<SceneEntry> {
        let mut out = Vec::new();
        for block in &self.feed_mode_block {
            for entry in &block.feed_mode {
                out.push(SceneEntry {
                    block_title: block.title.clone(),
                    entry_type: entry.entry_type.clone(),
                    text: entry.text.clone(),
                    scene_mode_id: entry.entity.feed_scene_mode.scene_mode_id,
                    sub_queue_type: entry.entity.feed_scene_mode.sub_queue_type.clone(),
                    cover_url: build_image_url(&entry.url_info, ""),
                });
            }
        }
        out
    }
}

/// 拍平后的听歌模式条目。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SceneEntry {
    pub block_title: String,
    pub entry_type: String,
    pub text: String,
    pub scene_mode_id: i64,
    pub sub_queue_type: String,
    pub cover_url: String,
}

/// 解析 `feed/mode` 回包。
pub fn parse_feed_mode(value: &serde_json::Value) -> Result<FeedModeResponse> {
    serde_json::from_value(value.clone())
        .map_err(|err| SodaError::json(format!("soda feed mode decode error: {err}")))
}

/// 解析 `discover/mix` 回包。
pub fn parse_discover_mix(value: &serde_json::Value) -> Result<DiscoverMixResponse> {
    serde_json::from_value(value.clone())
        .map_err(|err| SodaError::json(format!("soda discover mix decode error: {err}")))
}

fn require_login(soda: &Soda, what: &str) -> Result<()> {
    if !soda.has_cookie() {
        return Err(SodaError::invalid_input(format!("{what} requires cookie")));
    }
    Ok(())
}

/// 官方推荐队列（`FeedQueueItem.fetch`）使用的 `FeedSongTab`。
///
/// 官方请求体至少包含：
/// * `played_media`
/// * `is_first_request`
/// * `is_did_first_request`
/// * `feed_counts.mix_session_count`
///
/// 响应是 `FeedResponse`：`items[]` 中 `type=track` 的
/// `entity.track_wrapper.track` 就是可播放曲目；`has_more` 固定为 `true`。
pub fn fetch_feed_song_tab(soda: &Soda, body: &serde_json::Value) -> Result<serde_json::Value> {
    require_login(soda, "soda feed song tab")?;
    super::pc_post_json(soda, FEED_SONG_TAB_PATH, body)
}

/// 听歌模式（场景模式）：返回 `feed_mode_block` / `feed_mode_bar` 的原始结构。
pub fn fetch_feed_mode(soda: &Soda) -> Result<serde_json::Value> {
    require_login(soda, "soda feed mode")?;
    super::pc_post_json(soda, FEED_MODE_PATH, &serde_json::json!({}))
}

/// 场景模式内容流（歌单/电台/榜单）。
///
/// * `block_type`：客户端从 `feed_mode_block[].feed_mode[].type` 拿到（如 `discover_playlist`）
/// * `sub_channel_id`：场景下的子频道 id，默认 `0`
/// * `cursor` / `count`：翻页；首次传空串与 10~20
pub fn fetch_discover_mix(
    soda: &Soda,
    block_type: &str,
    sub_channel_id: i64,
    cursor: &str,
    count: i64,
) -> Result<serde_json::Value> {
    require_login(soda, "soda discover mix")?;
    if block_type.trim().is_empty() {
        return Err(SodaError::invalid_input(
            "soda discover mix requires block_type",
        ));
    }
    let body = discover_mix_body(block_type, sub_channel_id, cursor, count, "");
    super::pc_post_json(soda, DISCOVER_MIX_PATH, &body)
}

/// `discover/mix` 的请求体（独立出来便于离线断言）。
pub fn discover_mix_body(
    block_type: &str,
    sub_channel_id: i64,
    cursor: &str,
    count: i64,
    session_id: &str,
) -> serde_json::Value {
    let mut body = serde_json::json!({
        "block_type": block_type.trim(),
        "sub_channel_id": sub_channel_id,
        "exposure_radio_list": [],
    });
    if !cursor.trim().is_empty() {
        body["cursor"] = serde_json::json!(cursor.trim());
    }
    if count > 0 {
        body["count"] = serde_json::json!(count);
    }
    if !session_id.trim().is_empty() {
        body["session_id"] = serde_json::json!(session_id.trim());
    }
    body
}

/// 发现页首屏。
pub fn fetch_discover(soda: &Soda) -> Result<serde_json::Value> {
    require_login(soda, "soda discover")?;
    super::pc_post_json(soda, DISCOVER_PATH, &serde_json::json!({}))
}

/// 从 `feed_mode_block` 里挑出可用的场景（客户端会 `slice(0, 6)`）。
///
/// 返回 `(block_type, sub_channel_id, title)` 三元组；结构不符时返回空列表。
pub fn extract_scene_modes(response: &serde_json::Value) -> Vec<(String, i64, String)> {
    let mut out = Vec::new();
    let blocks = response
        .get("feed_mode_block")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    for block in blocks {
        let modes = block
            .get("feed_mode")
            .and_then(|value| value.as_array())
            .cloned()
            .unwrap_or_default();
        for mode in modes {
            let block_type = mode
                .get("type")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .trim()
                .to_string();
            if block_type.is_empty() {
                continue;
            }
            let title = mode
                .get("title")
                .or_else(|| mode.get("name"))
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .trim()
                .to_string();
            let sub_channel_id = mode
                .get("sub_channel_id")
                .and_then(|value| value.as_i64())
                .unwrap_or(0);
            out.push((block_type, sub_channel_id, title));
        }
    }
    out
}

impl Soda {
    /// 官方电台队列使用的 `FeedRadioTracks`。
    pub fn fetch_feed_radio_tracks_body(
        &self,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        require_login(self, "soda feed radio tracks")?;
        super::pc_post_json(self, FEED_RADIO_TRACKS_PATH, body)
    }

    /// 官方推荐队列使用的 `FeedSongTab`。
    pub fn fetch_feed_song_tab(&self, body: &serde_json::Value) -> Result<serde_json::Value> {
        fetch_feed_song_tab(self, body)
    }

    /// 听歌模式（场景模式）列表。
    pub fn fetch_feed_mode(&self) -> Result<serde_json::Value> {
        fetch_feed_mode(self)
    }

    /// 场景模式内容流。
    pub fn fetch_discover_mix(
        &self,
        block_type: &str,
        sub_channel_id: i64,
        cursor: &str,
        count: i64,
    ) -> Result<serde_json::Value> {
        fetch_discover_mix(self, block_type, sub_channel_id, cursor, count)
    }

    /// 发现页首屏。
    pub fn fetch_discover(&self) -> Result<serde_json::Value> {
        fetch_discover(self)
    }

    /// 用完整请求体调用发现页。
    pub fn fetch_discover_body(&self, body: &serde_json::Value) -> Result<serde_json::Value> {
        require_login(self, "soda discover")?;
        super::pc_post_json(self, DISCOVER_PATH, body)
    }

    /// 用完整请求体调用 `discover/mix`，便于兼容官方新 block 类型。
    pub fn fetch_discover_mix_body(&self, body: &serde_json::Value) -> Result<serde_json::Value> {
        require_login(self, "soda discover mix")?;
        super::pc_post_json(self, DISCOVER_MIX_PATH, body)
    }

    /// 听歌模式（类型化回包，含 `scenes()` 拍平结果）。
    pub fn feed_mode(&self) -> Result<FeedModeResponse> {
        parse_feed_mode(&fetch_feed_mode(self)?)
    }

    /// 场景模式内容流（类型化回包）。
    pub fn discover_mix(
        &self,
        block_type: &str,
        sub_channel_id: i64,
        cursor: &str,
        count: i64,
    ) -> Result<DiscoverMixResponse> {
        parse_discover_mix(&fetch_discover_mix(
            self,
            block_type,
            sub_channel_id,
            cursor,
            count,
        )?)
    }

    /// 从 `feed_mode` 回包里提取 `(block_type, sub_channel_id, title)`。
    pub fn scene_modes(&self, response: &serde_json::Value) -> Vec<(String, i64, String)> {
        extract_scene_modes(response)
    }
}

#[cfg(test)]
mod model_tests {
    use super::*;

    /// 真实回包片段（2026-09-15 抓自 `/luna/pc/feed/mode`）。
    const FEED_MODE_SAMPLE: &str = r#"{
        "status_code": 0,
        "feed_mode_block": [{
            "title": "场景",
            "type": "scene_mode",
            "feed_mode": [{
                "cutover_toast": "已为你开启深夜 EMO",
                "entity": {"feed_scene_mode": {"scene_mode_id": 4, "sub_queue_type": "scene_mode_emo"}},
                "text": "深夜 EMO",
                "type": "scene_mode",
                "url_info": {
                    "template_prefix": "tplv-b829550vbb",
                    "uri": "tos-cn-i-b829550vbb/2756dc722481aadf5fefdb15f1813ee1.png",
                    "urls": ["https://p3-luna.douyinpic.com/img/"]
                }
            }]
        }]
    }"#;

    /// 真实回包片段（抓自 `/luna/pc/discover/mix`）。
    const DISCOVER_SAMPLE: &str = r#"{
        "has_more": true,
        "inner_block": [{
            "inner_block_id": "7031074411738515464",
            "resources": [{
                "entity": {"playlist": {
                    "id": "7031074411738515464",
                    "title": "KTV情侣对唱",
                    "public_title": "KTV情侣对唱· 用歌声撒狗粮呀～",
                    "desc": "适合去ktv和自己的对象一起唱起来～",
                    "count_tracks": 20,
                    "type": 3,
                    "url_cover": {
                        "template_prefix": "tplv-b829550vbb",
                        "uri": "ies-music/pgc_cover_3656fc2edd76a1f76f5ef87ee9818998",
                        "urls": ["https://p3-luna.douyinpic.com/img/"]
                    }
                }},
                "fallback_type": "fallback_tcc_downgrade"
            }]
        }]
    }"#;

    #[test]
    fn feed_mode_sample_parses_and_flattens() {
        let value: serde_json::Value = serde_json::from_str(FEED_MODE_SAMPLE).unwrap();
        let parsed = parse_feed_mode(&value).expect("parse");
        assert_eq!(parsed.feed_mode_block.len(), 1);
        let scenes = parsed.scenes();
        assert_eq!(scenes.len(), 1);
        assert_eq!(scenes[0].text, "深夜 EMO");
        assert_eq!(scenes[0].scene_mode_id, 4);
        assert_eq!(scenes[0].sub_queue_type, "scene_mode_emo");
        assert_eq!(scenes[0].entry_type, "scene_mode");
        assert!(scenes[0]
            .cover_url
            .starts_with("https://p3-luna.douyinpic.com/img/"));
    }

    #[test]
    fn discover_mix_sample_parses_playlist() {
        let value: serde_json::Value = serde_json::from_str(DISCOVER_SAMPLE).unwrap();
        let parsed = parse_discover_mix(&value).expect("parse");
        assert!(parsed.has_more);
        assert_eq!(parsed.inner_block.len(), 1);
        let playlist = &parsed.inner_block[0].resources[0].entity.playlist;
        assert_eq!(playlist.id, "7031074411738515464");
        assert_eq!(playlist.count_tracks, 20);
        assert_eq!(playlist.display_title(), "KTV情侣对唱· 用歌声撒狗粮呀～");
        assert!(playlist.cover_url().contains("ies-music/pgc_cover_"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discover_body_omits_empty_optional_fields() {
        let body = discover_mix_body("discovery_playlist", 0, "", 0, "");
        assert_eq!(body["block_type"], "discovery_playlist");
        assert_eq!(body["sub_channel_id"], 0);
        assert!(body.get("cursor").is_none());
        assert!(body.get("count").is_none());
        assert_eq!(body["exposure_radio_list"], serde_json::json!([]));
    }

    #[test]
    fn discover_body_keeps_paging_fields() {
        let body = discover_mix_body("discovery_radio", 7, "cursor-1", 20, "sess-1");
        assert_eq!(body["cursor"], "cursor-1");
        assert_eq!(body["count"], 20);
        assert_eq!(body["session_id"], "sess-1");
        assert_eq!(body["sub_channel_id"], 7);
    }

    #[test]
    fn scene_modes_are_extracted_from_blocks() {
        let response = serde_json::json!({
            "feed_mode_block": [{
                "feed_mode": [
                    {"type": "discovery_playlist", "title": "场景歌单", "sub_channel_id": 3},
                    {"type": "discovery_radio", "name": "电台"},
                    {"title": "缺 type"}
                ]
            }]
        });
        let scenes = extract_scene_modes(&response);
        assert_eq!(scenes.len(), 2);
        assert_eq!(
            scenes[0],
            ("discovery_playlist".to_string(), 3, "场景歌单".to_string())
        );
        assert_eq!(
            scenes[1],
            ("discovery_radio".to_string(), 0, "电台".to_string())
        );
    }
}
