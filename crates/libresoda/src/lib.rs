//! # libresoda
//!
//! `libresoda` 是 [music-lib](https://github.com/guohuiyuan/music-lib) 中 `soda` 包
//! （汽水音乐 / Soda Music，抖音官方音乐 App）的 Rust 复刻实现，封装为一个库 crate。
//!
//! 能力概览：
//!
//! - 搜索：歌曲 / 专辑 / 歌单（Android 搜索接口）
//! - 详情：单曲（`pc/track_v2` + `h5/seo_track` 兜底）、专辑分享页、歌单（分页 + web 兜底）
//! - 歌词：汽水逐字歌词 → LRC
//! - 下载：音质/时长/码率择优，VIP 曲目自动改走 PC 接口，加密流本地解密
//! - 解密：`play_auth`（spade）密钥还原 + MP4/CENC 样本解密（AES-CTR）
//! - 账号：Cookie 登录态探测（`IsVipAccount` 语义）、我的歌单
//! - 链接解析：分享短链、分享页、各种 id 形态
//! - 整曲取流：`x-helios` / `x-medusa` 应用签名凭证接入（VIP 曲目不再只有试听），
//!   见 [`Soda::check_stream_access`] 与 `docs/FULL-QUALITY-STREAM.md`
//!
//! 自写客户端所需的写操作与推荐流（上游 `music-lib` 没有，本仓库新增）：
//!
//! - 推荐流 / 听歌模式：`Soda::feed_mode`、`Soda::discover_mix`、`Soda::fetch_discover`
//! - 我喜欢的音乐与收藏：`Soda::collect_media` / `collect_playlists` / `collect_albums`
//!   / `collect_artists`、`Soda::collected_items`、`Soda::collected_artists`
//! - 歌单管理：`Soda::create_playlist` / `create_playlist_with_media`、`append_playlist_media`、
//!   `sort_playlist_media`、`update_playlist_info`、`delete_playlists`、
//!   `append_playlist_tracks`、`import_playlist`、`playlist_name_has_sensitive_word`
//! - 歌单列表：`Soda::get_my_playlists`（我创建的）、`Soda::get_recommend_playlists`
//! - 音质档位：`Soda::set_quality_preference`（`best` / `lossless` / `highest` / `medium`…）
//! - 播放记录：`Soda::append_recently_played_media`、`Soda::recently_played_media`、
//!   `Soda::media_stats`
//! - 搜索联想：`Soda::suggest`、`Soda::suggest_words`
//! - 专辑 / 艺人：`Soda::fetch_pc_album_detail`、`Soda::fetch_artist_detail`、
//!   `Soda::list_artist_tracks`、`Soda::list_artist_albums`
//!
//! 每项能力的调用示例、接口路径与真机实测状态见 `docs/CLIENT-API.md`。
//!
//!
//! ## 快速开始
//!
//! ```no_run
//! use libresoda::Soda;
//!
//! let soda = Soda::new(""); // 不带 Cookie 时只能拿免费明文流
//! for song in soda.search("周杰伦")? {
//!     println!("{} - {}", song.name, song.artist);
//! }
//! # Ok::<(), libresoda::SodaError>(())
//! ```

pub mod error;
pub mod http;
pub mod model;
pub mod soda;
pub mod util;

pub use error::{Result, SodaError};
pub use model::{Playlist, PlaylistCategory, Song};
pub use soda::signature::AppCredentials;
pub use soda::stream::StreamAccessReport;
pub use soda::{soda, Soda};
