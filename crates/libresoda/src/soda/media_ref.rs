//! 媒体引用：官方 IDL 里写操作统一用 `{"id": "...", "type": "..."}` 表示一条媒体。
//!
//! 上游 `music-lib` 没有对应实现（它只有读接口）；这是给自写客户端补的写操作基础类型。

use serde::{Deserialize, Serialize};

/// 媒体类型：单曲。客户端在做队列/播放时也会用 `media_type` 表示别的形态，
/// 但对 `collection` / `playlist media` 这类接口，单曲固定是 `track`。
pub const MEDIA_TYPE_TRACK: &str = "track";

/// 媒体类型：UGC 短片（官方 `MediaTypeUgcClip`）。歌单/收藏接口里也会出现这种条目
///（客户端 `AddToPlaylistMenu` 对 `media_type === 'ugc_clip'` 有单独分支）。
pub const MEDIA_TYPE_UGC_CLIP: &str = "ugc_clip";

/// 一条媒体引用（`{id, type}`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaRef {
    pub id: String,
    #[serde(rename = "type")]
    pub media_type: String,
}

impl MediaRef {
    /// 单曲引用（等价客户端里的 `{ type: entity.playable?.media_type ?? 'track', id }`）。
    pub fn track(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            media_type: MEDIA_TYPE_TRACK.to_string(),
        }
    }

    pub fn new(id: impl Into<String>, media_type: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            media_type: media_type.into(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.id.trim().is_empty()
    }
}

/// 把媒体列表转成接口需要的 JSON 数组，顺带过滤空 id。
pub(crate) fn media_array(media: &[MediaRef]) -> serde_json::Value {
    serde_json::Value::Array(
        media
            .iter()
            .filter(|item| !item.is_empty())
            .map(|item| {
                serde_json::json!({
                    "id": item.id.trim(),
                    "type": item.media_type.trim(),
                })
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn track_ref_uses_track_type() {
        let item = MediaRef::track("7501674235158431760");
        assert_eq!(item.media_type, "track");
        assert_eq!(item.id, "7501674235158431760");
    }

    #[test]
    fn media_array_filters_blank_ids() {
        let value = media_array(&[
            MediaRef::track("1"),
            MediaRef::track("  "),
            MediaRef::track("2"),
        ]);
        assert_eq!(
            value,
            serde_json::json!([
                {"id": "1", "type": "track"},
                {"id": "2", "type": "track"},
            ])
        );
    }
}
