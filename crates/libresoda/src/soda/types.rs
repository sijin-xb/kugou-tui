//! 汽水接口的原始数据结构（逐字段对应上游 `soda/soda.go` 中的 `soda*` 类型）。

use crate::util::{extra_from_pairs, join_artists};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// PC 端 UA。
pub const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/134.0.0.0 Safari/537.36";
/// PC App UA（`pc/me`、`pc/track_v2` 等接口使用）。
pub const PC_APP_USER_AGENT: &str = "LunaPC/3.3.0(359450208)";
/// 用于探测账号 VIP 状态的曲目。
pub const VIP_PROBE_TRACK_ID: &str = "7304719759323564095";
/// 探测用曲目的短链。
pub const VIP_PROBE_TRACK_URL: &str = "https://qishui.douyin.com/s/iQeFw9cE/";
/// H5 SEO 单曲接口（无需签名）。
pub const SODA_SEO_BASE: &str = "https://beta-luna.douyin.com/luna/h5/seo_track";
/// Android 搜索网关。
pub const SODA_ANDROID_API_BASE: &str = "https://api.qishui.com/luna";
/// Android 搜索 UA。
pub const SODA_ANDROID_SEARCH_USER_AGENT: &str = "com.luna.music/100198030 (Linux; U; Android 15; zh_CN_#Hans; ABR-AL80; Build/V417IR;tt-ok/3.12.13.19)";
/// Android 搜索每页条数。
pub const SODA_ANDROID_SEARCH_PAGE_SIZE: i64 = 20;
/// 抖音图床前缀。
pub const SODA_DOUYIN_IMAGE_BASE_URL: &str = "https://p3-luna.douyinpic.com/img/";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ArtistStats {
    pub count_collected: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Artist {
    pub id: String,
    pub name: String,
    pub count_tracks: i64,
    pub stats: ArtistStats,
    pub url_avatar: Image,
}

/// 图片描述：可能给 `urls`，也可能给 `uri` + `template_prefix`。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Image {
    pub urls: Vec<String>,
    pub uri: String,
    pub template_prefix: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BitRate {
    #[serde(rename = "br")]
    pub br: i64,
    pub quality: String,
    pub size: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TrackPlayInfo {
    pub main_play_url: String,
    pub backup_play_url: String,
    pub play_auth: String,
    pub size: i64,
    pub format: String,
    pub bitrate: i64,
    pub quality: String,
    pub duration: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TrackAudioInfo {
    pub play_info_list: Vec<TrackPlayInfo>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Album {
    pub id: String,
    pub name: String,
    pub url_cover: Image,
    /// 搜索结果里带的主艺人。
    pub artists: Vec<Artist>,
    /// 发行公司 / 厂牌。
    pub company: String,
    /// 曲目数量。
    pub count_tracks: i64,
    /// 发行时间（毫秒时间戳）。
    pub release_date: i64,
    /// 分享页里的简介行。
    #[serde(rename = "pclines")]
    pub pc_lines: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Preview {
    #[serde(rename = "vid")]
    pub vid: String,
    pub start: i64,
    pub duration: i64,
    pub bit_rates: Vec<BitRate>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct QualityBenefit {
    pub condition: String,
    pub need_vip: bool,
    pub need_purchase: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct QualityPolicy {
    pub play_detail: Option<QualityBenefit>,
    pub download_detail: Option<QualityBenefit>,
}

/// 版权/权益标签，决定一首歌是否需要 VIP。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LabelInfo {
    pub only_vip_download: bool,
    pub only_vip_playable: bool,
    pub quality_only_vip_can_download: Vec<String>,
    pub quality_only_vip_can_play: Vec<String>,
    pub quality_map: BTreeMap<String, QualityPolicy>,
}

impl LabelInfo {
    /// 等价上游 `sodaLabelInfo.IsVIP()`。
    pub fn is_vip(&self) -> bool {
        if self.only_vip_download || self.only_vip_playable {
            return true;
        }
        if !self.quality_only_vip_can_download.is_empty()
            || !self.quality_only_vip_can_play.is_empty()
        {
            return true;
        }
        for policy in self.quality_map.values() {
            if policy.play_detail.as_ref().is_some_and(|it| it.need_vip) {
                return true;
            }
            if policy
                .download_detail
                .as_ref()
                .is_some_and(|it| it.need_vip)
            {
                return true;
            }
        }
        false
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Track {
    pub id: String,
    pub name: String,
    pub duration: i64,
    #[serde(rename = "vid")]
    pub vid: String,
    pub artists: Vec<Artist>,
    pub album: Album,
    pub bit_rates: Vec<BitRate>,
    pub preview: Preview,
    pub label_info: LabelInfo,
    pub audio_info: TrackAudioInfo,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ApiStatusInfo {
    pub status_msg: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UserPlaylistOwner {
    pub id: String,
    pub nickname: String,
    pub public_name: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ResourceCount {
    pub track_cnt: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PlaylistStats {
    pub count_played: i64,
    pub count_collected: i64,
}

/// 歌单条目（搜索结果、歌单详情、我的歌单共用）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UserPlaylistItem {
    pub id: String,
    pub title: String,
    pub public_title: String,
    pub desc: String,
    pub url_cover: Image,
    pub count_tracks: i64,
    pub play_count: i64,
    pub owner: UserPlaylistOwner,
    pub review_status: String,
    /// 隐私歌单标记（客户端 `PlaylistMoreAction` 的「设为隐私/公开」读的就是它）。
    pub is_private: bool,
    #[serde(rename = "type")]
    pub playlist_type: i64,
    pub resource_cnt: ResourceCount,
    pub stats: PlaylistStats,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TrackWrapper {
    pub track: Track,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MediaResourceEntity {
    pub track_wrapper: TrackWrapper,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MediaResource {
    #[serde(rename = "type")]
    pub resource_type: String,
    pub entity: MediaResourceEntity,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PlaylistDetailResponse {
    pub status_code: i64,
    pub status_info: ApiStatusInfo,
    pub next_cursor: String,
    pub has_more: bool,
    pub playlist: UserPlaylistItem,
    pub media_resources: Vec<MediaResource>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LyricBody {
    pub content: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TrackPlayer {
    pub media_id: String,
    pub url_player_info: String,
    /// 上游是 `json.RawMessage`：这里保留原始 JSON，交给视频流解析逻辑处理。
    pub video_model: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TrackV2Response {
    pub status_code: i64,
    pub status_info: ApiStatusInfo,
    pub track: Track,
    pub track_info: Track,
    pub track_player: TrackPlayer,
    pub lyric: LyricBody,
}

impl TrackV2Response {
    /// 等价上游 `primaryTrack()`：优先 `track`，否则退回 `track_info`。
    pub fn primary_track(&self) -> Track {
        if !self.track.id.is_empty() {
            self.track.clone()
        } else {
            self.track_info.clone()
        }
    }
}

/// 分享页里的专辑信息（对应上游 `sodaShareAlbumPage.LoaderData.AlbumPage.AlbumInfo`）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ShareAlbumInfo {
    pub id: String,
    pub name: String,
    pub artists: Vec<Artist>,
    pub company: String,
    pub count_tracks: i64,
    pub url_cover: Image,
    pub release_date: i64,
    #[serde(rename = "pclines")]
    pub pc_lines: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SeoAlbumPage {
    #[serde(rename = "albumInfo")]
    pub album_info: ShareAlbumInfo,
    #[serde(rename = "trackList")]
    pub track_list: Vec<Track>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SeoLoaderData {
    pub album_page: SeoAlbumPage,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ShareAlbumPage {
    #[serde(rename = "loaderData")]
    pub loader_data: SeoLoaderData,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SeoTrack {
    pub track: Track,
    pub lyric: LyricBody,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SeoTrackResponse {
    pub status_code: i64,
    pub status_info: ApiStatusInfo,
    pub track_player: TrackPlayer,
    pub seo_track: SeoTrack,
    pub lyric: LyricBody,
}

/// 解析后的下载/播放流信息（对应上游 `DownloadInfo`）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DownloadInfo {
    pub url: String,
    pub play_auth: String,
    pub format: String,
    pub size: i64,
    pub duration: f64,
    pub bitrate: i64,
    pub quality: String,
    /// 是否只是试听片段（明显短于整曲）。
    ///
    /// 汽水对「仅会员可播」的曲目会给未带应用级签名（`x-helios` / `x-medusa`）
    /// 的请求下发 30～60 秒试听流；下游播放器必须靠这个标记避免把试听当整曲。
    #[serde(default)]
    pub is_preview: bool,
    /// 人工可读的补充说明（例如"试听片段：需要应用签名凭证"）。
    #[serde(default)]
    pub note: String,
}

impl DownloadInfo {
    /// 带 `#auth=` 片段的可播放/可下载地址（等价 `sodaDownloadInfoURL`）。
    pub fn full_url(&self) -> String {
        if self.play_auth.trim().is_empty() {
            return self.url.clone();
        }
        format!(
            "{}#auth={}",
            self.url,
            crate::util::query_escape(&self.play_auth)
        )
    }
}

/// player info 接口的 `PlayInfoList` 元素（注意字段名是 PascalCase）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PlayerInfo {
    #[serde(rename = "MainPlayUrl")]
    pub main_play_url: String,
    #[serde(rename = "BackupPlayUrl")]
    pub backup_play_url: String,
    #[serde(rename = "PlayAuth")]
    pub play_auth: String,
    #[serde(rename = "Size")]
    pub size: i64,
    #[serde(rename = "Bitrate")]
    pub bitrate: i64,
    #[serde(rename = "Format")]
    pub format: String,
    #[serde(rename = "Duration")]
    pub duration: f64,
    #[serde(rename = "Quality")]
    pub quality: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PlayerInfoResponse {
    #[serde(rename = "ResponseMetadata")]
    pub response_metadata: PlayerInfoMetadata,
    #[serde(rename = "Result")]
    pub result: PlayerInfoResult,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PlayerInfoMetadata {
    #[serde(rename = "Error")]
    pub error: PlayerInfoError,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PlayerInfoError {
    #[serde(rename = "Message")]
    pub message: String,
    #[serde(rename = "Code")]
    pub code: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PlayerInfoResult {
    #[serde(rename = "Data")]
    pub data: PlayerInfoData,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PlayerInfoData {
    #[serde(rename = "PlayInfoList")]
    pub play_info_list: Vec<PlayerInfo>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PCMeResponse {
    pub status_code: i64,
    pub status_info: ApiStatusInfo,
    pub my_info: PCMeInfo,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PCMeInfo {
    pub id: String,
    pub nickname: String,
    pub public_name: String,
    pub larger_avatar_url: Image,
    /// 官方 `/luna/pc/me` 直接给出的会员标记（比"探测完整流"可靠得多）。
    #[serde(default)]
    pub is_vip: bool,
    /// 会员档位，例如 `vip` / `svip`。
    #[serde(default)]
    pub vip_stage: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UserPlaylistResponse {
    pub status_code: i64,
    pub status_info: ApiStatusInfo,
    pub next_cursor: String,
    pub has_more: bool,
    pub playlists: Vec<UserPlaylistItem>,
}

/// 生成 `extra` 字段（等价 `sodaTrackExtra`）。
pub fn track_extra(
    track_id: &str,
    label: &LabelInfo,
    values: &[(&str, &str)],
) -> BTreeMap<String, String> {
    let mut extra = extra_from_pairs([
        ("track_id", track_id),
        ("is_vip", if label.is_vip() { "true" } else { "false" }),
    ]);
    if label.only_vip_download {
        extra.insert("only_vip_download".into(), "true".into());
    }
    if label.only_vip_playable {
        extra.insert("only_vip_playable".into(), "true".into());
    }
    if !label.quality_only_vip_can_download.is_empty() {
        extra.insert(
            "vip_download_qualities".into(),
            label.quality_only_vip_can_download.join(","),
        );
    }
    if !label.quality_only_vip_can_play.is_empty() {
        extra.insert(
            "vip_play_qualities".into(),
            label.quality_only_vip_can_play.join(","),
        );
    }
    for (key, value) in values {
        if !value.trim().is_empty() {
            extra.insert((*key).to_string(), (*value).to_string());
        }
    }
    extra
}

/// 等价 `sodaJoinArtists`。
pub fn join_track_artists(artists: &[Artist]) -> String {
    join_artists(artists.iter().map(|artist| artist.name.as_str()))
}

/// 构造 `https://www.qishui.com/track/<id>`。
pub fn track_link(track_id: &str) -> String {
    format!("https://www.qishui.com/track/{track_id}")
}

/// 构造 `https://www.qishui.com/playlist/<id>`。
pub fn playlist_link(playlist_id: &str) -> String {
    format!("https://www.qishui.com/playlist/{playlist_id}")
}

/// 构造专辑分享链接（等价 `sodaAlbumLink`）。
pub fn album_link(album_id: &str) -> String {
    format!(
        "https://www.qishui.com/share/album?album_id={}",
        album_id.trim()
    )
}

/// 等价 `sodaBuildImageURL`。
pub fn build_image_url(image: &Image, suffix: &str) -> String {
    let Some(first) = image.urls.first() else {
        return String::new();
    };

    let mut cover = first.trim().to_string();
    let uri = image.uri.trim();
    let template_prefix = image.template_prefix.trim();
    if !uri.is_empty() && !template_prefix.is_empty() {
        return format!(
            "{}/{uri}~{template_prefix}-resize:960:960.png",
            SODA_DOUYIN_IMAGE_BASE_URL.trim_end_matches('/')
        );
    }
    if !uri.is_empty() && !cover.contains(uri) {
        cover.push_str(uri);
    }
    if cover.is_empty() {
        return String::new();
    }
    if !suffix.is_empty() && !cover.contains('~') {
        cover.push_str(suffix);
    }
    cover
}

/// 等价 `sodaMaxBitRateSize`。
pub fn max_bitrate_size(bit_rates: &[BitRate]) -> i64 {
    bit_rates.iter().map(|item| item.size).max().unwrap_or(0)
}
