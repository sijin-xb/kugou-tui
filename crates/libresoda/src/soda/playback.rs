//! 播放历史（「最近播放」）读写。
//!
//! 上游 `music-lib` 没有这些接口；照官方客户端 IDL
//!（`src/idl/goapi/main.ts` 的 `GetRecentPlayedMedia` / `AppendRecentlyPlayedMedia` /
//! `DeleteRecentlyPlayedMedia`）实现：
//!
//! | 能力 | 接口 | 参数 |
//! | --- | --- | --- |
//! | 读最近播放 | `GET /luna/pc/me/recently-played-media` | `cursor` / `count` / `media_type` / `scene` |
//! | 写入播放记录 | `POST /luna/pc/me/recently-played-media` | `{media:[{id,type}]}` |
//! | 删一条记录 | `POST /luna/pc/me/recently-played-media/delete` | `{media:[{id,type}]}` |

use super::media_ref::{media_array, MediaRef};
use super::Soda;
use crate::error::{Result, SodaError};

pub const RECENTLY_PLAYED_PATH: &str = "/luna/pc/me/recently-played-media";
pub const RECENTLY_PLAYED_DELETE_PATH: &str = "/luna/pc/me/recently-played-media/delete";
/// 播放统计 / 上报（参数随版本变动，透传查询参数）。
pub const MEDIA_STATS_PATH: &str = "/luna/pc/media-stats";

/// 默认媒体类型（客户端单曲固定 `track`）。
pub const MEDIA_TYPE_TRACK: &str = "track";

fn require_login(soda: &Soda, what: &str) -> Result<()> {
    if !soda.has_cookie() {
        return Err(SodaError::invalid_input(format!("{what} requires cookie")));
    }
    Ok(())
}

/// 最近播放的查询参数（独立出来便于离线断言形状）。
pub fn recently_played_params(
    cursor: &str,
    count: i64,
    media_type: &str,
    scene: &str,
) -> Vec<(&'static str, String)> {
    let count = if count <= 0 { 20 } else { count.min(100) };
    vec![
        ("cursor", cursor.trim().to_string()),
        ("count", count.to_string()),
        (
            "media_type",
            if media_type.trim().is_empty() {
                MEDIA_TYPE_TRACK.to_string()
            } else {
                media_type.trim().to_string()
            },
        ),
        ("scene", scene.trim().to_string()),
    ]
}

/// 读最近播放（原始回包，字段随版本变动）。
pub fn recently_played_media(
    soda: &Soda,
    cursor: &str,
    count: i64,
    media_type: &str,
    scene: &str,
) -> Result<serde_json::Value> {
    require_login(soda, "soda recently played")?;
    let params = recently_played_params(cursor, count, media_type, scene);
    super::pc_get_json(soda, RECENTLY_PLAYED_PATH, &params)
}

/// 写一条播放记录（播放器在开始播放时调用）。
pub fn append_recently_played_media(soda: &Soda, media: &[MediaRef]) -> Result<serde_json::Value> {
    require_login(soda, "soda append recently played")?;
    if media.iter().filter(|item| !item.is_empty()).count() == 0 {
        return Err(SodaError::invalid_input(
            "soda append recently played requires media",
        ));
    }
    let body = serde_json::json!({ "media": media_array(media) });
    super::pc_post_json(soda, RECENTLY_PLAYED_PATH, &body)
}

/// 删除一条播放记录。
pub fn delete_recently_played_media(soda: &Soda, media: &[MediaRef]) -> Result<serde_json::Value> {
    require_login(soda, "soda delete recently played")?;
    if media.iter().filter(|item| !item.is_empty()).count() == 0 {
        return Err(SodaError::invalid_input(
            "soda delete recently played requires media",
        ));
    }
    let body = serde_json::json!({ "media": media_array(media) });
    super::pc_post_json(soda, RECENTLY_PLAYED_DELETE_PATH, &body)
}

/// 播放统计 / 上报；参数由调用方按需给（如 `media_id` / `media_type`）。
pub fn media_stats(soda: &Soda, params: &[(&str, String)]) -> Result<serde_json::Value> {
    require_login(soda, "soda media stats")?;
    super::pc_get_json(soda, MEDIA_STATS_PATH, params)
}

impl Soda {
    /// 读最近播放。
    pub fn recently_played_media(
        &self,
        cursor: &str,
        count: i64,
        media_type: &str,
        scene: &str,
    ) -> Result<serde_json::Value> {
        recently_played_media(self, cursor, count, media_type, scene)
    }

    /// 写一条播放记录。
    pub fn append_recently_played_media(&self, media: &[MediaRef]) -> Result<serde_json::Value> {
        append_recently_played_media(self, media)
    }

    /// 删除一条播放记录。
    pub fn delete_recently_played_media(&self, media: &[MediaRef]) -> Result<serde_json::Value> {
        delete_recently_played_media(self, media)
    }

    /// 播放统计 / 上报（参数透传）。
    pub fn media_stats(&self, params: &[(&str, String)]) -> Result<serde_json::Value> {
        media_stats(self, params)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_default_count_and_media_type() {
        let params = recently_played_params("", 0, "", "");
        assert_eq!(params[1], ("count", "20".to_string()));
        assert_eq!(params[2], ("media_type", "track".to_string()));
        assert_eq!(params[3], ("scene", String::new()));
    }

    #[test]
    fn params_clamps_count() {
        let params = recently_played_params("cursor-1", 500, "track", "player");
        assert_eq!(params[0], ("cursor", "cursor-1".to_string()));
        assert_eq!(params[1], ("count", "100".to_string()));
        assert_eq!(params[3], ("scene", "player".to_string()));
    }
}
