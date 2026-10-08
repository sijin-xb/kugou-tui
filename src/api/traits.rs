//! API 后端的抽象边界。
//!
//! 上层（[`crate::source`] 分派层与 `app`）只认这个 trait。两个实现：
//!
//! * [`crate::api::node::NodeApi`] —— 走本机 KuGouMusicApi（HTTP）；
//! * [`crate::api::native::NativeApi`] —— 内嵌纯 Rust，不依赖 Node。
//!
//! 用原生 `async fn in trait` 而不是 `async-trait`：省一条 proc-macro 依赖链。
//! 代价是当前写不了 `dyn MusicApi`（AFIT 不满足 dyn 兼容），将来真需要 trait
//! 对象时换回 `async-trait` 是机械改动——`Send` 现在由具体类型各自保证。
//!
//! # 方法集为什么是 26 个，而不是 `NodeApi` 上的 28 个
//!
//! `NodeApi` 挂着 28 个 `pub` 方法，其中两个**只服务于分页内部实现**，不属于
//! 「一个音源平台要提供的能力」：
//!
//! * `artist_tracks(artist_id, sort, page, page_size)` —— 歌手单曲的**单页**请求；
//! * `rank_tracks(rank_id, page, page_size)` —— 榜单歌曲的**单页**请求。
//!
//! 它们唯一的调用点是 `artist_tracks_all` / `rank_tracks_all` 内部的
//! `collect_all_pages` 闭包（`src/api/catalog.rs`），而对外承诺的语义是「取全」
//! ——`artist_tracks_all` / `rank_tracks_all` 这两个**在** trait 里。
//!
//! 分页是 Node 版为绕开上游 `pagesize` 硬上限 30 才需要的实现细节；native 可以
//! 一次拿全，没有理由被迫暴露同样的单页签名。**trait 收的是「音源能力」，
//! `NodeApi` 多出来的两个是「HTTP 实现细节」**——这不是漏对齐，是有意的边界。
//! 其余 26 个一一对应，且声明顺序完全一致。
//!
//! 四份清单（本文件 / `node.rs` 的 trait impl / `native` 的宏列表 /
//! `delegate_to_backend!`）**由编译器强制同步**：任一处漏一个方法，`ApiClient`
//! 就不满足 `MusicApi`，`src/source/mod.rs` 的调用点立刻编译失败。不要靠人眼比对。

use serde_json::Value;

use crate::api::catalog::StreamUrl;
use crate::api::cloud::{QrCheck, UserInfo, VipInfo};
use crate::api::model::{Artist, Lyric, Playlist, RankBoard, Song};
use crate::error::Result;
use crate::source::SourceKind;

/// 一个音源平台要实现的全部接口。
///
/// 方法集就是迁移范围：只包含 TUI 实际调用到的接口，见 `docs/NATIVE_API.md`。
#[allow(async_fn_in_trait)] // 见模块头：现在不做 dyn
pub trait MusicApi: Send + Sync {
    // ------------------------------------------------------------------
    // 目录：搜索 / 歌单 / 歌手 / 榜单
    // ------------------------------------------------------------------

    async fn search_songs(&self, keywords: &str, page: u32, page_size: u32) -> Result<Vec<Song>>;

    async fn plaza_playlists(
        &self,
        category_id: i64,
        page: u32,
        page_size: u32,
    ) -> Result<Vec<Playlist>>;

    async fn playlist_tracks(
        &self,
        global_id: &str,
        page: u32,
        page_size: u32,
        fresh: bool,
    ) -> Result<Vec<Song>>;

    async fn user_playlists(&self) -> Result<Vec<Playlist>>;

    async fn user_playlist_tracks(
        &self,
        list_id: i64,
        page: u32,
        page_size: u32,
        fresh: bool,
    ) -> Result<Vec<Song>>;

    async fn artist_list(&self, kind: i64, hot_size: u32) -> Result<Vec<Artist>>;

    async fn rank_boards(&self) -> Result<Vec<RankBoard>>;

    /// 歌单内全部歌曲（内部翻页）。
    async fn playlist_tracks_all(&self, global_id: &str, fresh: bool) -> Result<Vec<Song>>;

    /// 用户歌单内全部歌曲（内部翻页）。
    async fn user_playlist_tracks_all(&self, list_id: i64, fresh: bool) -> Result<Vec<Song>>;

    /// 歌手全部歌曲（内部翻页）。
    async fn artist_tracks_all(&self, artist_id: i64, sort: &str) -> Result<Vec<Song>>;

    /// 榜单全部歌曲（内部翻页）。
    async fn rank_tracks_all(&self, rank_id: i64) -> Result<Vec<Song>>;

    // ------------------------------------------------------------------
    // 播放直链
    // ------------------------------------------------------------------

    async fn song_stream_url(&self, song: &Song, quality: &str) -> Result<StreamUrl>;

    // ------------------------------------------------------------------
    // 歌词
    // ------------------------------------------------------------------

    async fn fetch_lyric(&self, song: &Song) -> Result<Lyric>;

    // ------------------------------------------------------------------
    // 登录 / 用户 / VIP
    // ------------------------------------------------------------------

    async fn login_qr_key(&self) -> Result<String>;

    async fn login_qr_create(&self, key: &str) -> Result<String>;

    async fn login_qr_check(&self, key: &str) -> Result<QrCheck>;

    async fn user_detail(&self) -> Result<UserInfo>;

    async fn user_vip_detail(&self) -> Result<VipInfo>;

    async fn claim_day_vip(&self, receive_day: &str) -> Result<Value>;

    async fn upgrade_day_vip(&self) -> Result<Value>;

    async fn claimed_vip_days(&self) -> Result<Vec<String>>;

    /// 设备指纹 `dfid`。native 后端由自己生成并持久化，不走网络。
    async fn fetch_device_fingerprint(&self) -> Result<String>;

    // ------------------------------------------------------------------
    // 云端歌单写操作
    // ------------------------------------------------------------------

    async fn add_tracks_to_playlist(
        &self,
        source: SourceKind,
        list_id: i64,
        songs: &[Song],
    ) -> Result<usize>;

    async fn remove_tracks_from_playlist(
        &self,
        source: SourceKind,
        list_id: i64,
        songs: &[Song],
    ) -> Result<usize>;

    async fn delete_playlist(&self, source: SourceKind, list_id: i64) -> Result<()>;

    async fn create_playlist(&self, source: SourceKind, name: &str) -> Result<Option<i64>>;
}
