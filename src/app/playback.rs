//! 播放控制：起播、切歌、进度、音量、静音、歌词偏移。
//!
//! 从 `update.rs` 里切出来的第三块（见 `docs/MAINTENANCE.md` §1.8）。这里的代码
//! **只改「正在播什么、播到哪、多大声」**；需要网络时只负责把任务 `spawn` 出去，
//! 结果一律走 `Loaded` 事件回来由 `update.rs` 处理。
//!
//! ## 边界：原来那一节里还挤着五个不属于播放的方法
//!
//! 它们**留在 `update.rs`**，各有理由——所以 `playback.rs` 比「播放控制」那一节小，
//! 这是有意的：
//!
//! | 留在原处 | 为什么不算播放 |
//! |---|---|
//! | `sync_mpris` / `sync_tray` | 输出适配器，由 `tick()` 每拍调一次把状态推给 D-Bus 组件；调用方在 `update.rs` |
//! | `toggle_window` | 走 niri 的 compositor IPC，与音频无关 |
//! | `client_for` | 全文件共用的客户端构造辅助（6 处调用），搬走只会让调用方到处写 `pub(super)` |
//! | `request_lyric` | 「取歌词」这个网络请求；它的结果处理在 `handle_loaded` 里，请求与处理放同一个文件才省上下文 |
//!
//! 宁可这一块小一点、边界干净，也不要为了凑体积把邻居一起搬进来。
//!
//! ## 三条容易踩的坑
//!
//! * **切歌必须先 `cancel()` 上一条流**（`start_playback` 开头）。被丢下的下载任务
//!   会继续往自己的缓冲里堆数据，谁也不回收——这是这个项目里最贵的一类泄漏。
//! * **音源跟着歌走，不跟当前音源走**。队列可以跨音源，而 hash 只在该平台的接口里
//!   有意义，所以一律用 `song.source` 取客户端。
//! * **停住不等于清空位置**。`stop_playback` 不动 `position_ms`，用户按 Space 才能
//!   从停住的地方续上，而不是从头。

use crate::api::model::Song;
use crate::app::App;
use crate::app::state::CoverArt;
use crate::app::update::describe_song;
use crate::audio::engine::{AudioSource, PlaybackState};
use crate::event::Loaded;
use crate::logger::tlog;

/// 「上一首」在播放超过这个时长后，先回到本曲开头而不是切歌。
///
/// 跟播放强相关，所以跟着播放控制一起从 `update.rs` 搬过来了。
const RESTART_THRESHOLD_MS: u64 = 3_000;

impl App {
    // 播放控制
    // ==================================================================

    /// 用一批歌曲替换播放队列并起播第 `index` 首。
    pub fn play_from(&mut self, songs: Vec<Song>, index: usize) {
        let Some(song) = self.state.queue.replace_with(songs, index).cloned() else {
            self.state.warn("列表为空，无法播放");
            return;
        };
        self.sync_queue_cursor(index);
        self.start_playback(song, 0);
    }

    pub(super) fn play_from_focused_songs(&mut self) {
        let selection = self.state.focused_songs().and_then(|songs| {
            songs
                .selected_index()
                .map(|index| (songs.songs.clone(), index))
        });

        match selection {
            Some((songs, index)) => self.play_from(songs, index),
            None => self.state.warn("当前没有可播放的歌曲"),
        }
    }

    pub(super) fn play_from_queue(&mut self) {
        let Some(index) = self.state.queue_cursor.selected() else {
            self.state.warn("播放队列为空");
            return;
        };
        let Some(song) = self.state.queue.jump_to(index).cloned() else {
            self.state.warn("播放队列为空");
            return;
        };
        self.start_playback(song, 0);
    }

    pub(super) fn toggle_playback(&mut self) {
        match self.state.playback {
            // 正在缓冲时忽略，避免连按导致状态错乱
            PlaybackState::Loading => {}
            PlaybackState::Playing | PlaybackState::Paused => self.audio.toggle(),
            PlaybackState::Stopped => {
                if let Some(song) = self.state.queue.current().cloned() {
                    // 会话恢复的那首：从上次的位置续播，而不是从头。
                    // hash 对不上就说明用户换了歌，这个位置作废。
                    let resume = match self.state.resume.take() {
                        Some((hash, ms)) if hash == song.hash => ms,
                        _ => 0,
                    };
                    if resume > 0 {
                        self.state.info(format!(
                            "接着上次播：{}",
                            crate::api::model::format_duration_ms(resume)
                        ));
                    }
                    self.start_playback(song, resume);
                } else {
                    self.play_from_focused_songs();
                }
            }
        }
    }

    pub(super) fn next_track(&mut self, triggered_by_user: bool) {
        let Some(index) = self.state.queue.advance(triggered_by_user) else {
            // 顺序播放到底：停止而不是循环
            self.audio.stop();
            self.state.playback = PlaybackState::Stopped;
            self.state.position_ms = 0;
            self.state.info("播放队列已到末尾");
            return;
        };
        self.sync_queue_cursor(index);
        if let Some(song) = self.state.queue.current().cloned() {
            self.start_playback(song, 0);
        }
    }

    pub(super) fn previous_track(&mut self) {
        // 播放超过 3 秒时先回到本曲开头，这是主流播放器的通用行为
        if self.state.position_ms > RESTART_THRESHOLD_MS {
            self.audio.seek_to(0);
            self.state.position_ms = 0;
            return;
        }

        let Some(index) = self.state.queue.retreat() else {
            self.state.warn("播放队列为空");
            return;
        };
        self.sync_queue_cursor(index);
        if let Some(song) = self.state.queue.current().cloned() {
            self.start_playback(song, 0);
        }
    }
    /// 绝对定位到 `position_ms`。
    ///
    /// 与 [`Self::seek_by`] 的区别：那个是相对步进，这个是"跳到某处"。
    /// MPRIS 的 SetPosition（桌面组件拖进度条）需要后者。
    pub(super) fn seek_to(&mut self, position_ms: u64) {
        if self.state.current.is_none() {
            return;
        }
        // 夹在时长范围内，避免拖到尽头后位置越界
        let target = if self.state.duration_ms > 0 {
            position_ms.min(self.state.duration_ms.saturating_sub(1))
        } else {
            position_ms
        };

        self.audio.seek_to(target);
        // 乐观更新，让进度条立刻响应；音频线程随后给出真实位置
        self.state.position_ms = target;
    }

    pub(super) fn seek_by(&mut self, delta_ms: i64) {
        if self.state.current.is_none() {
            return;
        }
        self.audio.seek_by(delta_ms);
        // 乐观更新，让进度条立刻响应；音频线程随后会给出真实位置
        let next = (self.state.position_ms as i64 + delta_ms).max(0) as u64;
        self.state.position_ms = match self.state.duration_ms {
            0 => next,
            duration => next.min(duration),
        };
    }

    pub(super) fn adjust_volume(&mut self, delta: f32) {
        // 静音状态下按音量键，先解除静音再调整
        let base = self
            .state
            .volume_before_mute
            .take()
            .unwrap_or(self.state.volume);
        let next = (base + delta).clamp(0.0, 1.0);

        self.state.volume = next;
        self.audio.set_volume(next);
        self.state.info(format!("音量 {:.0}%", next * 100.0));
    }
    pub(super) fn toggle_mute(&mut self) {
        match self.state.volume_before_mute.take() {
            Some(previous) => {
                self.state.volume = previous;
                self.audio.set_volume(previous);
                self.state
                    .info(format!("取消静音，音量 {:.0}%", previous * 100.0));
            }
            None => {
                self.state.volume_before_mute = Some(self.state.volume);
                self.state.volume = 0.0;
                self.audio.set_volume(0.0);
                self.state.info("已静音");
            }
        }
    }

    pub(super) fn adjust_lyric_offset(&mut self, delta_ms: i64) {
        let offset = (self.state.config.lyric_offset_ms + delta_ms).clamp(-10_000, 10_000);
        self.state.config.lyric_offset_ms = offset;
        self.state.info(format!("歌词偏移 {offset:+} ms"));
    }

    /// 把「当前这首听到哪儿了」记进 `state.resume`，供非正常收场之后续播。
    ///
    /// 断流、下载失败都会让歌停下来，但听到的位置是有效的：用户按播放
    /// （Space）就该从这儿接着听，而不是从头。`state.resume` 本来就只对
    /// hash 相同的曲目生效、换歌即作废，语义正好，不必再造一套。
    pub(super) fn mark_resume_point(&mut self) {
        let Some(song) = self.state.current.as_ref() else {
            return;
        };
        if self.state.position_ms == 0 {
            return;
        }
        self.state.resume = Some((song.hash.clone(), self.state.position_ms));
    }

    /// 停止播放，并把还在下的那条流一起叫停。
    ///
    /// 光调 `audio.stop()` 会漏掉流式下载：音频线程停了，后台任务还在把整首
    /// 往缓冲和 `.part` 文件里灌，谁也不回收它。
    pub(super) fn stop_playback(&mut self) {
        if let Some(stream) = self.active_stream.take() {
            stream.cancel();
        }
        self.stream_retried = None;
        self.audio.stop();
    }

    /// 起播一首歌：先查缓存，未命中则解析直链 → 下载 → 播放。
    pub(super) fn start_playback(&mut self, song: Song, start_at_ms: u64) {
        // 上一首如果还在边下边播，通知后台任务收工：任务退出、内存窗口和
        // 文件句柄一起释放。频繁切歌时这一步很关键——否则每个被丢下的下载
        // 任务都会继续把数据往它的缓冲里堆，谁也回收不了。
        if let Some(stream) = self.active_stream.take() {
            stream.cancel();
        }
        // 「已自动兜过一次」的记号按曲目算：换了歌就清掉，同一次断流不重复兜。
        if self.stream_retried.as_deref() != Some(song.hash.as_str()) {
            self.stream_retried = None;
        }

        self.state.current = Some(song.clone());
        self.state.duration_ms = song.duration_ms;
        self.state.position_ms = start_at_ms;
        self.state.download_progress = None;
        self.state.lyric = crate::app::state::LyricPane {
            hash: Some(song.hash.clone()),
            ..Default::default()
        };
        // 切歌后先把旧封面清掉，否则会短暂显示上一张
        if !self.state.cover.belongs_to(&song.hash) {
            self.state.cover = CoverArt::default();
        }
        self.load_cover(&song);

        self.audio.mark_loading();
        self.state.playback = PlaybackState::Loading;

        // privilege 不含可播标记时提前提示，省得用户对着「缓冲中」干等
        if song.looks_playable() {
            self.state
                .info(format!("正在解析播放地址：{}", describe_song(&song)));
        } else {
            self.state.warn(format!(
                "《{}》可能受版权限制（VIP/已下架），若取链失败请换一首",
                song.name
            ));
        }

        self.request_lyric(song.clone());
        self.request_stream(song, start_at_ms);
    }

    fn request_stream(&mut self, song: Song, start_at_ms: u64) {
        let key = song.cache_key(&self.state.config.quality);

        // 缓存命中直接播，零网络开销
        if let Some(path) = self.cache.find(&key) {
            tlog!(crate::logger::LEVEL_DEBUG, "缓存命中：{}", path.display());
            self.audio
                .load(AudioSource::File(path), start_at_ms, song.duration_ms);
            return;
        }

        // 用**这首歌自己的来源**，不是当前音源：队列可以跨音源，
        // 切过音源之后队列里的旧歌仍要能播。
        let source = song.source;
        let api = match self.client_for(source) {
            Ok(client) => client,
            Err(error) => {
                self.state
                    .error(format!("无法连接「{}」：{error}", source.label()));
                return;
            }
        };
        let bus = self.bus.clone();
        let quality = self.state.config.quality.clone();

        self.runtime.spawn(async move {
            match source.song_stream_url(&api, &song, &quality).await {
                Ok(stream) => bus.emit(Loaded::StreamReady {
                    song: Box::new(song),
                    url: stream.url,
                    start_at_ms,
                    is_trial: stream.is_trial,
                    reason: stream.reason,
                }),
                Err(error) => bus.fail(format!("获取《{}》的播放地址失败", song.name), error),
            }
        });
    }
}
