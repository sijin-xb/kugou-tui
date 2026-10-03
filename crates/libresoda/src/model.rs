//! 与上游 `music-lib/model` 对应的数据模型（只保留 soda 用到的部分）。

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// 所有音乐源通用的歌曲结构（对应 Go 的 `model.Song`）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Song {
    pub id: String,
    pub name: String,
    pub artist: String,
    pub album: String,
    /// 某些源特有，用于获取封面。
    pub album_id: String,
    /// 时长（秒）。
    pub duration: i64,
    /// 文件大小（字节）。
    pub size: i64,
    /// 码率（kbps）。
    pub bitrate: i64,
    /// 来源标识，汽水固定为 `soda`。
    pub source: String,
    /// 真实音频文件下载链接；加密流形如 `<url>#auth=<percent-encoded play_auth>`。
    pub url: String,
    /// 文件后缀（mp3 / m4a / flac ...）。
    pub ext: String,
    pub cover: String,
    /// 歌曲原始链接（网页地址）。
    pub link: String,
    /// 源特有元数据（`track_id`、`quality`、`is_vip` ...）。
    pub extra: BTreeMap<String, String>,
    /// 探测后标记歌曲是否无效。
    pub is_invalid: bool,
    /// 是否需要付费/VIP 权益才能完整播放或下载。
    pub is_vip: bool,
}

impl Song {
    pub fn extra_get(&self, key: &str) -> Option<&str> {
        self.extra.get(key).map(|value| value.as_str())
    }

    pub fn extra_set(&mut self, key: &str, value: impl Into<String>) {
        let value = value.into();
        if !value.trim().is_empty() {
            self.extra.insert(key.to_string(), value);
        }
    }
}

/// 所有音乐源通用的歌单/专辑结构（对应 Go 的 `model.Playlist`）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Playlist {
    pub id: String,
    pub name: String,
    pub cover: String,
    pub track_count: i64,
    pub play_count: i64,
    pub creator: String,
    pub description: String,
    pub source: String,
    pub link: String,
    pub extra: BTreeMap<String, String>,
}

impl Playlist {
    pub fn extra_get(&self, key: &str) -> Option<&str> {
        self.extra.get(key).map(|value| value.as_str())
    }
}

/// 歌单分类（对应 Go 的 `model.PlaylistCategory`）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PlaylistCategory {
    pub id: String,
    pub name: String,
    pub group: String,
    pub source: String,
    pub count: i64,
    pub hot: bool,
    pub extra: BTreeMap<String, String>,
}

/// 二维码登录状态（对应 Go 的 `model.QRLoginStatus*`）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QRLoginStatus {
    /// 等待扫码。
    #[default]
    Waiting,
    /// 已扫码，等待手机确认 / 需要短信验证。
    Scanned,
    /// 登录成功，服务端已下发会话 Cookie。
    Success,
    /// 二维码过期。
    Expired,
    /// 失败。
    Failed,
}

impl QRLoginStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            QRLoginStatus::Waiting => "waiting",
            QRLoginStatus::Scanned => "scanned",
            QRLoginStatus::Success => "success",
            QRLoginStatus::Expired => "expired",
            QRLoginStatus::Failed => "failed",
        }
    }
}

impl std::fmt::Display for QRLoginStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 二维码登录会话（对应 Go 的 `model.QRLoginSession`）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct QRLoginSession {
    /// 来源标识，汽水固定为 `soda`。
    pub source: String,
    /// 轮询用的 key / token。
    pub key: String,
    /// 登录二维码内容（通常是可直接渲染的 URL）。
    pub url: String,
    /// 服务端直接返回的二维码图片地址（可能是 base64 data URL）。
    pub image_url: String,
    /// 过期时间（Unix 秒）。
    pub expires_at: i64,
    /// 附加信息（token、scan_login_url、is_frontier 等）。
    pub extra: BTreeMap<String, String>,
}

/// 二维码登录轮询结果（对应 Go 的 `model.QRLoginResult`）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct QRLoginResult {
    /// 来源标识。
    pub source: String,
    /// 轮询 key。
    pub key: String,
    /// 当前状态。
    pub status: QRLoginStatus,
    /// 服务端返回的提示文案。
    pub message: String,
    /// 登录成功后回填的 Cookie（形如 `k=v; k2=v2`）。
    pub cookie: String,
    /// 登录成功后的 Cookie 集合。
    pub cookies: BTreeMap<String, String>,
    /// 附加信息（MFA、限流、API 状态等）。
    pub extra: BTreeMap<String, String>,
}

impl QRLoginResult {
    pub fn extra_set(&mut self, key: &str, value: impl Into<String>) {
        self.extra.insert(key.to_string(), value.into());
    }
}

/// 汽水实现里统一使用的来源标识。
pub const SOURCE_SODA: &str = "soda";
