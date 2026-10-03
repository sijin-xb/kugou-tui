//! 艺人详情与作品列表（上游 `music-lib` 没有；照官方客户端 IDL 实现）。
//!
//! | 能力 | 接口 |
//! | --- | --- |
//! | 艺人详情 | `GET /luna/pc/artists/{artist_id}` |
//! | 艺人单曲 | `GET /luna/pc/artists/{artist_id}/tracks` |
//! | 艺人专辑 | `GET /luna/pc/artists/{artist_id}/albums` |
//!
//! 回包结构随版本变动，统一返回原始 JSON（`serde_json::Value`）。

use super::Soda;
use crate::error::{Result, SodaError};

pub const ARTIST_PATH: &str = "/luna/pc/artists";

fn require_id(artist_id: &str, what: &str) -> Result<String> {
    let id = artist_id.trim();
    if id.is_empty() {
        return Err(SodaError::invalid_input(format!(
            "{what} requires artist_id"
        )));
    }
    Ok(id.to_string())
}

fn list_params(cursor: &str, count: i64) -> Vec<(&'static str, String)> {
    let count = if count <= 0 { 20 } else { count.min(100) };
    vec![
        ("cursor", cursor.trim().to_string()),
        ("count", count.to_string()),
    ]
}

/// 艺人详情。
pub fn fetch_artist_detail(soda: &Soda, artist_id: &str) -> Result<serde_json::Value> {
    let id = require_id(artist_id, "soda artist detail")?;
    super::pc_get_json(soda, &format!("{ARTIST_PATH}/{id}"), &[])
}

/// 艺人单曲列表。
pub fn list_artist_tracks(
    soda: &Soda,
    artist_id: &str,
    cursor: &str,
    count: i64,
) -> Result<serde_json::Value> {
    let id = require_id(artist_id, "soda artist tracks")?;
    super::pc_get_json(
        soda,
        &format!("{ARTIST_PATH}/{id}/tracks"),
        &list_params(cursor, count),
    )
}

/// 艺人专辑列表。
pub fn list_artist_albums(
    soda: &Soda,
    artist_id: &str,
    cursor: &str,
    count: i64,
) -> Result<serde_json::Value> {
    let id = require_id(artist_id, "soda artist albums")?;
    super::pc_get_json(
        soda,
        &format!("{ARTIST_PATH}/{id}/albums"),
        &list_params(cursor, count),
    )
}

impl Soda {
    /// 艺人详情。
    pub fn fetch_artist_detail(&self, artist_id: &str) -> Result<serde_json::Value> {
        fetch_artist_detail(self, artist_id)
    }

    /// 艺人单曲列表。
    pub fn list_artist_tracks(
        &self,
        artist_id: &str,
        cursor: &str,
        count: i64,
    ) -> Result<serde_json::Value> {
        list_artist_tracks(self, artist_id, cursor, count)
    }

    /// 艺人专辑列表。
    pub fn list_artist_albums(
        &self,
        artist_id: &str,
        cursor: &str,
        count: i64,
    ) -> Result<serde_json::Value> {
        list_artist_albums(self, artist_id, cursor, count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_params_clamp_count() {
        assert_eq!(list_params("", 0)[1], ("count", "20".to_string()));
        assert_eq!(list_params("c1", 500)[1], ("count", "100".to_string()));
        assert_eq!(list_params("c1", 5)[0], ("cursor", "c1".to_string()));
    }

    #[test]
    fn require_id_rejects_blank() {
        assert!(require_id("  ", "x").is_err());
        assert_eq!(require_id(" 42 ", "x").unwrap(), "42");
    }
}
