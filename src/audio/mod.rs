//! 音频子系统。
//!
//! 几个模块各管一件事：
//!
//! * [`cache`] —— 磁盘缓存与容量回收，决定「文件放在哪、满了删谁」；
//! * [`download`] —— 把直链下载成缓存文件，决定「怎么拿数据」；
//! * [`downmix`] —— 多声道 → 立体声下混，决定「多声道文件怎么出声」；
//! * [`resample`] —— 抗混叠重采样，决定「hi-res 母版降设备率怎么不失真」；
//! * [`engine`] —— 独占音频线程的播放引擎，决定「怎么出声」。
//!
//! 它们之间没有直接依赖：主线程负责编排
//!
//! ```text
//! song_stream_url()  ──▶  cache.find(key)  命中 ──▶ engine.load(path)
//!                            │
//!                          未命中
//!                            ▼
//!                   download.fetch_to(url, cache.path_for(key, ext))
//!                            │
//!                            ▼
//!                   cache.enforce_limit()  ──▶  engine.load(path)
//! ```
//!
//! 这样拆分的好处是每一环都能单独测试：缓存回收不用真的播放，
//! 下载不用真的出声，播放引擎不用真的联网。

pub mod cache;
pub mod download;
pub mod downmix;
pub mod engine;
pub mod levels;
pub mod resample;
pub mod spectrum;
pub mod streaming;

pub use cache::AudioCache;
pub use download::Downloader;
pub use engine::{AudioHandle, list_output_devices};
