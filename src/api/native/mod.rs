//! 纯 Rust 后端。当前只是骨架：所有方法都明确返回「尚未实现」。
//!
//! 实现顺序：底层先行，KAT 全绿之后才接网络——签名错一个字节就全部失败，
//! 而且不会有清楚的报错，混在网络调试里查不动。
//!
//! 平台参数由 [`SourceKind`] 携带：标准版与概念版（lite）的盐值、`appid`、
//! `clientver` 都不同，两个都要能跑。

// 阶段 2 的底层纯函数：签名 / 设备指纹 / KRC 解密。调用方是阶段 3 起接入的
// 网络层，在那之前只有各模块自己的单元测试引用它们，非测试构建会报 dead_code。
// 阶段 3 接完网络后删掉这些 `allow`。
#[allow(dead_code)]
pub mod crypto;
#[allow(dead_code)]
pub mod device;
#[allow(dead_code)]
pub mod krc;
#[allow(dead_code)]
pub mod sign;

use crate::api::cloud::{QrCheck, UserInfo, VipInfo};
use crate::api::catalog::StreamUrl;
use crate::api::model::{Artist, Lyric, Playlist, RankBoard, Song};
use crate::api::traits::MusicApi;
use crate::error::{AppError, Result};
use crate::source::SourceKind;
use serde_json::Value;

/// 内嵌的纯 Rust 后端。
#[derive(Debug, Clone)]
pub struct NativeApi {
    cookie: Option<String>,
    /// 供界面显示。native 没有服务地址，用平台名代替——标准版与概念版是两套
    /// `appid`/盐值，出问题时「现在到底在用哪一套」是第一个要确认的事。
    label: String,
}

impl NativeApi {
    pub fn new(kind: SourceKind, cookie: Option<String>) -> Self {
        Self {
            cookie: cookie.filter(|value| !value.trim().is_empty()),
            label: format!("native（{}，内嵌）", kind.label()),
        }
    }

    pub fn base(&self) -> &str {
        &self.label
    }

    pub fn cookie(&self) -> Option<&str> {
        self.cookie.as_deref()
    }

    pub fn set_cookie(&mut self, cookie: Option<String>) {
        self.cookie = cookie.filter(|value| !value.trim().is_empty());
    }
}

/// 生成一批「尚未实现」的方法体。
///
/// 阶段 1 的出口条件之一就是 `--api native` 必须**明确报错**而不是静默失败，
/// 所以这里不返回空列表、不返回默认值。
macro_rules! not_implemented {
    ($( async fn $name:ident ( &self $(, $arg:ident : $ty:ty)* ) -> $ret:ty ; )*) => {
        impl MusicApi for NativeApi {
            $(
                async fn $name(&self $(, $arg: $ty)*) -> $ret {
                    $( let _ = $arg; )*
                    Err(AppError::Other(format!(
                        "native 后端尚未实现：{}",
                        stringify!($name)
                    )))
                }
            )*
        }
    };
}

not_implemented! {
    async fn search_songs(&self, keywords: &str, page: u32, page_size: u32) -> Result<Vec<Song>>;
    async fn plaza_playlists(&self, category_id: i64, page: u32, page_size: u32) -> Result<Vec<Playlist>>;
    async fn playlist_tracks(&self, global_id: &str, page: u32, page_size: u32, fresh: bool) -> Result<Vec<Song>>;
    async fn user_playlists(&self) -> Result<Vec<Playlist>>;
    async fn user_playlist_tracks(&self, list_id: i64, page: u32, page_size: u32, fresh: bool) -> Result<Vec<Song>>;
    async fn artist_list(&self, kind: i64, hot_size: u32) -> Result<Vec<Artist>>;
    async fn rank_boards(&self) -> Result<Vec<RankBoard>>;
    async fn playlist_tracks_all(&self, global_id: &str, fresh: bool) -> Result<Vec<Song>>;
    async fn user_playlist_tracks_all(&self, list_id: i64, fresh: bool) -> Result<Vec<Song>>;
    async fn artist_tracks_all(&self, artist_id: i64, sort: &str) -> Result<Vec<Song>>;
    async fn rank_tracks_all(&self, rank_id: i64) -> Result<Vec<Song>>;
    async fn song_stream_url(&self, song: &Song, quality: &str) -> Result<StreamUrl>;
    async fn fetch_lyric(&self, song: &Song) -> Result<Lyric>;
    async fn login_qr_key(&self) -> Result<String>;
    async fn login_qr_create(&self, key: &str) -> Result<String>;
    async fn login_qr_check(&self, key: &str) -> Result<QrCheck>;
    async fn user_detail(&self) -> Result<UserInfo>;
    async fn user_vip_detail(&self) -> Result<VipInfo>;
    async fn claim_day_vip(&self, receive_day: &str) -> Result<Value>;
    async fn upgrade_day_vip(&self) -> Result<Value>;
    async fn claimed_vip_days(&self) -> Result<Vec<String>>;
    async fn fetch_device_fingerprint(&self) -> Result<String>;
    async fn add_tracks_to_playlist(&self, source: SourceKind, list_id: i64, songs: &[Song]) -> Result<usize>;
    async fn remove_tracks_from_playlist(&self, source: SourceKind, list_id: i64, songs: &[Song]) -> Result<usize>;
    async fn delete_playlist(&self, source: SourceKind, list_id: i64) -> Result<()>;
    async fn create_playlist(&self, source: SourceKind, name: &str) -> Result<Option<i64>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 阶段 1 的出口条件：native 必须明确报错，不能静默返回空。
    #[tokio::test]
    async fn placeholder_reports_not_implemented() {
        let api = NativeApi::new(SourceKind::KugouConcept, None);
        let error = api.search_songs("x", 1, 30).await.unwrap_err();
        let message = error.to_string();
        assert!(message.contains("尚未实现"), "实际：{message}");
        assert!(message.contains("search_songs"), "要指出是哪个方法：{message}");
    }

    /// 两套平台的盐值/appid 不同，构造时就必须带上平台。
    #[test]
    fn keeps_its_platform() {
        let standard = NativeApi::new(SourceKind::Kugou, None);
        let lite = NativeApi::new(SourceKind::KugouConcept, None);
        assert!(standard.base().contains("酷狗"));
        assert!(lite.base().contains("概念版"));
    }
}
