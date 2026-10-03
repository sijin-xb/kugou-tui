//! 汽水接口的响应结构与 JSON 解析。
//!
//! # 为什么要自己解析而不 derive
//!
//! 汽水的响应布局很不统一，同一份 JSON 里同一个字段可能是字符串、对象、
//! 数组或缺失，而且**字段名大小写混用**（`play_info_list` 是小写下划线，
//! `MainPlayUrl` 是 PascalCase）。derive 会在这类输入上直接失败，
//! 一个字段的意外类型不该让整首歌查不出来。
//!
//! 所以这里统一走**防御式取值**（[`pick_i64`] / [`pick_string`] 那一套，
//! 与项目内其它音源共用）：任何一段缺失只丢那一条数据，不会 panic，
//! 也不会因为上游改了个字段布局就整页空白。

use serde_json::Value;

use crate::api::model::Song;
use crate::api::model::{Singer, pick_i64, pick_string, pick_u32, pick_u64};

/// 图片描述：可能给 `urls` 数组，也可能给 `uri` + `template_prefix`。
///
/// 汽水的两种形态不能互相替代：搜索结果给前者，详情页给后者。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImageRef {
    pub urls: Vec<String>,
    pub uri: String,
    pub template_prefix: String,
}

impl ImageRef {
    fn from_json(value: &Value) -> Self {
        Self {
            urls: value
                .get("urls")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str().map(str::to_string))
                        .filter(|url| !url.trim().is_empty())
                        .collect()
                })
                .unwrap_or_default(),
            uri: pick_string(value, &["uri"]).unwrap_or_default(),
            template_prefix: pick_string(value, &["template_prefix", "templatePrefix"])
                .unwrap_or_default(),
        }
    }

    /// 拼出可直接下载的封面地址。
    ///
    /// 两种形态各有各的拼法（`uri` + `template_prefix` 要走抖音图床并带
    /// `-resize` 后缀；`urls` 直接用），所以必须都支持——只认一种的话，
    /// 搜索结果有封面、详情页没封面（反之亦然）。
    pub fn to_url(&self) -> Option<String> {
        if !self.uri.is_empty() && !self.template_prefix.is_empty() {
            return Some(format!(
                "https://p3-luna.douyinpic.com/img/{}~{}-resize:960:960.png",
                self.uri, self.template_prefix
            ));
        }

        let first = self.urls.first()?.trim();
        if first.is_empty() {
            return None;
        }
        // 有些条目 urls 为空但有 uri，且 uri 没被拼进 urls
        if !self.uri.is_empty() && !first.contains(self.uri.as_str()) {
            return Some(format!("{first}{}", self.uri));
        }
        // 没有 ~ 后缀时补上：抖音图床缺了它会返回原图（十几 MB），终端渲染扛不住
        if !first.contains('~') {
            return Some(format!("{first}~tplv:dygcmahweh.image"));
        }
        Some(first.to_string())
    }
}

/// 版权/权益标签，决定一首歌是否需要会员。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LabelInfo {
    pub only_vip_download: bool,
    pub only_vip_playable: bool,
    pub quality_only_vip_can_play: Vec<String>,
    pub quality_map: Vec<(String, bool)>,
}

impl LabelInfo {
    /// 这首歌是否涉及会员权益（任一音质需要 VIP 即算）。
    pub fn is_vip(&self) -> bool {
        self.only_vip_download
            || self.only_vip_playable
            || !self.quality_only_vip_can_play.is_empty()
            || self.quality_map.iter().any(|(_, needs_vip)| *needs_vip)
    }

    /// 该音质档位是否需要会员。用于把「整首要 VIP」细化成「只有无损要 VIP」，
    /// 从而在降级到低一档**能拿到整曲**时如实告诉用户，而不是笼统说「要会员」。
    pub fn quality_needs_vip(&self, quality: &str) -> bool {
        let target = quality.trim().to_ascii_lowercase();
        self.quality_map
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(&target))
            .map(|(_, needs_vip)| *needs_vip)
            .unwrap_or(false)
    }

    fn from_json(value: &Value) -> Self {
        let flag = |keys: &[&str]| {
            keys.iter()
                .any(|key| matches!(value.get(*key), Some(Value::Bool(true)) | Some(Value::Number(_)) if value.get(*key).and_then(Value::as_bool) == Some(true)))
        };
        let list = |key: &str| {
            value
                .get(key)
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        };

        // quality_map 的结构是 { "<档位>": { "play_detail": { "need_vip": true } } }
        let quality_map = value
            .get("quality_map")
            .and_then(Value::as_object)
            .map(|map| {
                map.iter()
                    .map(|(name, detail)| {
                        let needs_vip = detail
                            .get("play_detail")
                            .or_else(|| detail.get("download_detail"))
                            .and_then(|policy| policy.get("need_vip"))
                            .and_then(Value::as_bool)
                            .unwrap_or(false);
                        (name.clone(), needs_vip)
                    })
                    .collect()
            })
            .unwrap_or_default();

        Self {
            only_vip_download: flag(&["only_vip_download"]),
            only_vip_playable: flag(&["only_vip_playable"]),
            quality_only_vip_can_play: list("quality_only_vip_can_play"),
            quality_map,
        }
    }
}

/// 一条曲目（搜索结果与详情页共用）。
#[derive(Debug, Clone, Default)]
pub struct Track {
    pub id: String,
    pub name: String,
    /// 时长（毫秒）。领域模型要毫秒，汽水给的是毫秒，无需换算。
    pub duration_ms: u64,
    pub artists: Vec<Singer>,
    pub album_id: String,
    pub album_name: String,
    pub cover: Option<String>,
    pub label: LabelInfo,
    /// 逐字歌词原文（详情页才带，搜索结果为空）。
    pub lyric_raw: String,
}

impl Track {
    /// 从一条 JSON 解析。`id` 缺失直接放弃这一条（它没法当主键用）。
    pub fn from_json(value: &Value) -> Option<Self> {
        let id = pick_string(value, &["id"])?.trim().to_string();
        if id.is_empty() {
            return None;
        }

        let album = value.get("album");
        // 封面在 `album.url_cover` 里（实测形状），不是直接挂在 album 上。
        // 但也有接口把它平铺在 album 自身（`album.uri`），两处都认。
        let cover = album.and_then(|album| {
            album
                .get("url_cover")
                .or_else(|| album.get("cover"))
                .map(ImageRef::from_json)
                .or_else(|| {
                    // 平铺形态：album 自身就带 uri / urls
                    let image = ImageRef::from_json(album);
                    if image.uri.is_empty() && image.urls.is_empty() {
                        None
                    } else {
                        Some(image)
                    }
                })
                .and_then(|image| image.to_url())
        });

        Some(Self {
            id,
            name: pick_string(value, &["name", "title"]).unwrap_or_else(|| "未知曲目".to_string()),
            duration_ms: pick_u64(value, &["duration"]).unwrap_or_default(),
            artists: parse_artists(value),
            album_id: album
                .and_then(|album| pick_string(album, &["id"]))
                .unwrap_or_default(),
            album_name: album
                .and_then(|album| pick_string(album, &["name"]))
                .unwrap_or_default(),
            cover,
            label: value
                .get("label_info")
                .map(LabelInfo::from_json)
                .unwrap_or_default(),
            lyric_raw: extract_lyric(value),
        })
    }

    /// 转成领域模型。
    ///
    /// `hash` 复用为曲目 id：它是本音源取链与取歌词的唯一主键，
    /// 与酷狗的 FileHash、网易云的数字 id 在各自音源内语义等价。
    /// `privilege` 借用为「是否需要会员」的标记（`1` = 需要），
    /// 让播放前的预警逻辑不必为每个音源各写一套。
    pub fn to_song(&self) -> Song {
        Song {
            name: self.name.clone(),
            hash: self.id.clone(),
            album_id: self.album_id.clone(),
            album_audio_id: 0,
            album_name: self.album_name.clone(),
            singers: self.artists.clone(),
            duration_ms: self.duration_ms,
            cover: self.cover.clone(),
            privilege: self.label.is_vip().then_some(1),
            file_id: None,
            extra_hashes: Default::default(),
            source: crate::source::SourceKind::Sodam,
        }
    }
}

/// 歌手数组。字段名在不同接口下可能是 `artists` 或 `artist`。
fn parse_artists(value: &Value) -> Vec<Singer> {
    let Some(artists) = value
        .get("artists")
        .or_else(|| value.get("artist"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    artists
        .iter()
        .filter_map(|artist| {
            Some(Singer {
                id: pick_i64(artist, &["id"]).unwrap_or_default(),
                name: pick_string(artist, &["name"])?,
            })
        })
        .collect()
}

/// 从详情响应里挖出歌词正文。
///
/// 歌词可能挂在 `lyric.content`，也可能在 `lyric` 直接是字符串；
/// 两处都试。
fn extract_lyric(value: &Value) -> String {
    let Some(lyric) = value.get("lyric") else {
        return String::new();
    };
    pick_string(lyric, &["content"])
        .or_else(|| lyric.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// 一个播放流候选。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StreamCandidate {
    pub url: String,
    /// 加密密钥（`play_auth`）。**空字符串表示明文流**，可直接交给播放器。
    pub play_auth: String,
    pub format: String,
    pub quality: String,
    /// 时长（毫秒）。
    pub duration_ms: u64,
    pub bitrate: u32,
    pub size: u64,
    /// 来自哪个端点：`pc`（App 端点，可拿整曲）/ `web`（分享页，只有试听）。
    pub origin: &'static str,
}

impl StreamCandidate {
    /// 是否加密。需要下载后解密才能播。
    pub fn is_encrypted(&self) -> bool {
        !self.play_auth.trim().is_empty()
    }

    /// 是否明显短于整曲（试听片段）。
    pub fn is_preview(&self, full_duration_ms: u64) -> bool {
        // 5 秒容差：编码器常让各档时长有零点几秒的差异，
        // 不留容差会把「同一首歌的高音质档」误判成试听。
        self.duration_ms > 0 && full_duration_ms > 0 && self.duration_ms + 5_000 < full_duration_ms
    }
}

/// 音质档位评分：数值越大越好。
///
/// 与 libresoda 的 `qualityRank` 同源：先看标签与格式判无损，
/// 再看标签判特殊音质（杜比/氛围），最后退回码率分档。
pub fn quality_rank(quality: &str, format: &str, bitrate: u32) -> i32 {
    let q = quality.trim().to_ascii_lowercase();
    let f = format.trim().to_ascii_lowercase();
    let br = bitrate / 1000; // bps → kbps

    let lossless_format = f.contains("flac") || f.contains("alac") || f.contains("wav");
    let lossless_label =
        q.contains("lossless") || q.contains("flac") || q.contains("sq") || q.contains("svip");
    let hires_label = q.contains("hires") || q.contains("master");

    if hires_label && (lossless_format || br >= 900) {
        return 110;
    }
    if lossless_label || lossless_format || br >= 900 {
        return 100;
    }
    if hires_label {
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
        v if v >= 900 => 100,
        v if v >= 320 => 70,
        v if v >= 256 => 65,
        v if v >= 192 => 55,
        v if v >= 128 => 50,
        v if v > 0 => 20,
        _ => 0,
    }
}

/// 候选流择优：**先比完整性，再比音质**。
///
/// 顺序不能反：试听片段的码率可能比整曲还高（整曲被降级到 128k 时
/// 试听反而是 320k），先比音质就会把 30 秒片段选成「最佳」——
/// 那正是这个函数存在的意义。
pub fn better_candidate(
    candidate: &StreamCandidate,
    current: &StreamCandidate,
    full_ms: u64,
) -> bool {
    let candidate_preview = candidate.is_preview(full_ms);
    let current_preview = current.is_preview(full_ms);
    if candidate_preview != current_preview {
        return !candidate_preview;
    }

    let candidate_rank = quality_rank(&candidate.quality, &candidate.format, candidate.bitrate);
    let current_rank = quality_rank(&current.quality, &current.format, current.bitrate);
    if candidate_rank != current_rank {
        return candidate_rank > current_rank;
    }
    if candidate.bitrate != current.bitrate {
        return candidate.bitrate > current.bitrate;
    }
    candidate.size > current.size
}

/// 音质配置值 → 该档位的最低可接受评分。
///
/// 用来**约束选流**：用户选了 flac 就不该拿 128k 的整曲冒充。
/// 返回 `None` 表示「这一档随便给什么都行」（128 是底线，什么都能满足）。
pub fn min_rank_for(quality: &str) -> Option<i32> {
    match quality.trim().to_ascii_lowercase().as_str() {
        "flac" => Some(90),  // 无损或 Hi-Res
        "high" => Some(70),  // 320k 上下
        "super" => Some(80), // 高码率
        "128" => None,       // 底线
        _ => None,
    }
}

/// 解析 `url_player_info`（取流信息）响应。
///
/// 注意字段名是 **PascalCase**（`MainPlayUrl` / `PlayAuth` / `Bitrate`…），
/// 与其它接口的小写下划线风格完全不同——这是实测形状，不是笔误。
pub fn parse_player_info(value: &Value) -> Vec<StreamCandidate> {
    let Some(list) = value
        .get("result")
        .and_then(|result| result.get("data"))
        .and_then(|data| data.get("play_info_list"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };

    list.iter()
        .filter_map(|entry| {
            let url = pick_string(entry, &["MainPlayUrl", "main_play_url"])
                .filter(|url| !url.trim().is_empty())
                .or_else(|| {
                    pick_string(entry, &["BackupPlayUrl", "backup_play_url"])
                        .filter(|url| !url.trim().is_empty())
                })?;
            Some(StreamCandidate {
                url,
                play_auth: pick_string(entry, &["PlayAuth", "play_auth"]).unwrap_or_default(),
                format: pick_string(entry, &["Format", "format"]).unwrap_or_default(),
                quality: pick_string(entry, &["Quality", "quality"]).unwrap_or_default(),
                duration_ms: pick_u64(entry, &["Duration", "duration"]).unwrap_or_default(),
                bitrate: pick_u32(entry, &["Bitrate", "bitrate"]).unwrap_or_default(),
                size: pick_u64(entry, &["Size", "size"]).unwrap_or_default(),
                origin: "web",
            })
        })
        .collect()
}

/// 从 `track_v2` 响应里取所有候选流。
///
/// 两条可能的来源，都要收：
///
/// * `track_player.video_model` —— 内嵌的流信息；
/// * `track_player.url_player_info` —— **另一个 URL**，要再发一次请求才拿得到内容。
///   少了它会漏掉相当一部分可播流。
pub fn extract_candidates(track_v2: &Value) -> (Vec<StreamCandidate>, Vec<String>) {
    let mut candidates = Vec::new();
    let mut follow_up = Vec::new();

    let Some(player) = track_v2.get("track_player") else {
        return (candidates, follow_up);
    };

    if let Some(model) = player.get("video_model") {
        collect_from_video_model(model, &mut candidates);
    }

    if let Some(url) = pick_string(player, &["url_player_info"])
        && !url.trim().is_empty()
    {
        follow_up.push(url);
    }

    (candidates, follow_up)
}

/// 从 `video_model` 里收流。
///
/// 它可能直接给 `play_url`，也可能给 `bit_rate` 列表 + `play_url` 模板。
/// 两种都收，取并集。
fn collect_from_video_model(model: &Value, out: &mut Vec<StreamCandidate>) {
    let play_auth = pick_string(model, &["play_auth", "PlayAuth"]).unwrap_or_default();
    let duration_ms = pick_u64(model, &["duration"]).unwrap_or_default();
    let base_url = pick_string(model, &["play_url", "PlayUrl"]).unwrap_or_default();

    if let Some(bit_rates) = model.get("bit_rate").and_then(Value::as_array) {
        for entry in bit_rates {
            let Some(url) = pick_string(entry, &["play_url", "PlayUrl"])
                .or_else(|| (!base_url.is_empty()).then_some(base_url.clone()))
            else {
                continue;
            };
            if url.trim().is_empty() {
                continue;
            }
            out.push(StreamCandidate {
                url,
                play_auth: pick_string(entry, &["play_auth", "PlayAuth"])
                    .unwrap_or_else(|| play_auth.clone()),
                format: pick_string(entry, &["format", "Format"]).unwrap_or_default(),
                quality: pick_string(entry, &["quality", "Quality"]).unwrap_or_default(),
                duration_ms: pick_u64(entry, &["duration"]).unwrap_or(duration_ms),
                bitrate: pick_u32(entry, &["bitrate", "Bitrate"]).unwrap_or_default(),
                size: pick_u64(entry, &["size", "Size"]).unwrap_or_default(),
                origin: "web",
            });
        }
    }

    // 没有 bit_rate 但直接给了 play_url 的情况
    if out.is_empty() && !base_url.trim().is_empty() {
        out.push(StreamCandidate {
            url: base_url,
            play_auth,
            format: pick_string(model, &["format", "Format"]).unwrap_or_default(),
            quality: pick_string(model, &["quality", "Quality"]).unwrap_or_default(),
            duration_ms,
            bitrate: pick_u32(model, &["bitrate", "Bitrate"]).unwrap_or_default(),
            size: pick_u64(model, &["size", "Size"]).unwrap_or_default(),
            origin: "web",
        });
    }
}

/// 汽水的业务错误码：响应里 `status_code != 0` 即为失败。
pub fn status_code(value: &Value) -> i64 {
    value
        .get("status_code")
        .and_then(Value::as_i64)
        .unwrap_or(0)
}

/// 业务错误信息。
pub fn status_message(value: &Value) -> String {
    value
        .get("status_info")
        .and_then(|info| pick_string(info, &["status_msg", "StatusMsg"]))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_track() -> Value {
        json!({
            "id": "7304719759323564095",
            "name": "测试歌曲",
            "duration": 240_000,
            "artists": [{"id": 1, "name": "歌手甲"}],
            "album": {
                "id": "alb-1",
                "name": "测试专辑",
                "url_cover": {"uri": "tos-cn-i-abc", "template_prefix": "abcdef"}
            },
            "label_info": {"only_vip_playable": false}
        })
    }

    #[test]
    fn track_requires_id() {
        assert!(Track::from_json(&json!({"name": "无 id"})).is_none());
        assert!(Track::from_json(&json!({"id": "  "})).is_none());
    }

    #[test]
    fn track_parses_full_shape() {
        let track = Track::from_json(&sample_track()).unwrap();
        assert_eq!(track.id, "7304719759323564095");
        assert_eq!(track.name, "测试歌曲");
        assert_eq!(track.duration_ms, 240_000);
        assert_eq!(track.artists.len(), 1);
        assert_eq!(track.artists[0].name, "歌手甲");
        assert_eq!(track.album_name, "测试专辑");
        // uri + template_prefix 形态要拼成抖音图床地址
        let cover = track.cover.unwrap();
        assert!(cover.starts_with("https://p3-luna.douyinpic.com/img/"));
        assert!(cover.contains("resize"));
    }

    #[test]
    fn track_survives_missing_optional_fields() {
        // 只有 id 的极端情况：其余字段全空，不能 panic
        let track = Track::from_json(&json!({"id": "x"})).unwrap();
        assert_eq!(track.name, "未知曲目");
        assert!(track.artists.is_empty());
        assert!(track.cover.is_none());
        assert_eq!(track.duration_ms, 0);
    }

    #[test]
    fn track_accepts_singular_artist_field() {
        let track = Track::from_json(&json!({
            "id": "x",
            "artist": [{"id": 2, "name": "独唱"}]
        }))
        .unwrap();
        assert_eq!(track.artists[0].name, "独唱");
    }

    #[test]
    fn to_song_marks_vip_via_privilege() {
        let mut value = sample_track();
        value["label_info"]["only_vip_playable"] = json!(true);
        let song = Track::from_json(&value).unwrap().to_song();
        assert_eq!(song.privilege, Some(1), "VIP 曲目要打上标记");
        assert_eq!(song.source, crate::source::SourceKind::Sodam);
        assert_eq!(song.hash, "7304719759323564095");

        // 非 VIP 曲目不能被误标
        let normal = Track::from_json(&sample_track()).unwrap().to_song();
        assert_eq!(normal.privilege, None);
    }

    #[test]
    fn label_info_detects_vip_from_any_signal() {
        assert!(!LabelInfo::default().is_vip());
        assert!(LabelInfo::from_json(&json!({"only_vip_playable": true})).is_vip());
        assert!(LabelInfo::from_json(&json!({"only_vip_download": true})).is_vip());
        assert!(LabelInfo::from_json(&json!({"quality_only_vip_can_play": ["lossless"]})).is_vip());
        assert!(
            LabelInfo::from_json(&json!({
                "quality_map": {"lossless": {"play_detail": {"need_vip": true}}}
            }))
            .is_vip()
        );
    }

    #[test]
    fn quality_map_answers_per_quality_vip() {
        let label = LabelInfo::from_json(&json!({
            "quality_map": {
                "lossless": {"play_detail": {"need_vip": true}},
                "standard": {"play_detail": {"need_vip": false}}
            }
        }));
        assert!(label.quality_needs_vip("lossless"));
        assert!(!label.quality_needs_vip("standard"));
        assert!(!label.quality_needs_vip("从未出现过的档位"));
    }

    #[test]
    fn image_url_handles_urls_form() {
        let image = ImageRef {
            urls: vec!["https://p3.douyinpic.com/abc".to_string()],
            ..Default::default()
        };
        let url = image.to_url().unwrap();
        assert!(url.starts_with("https://p3.douyinpic.com/abc"));
        // 缺 ~ 后缀要补，否则图床返回原图
        assert!(url.contains('~'));
    }

    #[test]
    fn image_url_is_none_when_empty() {
        assert!(ImageRef::default().to_url().is_none());
        assert!(
            ImageRef {
                urls: vec!["  ".into()],
                ..Default::default()
            }
            .to_url()
            .is_none()
        );
    }

    #[test]
    fn quality_rank_orders_tiers() {
        let lossless = quality_rank("lossless", "flac", 1000);
        let normal = quality_rank("standard", "mp3", 128);
        let preview = quality_rank("low", "mp3", 64);
        assert!(lossless > normal, "无损应高于标准");
        assert!(normal > preview, "标准应高于试听");
        // 杜比/氛围在无损之下、320 之上
        let dolby = quality_rank("dolby", "mp4", 256);
        assert!(normal < dolby);
    }

    #[test]
    fn quality_rank_uses_bitrate_when_label_is_unknown() {
        // 无标签时退回码率分档
        assert_eq!(quality_rank("", "", 320_000), 70);
        assert_eq!(quality_rank("", "", 128_000), 50);
    }

    /// 核心不变式：试听片段哪怕码率更高，也不能被选成最佳。
    #[test]
    fn better_candidate_prefers_full_track_over_higher_bitrate_preview() {
        let full_ms = 240_000;
        // 整曲被降级成 128k
        let full = StreamCandidate {
            url: "full".into(),
            duration_ms: 240_000,
            bitrate: 128_000,
            ..Default::default()
        };
        // 试听片段码率反而更高
        let preview = StreamCandidate {
            url: "preview".into(),
            duration_ms: 30_000,
            bitrate: 320_000,
            ..Default::default()
        };
        assert!(better_candidate(&full, &preview, full_ms), "整曲应胜出");
        assert!(!better_candidate(&preview, &full, full_ms), "试听不应胜出");
    }

    #[test]
    fn better_candidate_falls_back_to_quality_when_both_complete() {
        let full_ms = 240_000;
        let flac = StreamCandidate {
            duration_ms: 240_000,
            bitrate: 1_000_000,
            quality: "lossless".into(),
            format: "flac".into(),
            ..Default::default()
        };
        let mp3 = StreamCandidate {
            duration_ms: 240_000,
            bitrate: 320_000,
            quality: "higher".into(),
            format: "mp3".into(),
            ..Default::default()
        };
        assert!(better_candidate(&flac, &mp3, full_ms));
        assert!(!better_candidate(&mp3, &flac, full_ms));
    }

    #[test]
    fn preview_detection_has_tolerance() {
        let full_ms = 240_000;
        let candidate = |ms: u64| StreamCandidate {
            duration_ms: ms,
            ..Default::default()
        };
        // 30 秒：明显是试听
        assert!(candidate(30_000).is_preview(full_ms));
        // 与整曲等长：不是试听
        assert!(!candidate(240_000).is_preview(full_ms));
        // 差 3 秒：在容差内，仍算整曲
        assert!(!candidate(236_000).is_preview(full_ms));
        // 时长未知时不能瞎判
        assert!(!candidate(0).is_preview(full_ms));
        assert!(!StreamCandidate::default().is_preview(0));
    }

    #[test]
    fn parse_player_info_reads_pascal_case_fields() {
        let value = json!({
            "result": {
                "data": {
                    "play_info_list": [
                        {
                            "MainPlayUrl": "https://cdn.example.com/a.m4a",
                            "BackupPlayUrl": "",
                            "PlayAuth": "AUTH-1",
                            "Bitrate": 320000,
                            "Format": "m4a",
                            "Quality": "higher",
                            "Duration": 240000,
                            "Size": 9_600_000
                        },
                        {
                            "MainPlayUrl": "",
                            "BackupPlayUrl": "https://cdn.example.com/b.m4a",
                            "PlayAuth": "AUTH-2"
                        },
                        {"MainPlayUrl": "   "}
                    ]
                }
            }
        });
        let candidates = parse_player_info(&value);
        assert_eq!(candidates.len(), 2, "空的条目要被丢掉");
        assert_eq!(candidates[0].url, "https://cdn.example.com/a.m4a");
        assert_eq!(candidates[0].play_auth, "AUTH-1");
        assert!(candidates[0].is_encrypted());
        // 主链为空时应回退到备用链
        assert_eq!(candidates[1].url, "https://cdn.example.com/b.m4a");
    }

    #[test]
    fn parse_player_info_on_wrong_shape_is_empty() {
        assert!(parse_player_info(&json!({})).is_empty());
        assert!(parse_player_info(&json!({"result": {}})).is_empty());
    }

    #[test]
    fn extract_candidates_collects_video_model_and_followup() {
        let value = json!({
            "track_player": {
                "video_model": {
                    "play_url": "https://cdn.example.com/v.m4a",
                    "play_auth": "AUTH-V",
                    "duration": 240000,
                    "bit_rate": [
                        {"bitrate": 128000, "quality": "standard", "format": "mp3", "play_url": "https://cdn.example.com/128.mp3"},
                        {"bitrate": 320000, "quality": "higher", "format": "m4a", "play_url": "https://cdn.example.com/320.m4a"}
                    ]
                },
                "url_player_info": "https://api.qishui.com/player/info"
            }
        });
        let (candidates, follow_up) = extract_candidates(&value);
        assert_eq!(candidates.len(), 2);
        assert_eq!(follow_up, vec!["https://api.qishui.com/player/info"]);
        // play_auth 要能从父级继承到每个 bit_rate 条目
        assert!(candidates.iter().all(|c| c.play_auth == "AUTH-V"));
    }

    #[test]
    fn extract_candidates_on_empty_player() {
        let (candidates, follow_up) = extract_candidates(&json!({}));
        assert!(candidates.is_empty());
        assert!(follow_up.is_empty());
    }

    #[test]
    fn status_helpers_read_error_fields() {
        let value = json!({"status_code": 8, "status_info": {"status_msg": "需要登录"}});
        assert_eq!(status_code(&value), 8);
        assert_eq!(status_message(&value), "需要登录");
        // 成功时 status_code 为 0，消息为空
        let ok = json!({"status_code": 0});
        assert_eq!(status_code(&ok), 0);
        assert!(status_message(&ok).is_empty());
    }

    #[test]
    fn min_rank_for_gates_qualities() {
        assert_eq!(min_rank_for("flac"), Some(90));
        assert_eq!(min_rank_for("FLAC"), Some(90), "大小写不敏感");
        assert_eq!(min_rank_for("high"), Some(70));
        // 128 是底线，不设门槛
        assert_eq!(min_rank_for("128"), None);
        assert_eq!(min_rank_for("未知档位"), None);
    }
}
