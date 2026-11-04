//! 播放控制：起播、切歌、进度、音量、静音、歌词偏移。
//!
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
use crate::app::state::{CoverArt, SleepTimer};
use crate::app::update::describe_song;
use crate::audio::engine::{AudioSource, PlaybackState};
use crate::event::Loaded;
use crate::logger::tlog;
use std::time::Duration;

/// 「上一首」在播放超过这个时长后，先回到本曲开头而不是切歌。
///
/// 跟播放强相关，所以跟着播放控制一起从 `update.rs` 搬过来了。
const RESTART_THRESHOLD_MS: u64 = 3_000;

impl App {
    // 播放控制
    // ==================================================================

    /// 用一批歌曲替换播放队列并起播第 `index` 首。
    pub fn play_from(&mut self, songs: Vec<Song>, index: usize) {
        self.reset_auto_skip();
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
        self.reset_auto_skip();
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
        self.reset_auto_skip();
        match self.state.playback {
            // 正在缓冲时忽略，避免连按导致状态错乱
            PlaybackState::Loading => {}
            PlaybackState::Playing | PlaybackState::Paused => self.audio.toggle(),
            PlaybackState::Stopped => {
                // 断流等着重试：断点优先于队列与会话恢复的位置。
                //
                // 放在这里而不是塞进 `state.resume`，是因为 resume 的语义是「上次退出
                // 时听到哪」，两者混用会让一次断流位置被当成会话进度写进配置。
                if let Some((song, position_ms)) = self.pending_stream_retry.take() {
                    // 手动重试时把自动兜底的额度还回去：这次是用户主动发起的，
                    // 再断一次理应还能自动续一次（否则额度一旦用尽就永远不再兜）。
                    self.stream_retried = None;
                    self.state.info(format!(
                        "从 {} 继续《{}》",
                        crate::api::model::format_duration_ms(position_ms),
                        song.name
                    ));
                    self.start_playback(song, position_ms);
                    return;
                }
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
        self.reset_auto_skip();
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
        self.reset_auto_skip();
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

        // **停止态**（引擎里还没装载曲目）的 seek 是空操作：只改乐观位置的话，
        // 进度条跳过去了、Space 却从 0 开始——停止态下点歌词跳转「怎么点都没反应」
        // 就是这条。把目标记成续播点，Space 按下去从点击处接着放。
        if self.state.playback == PlaybackState::Stopped
            && let Some(song) = self.state.current.as_ref()
        {
            self.state.resume = Some((song.hash.clone(), target));
        }

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

    /// 把「随便听听」抓回来的歌洗牌、装进队列并开播第一首。
    ///
    /// 拿到空列表只提示不动作：队列里可能还有用户自己排的歌，宁可不改。
    pub(super) fn apply_random_queue(&mut self, title: &str, mut songs: Vec<Song>) {
        if songs.is_empty() {
            self.state
                .warn("随便听听没拿到歌（当前音源可能没有歌单广场），稍后再试");
            return;
        }
        crate::app::update::shuffle(&mut songs);
        let count = songs.len();
        let first = songs[0].clone();

        let mode = self.state.queue.mode();
        if self.state.queue.replace_with(songs, 0).is_none() {
            self.state.warn("随便听听没拿到歌，稍后再试");
            return;
        }
        // replace_with 会重置模式，把它装回去
        self.state.queue.set_mode(mode);
        self.state.current = self.state.queue.current().cloned();
        self.state.duration_ms = self
            .state
            .current
            .as_ref()
            .map(|song| song.duration_ms)
            .unwrap_or(0);
        // 先起播再报歌单名：start_playback 自己会写「正在解析播放地址」，
        // 后写的这条才留得住——歌单来源比解析过程更值得留在状态栏。
        self.start_playback(first, 0);
        self.state.info(format!("随便听听：{title} · {count} 首"));
    }

    /// 睡眠定时：在 关 → 15 → 30 → 60 → 120 分钟 之间循环。
    ///
    /// 切档按**档位下标**走（渐弱尾段剩余不足一分钟，从剩余时长反推会算错档），
    /// 所以从活动状态切档永远是「下一档」，不存在跳变。
    pub(super) fn cycle_sleep_timer(&mut self) {
        let next = match self.state.sleep_timer {
            None => Some(SleepTimer {
                remaining: Duration::from_secs(SLEEP_STEPS_MIN[0] * 60),
                step: 0,
            }),
            Some(current) => {
                let step = current.step + 1;
                SLEEP_STEPS_MIN.get(step).map(|&minutes| SleepTimer {
                    remaining: Duration::from_secs(minutes * 60),
                    step,
                })
            }
        };

        match next {
            Some(timer) => {
                self.state.sleep_timer = Some(timer);
                self.state
                    .info(format!("睡眠定时：{} 分钟", SLEEP_STEPS_MIN[timer.step]));
            }
            None => {
                self.state.sleep_timer = None;
                // 取消时把可能还在渐弱的音量还原
                self.audio.set_volume(self.state.volume);
                self.state.info("睡眠定时已取消");
            }
        }
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
        self.reset_auto_skip();
        if let Some(stream) = self.active_stream.take() {
            stream.cancel();
        }
        self.stream_retried = None;
        // 用户主动停了，之前那个「等你按 Space」的断点也就作废了：再按 Space 应当
        // 从曲目开头起播，而不是跳到一个几分钟前断掉的位置。
        self.pending_stream_retry = None;
        self.audio.stop();
    }

    /// 取链/装载还没成功就失败时，把「缓冲中」收回来。
    ///
    /// `start_playback` 先 `mark_loading()` 把状态设成 `Loading`，但引擎在
    /// 「没装载任何东西」时不会去改它（`sync()` 见 `!loaded` 直接返回）。取链
    /// 失败时如果不管，状态就永远停在 `Loading`——而 `toggle_playback` 恰恰
    /// 忽略 `Loading`，用户按 Space 毫无反应，看起来就是卡死。显式停一下让引擎
    /// 把状态落回 `Stopped`。
    pub(super) fn abort_loading(&mut self) {
        self.audio.stop();
        self.state.playback = PlaybackState::Stopped;
        self.state.busy = None;
        self.state.download_progress = None;
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
        // 起播了别的歌，之前那个断点就不该再生效——否则换歌之后按 Space 会跳回
        // 上一首断掉的位置。
        if self
            .pending_stream_retry
            .as_ref()
            .is_some_and(|(pending, _)| pending.hash != song.hash)
        {
            self.pending_stream_retry = None;
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
                self.handle_stream_failed(song, error);
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
                Err(error) => bus.emit(Loaded::StreamFailed {
                    song: Box::new(song),
                    error,
                }),
            }
        });
    }
}

/// 睡眠定时的档位表（分钟）。`z` 在 关 → 各档 → 关 之间循环。
pub(super) const SLEEP_STEPS_MIN: [u64; 4] = [15, 30, 60, 120];

/// 睡眠定时尾段的渐弱时长。剩余不足这个量时音量按比例往下压。
pub(super) const SLEEP_FADE: Duration = Duration::from_secs(15);

/// 睡眠定时倒计时：只在播放中走表（暂停即冻结），返回 true 表示**这一次**到点。
///
/// 到点时把剩余清零，由调用方把整个定时消费掉并暂停播放——若留着零值不清，
/// 下一拍又会报告一次「到点」，播放/暂停就会来回抖。
pub(super) fn advance_sleep_timer(
    timer: &mut SleepTimer,
    elapsed: Duration,
    playing: bool,
) -> bool {
    if !playing || timer.remaining.is_zero() {
        return false;
    }
    match timer.remaining.checked_sub(elapsed) {
        Some(rest) if !rest.is_zero() => {
            timer.remaining = rest;
            false
        }
        _ => {
            timer.remaining = Duration::ZERO;
            true
        }
    }
}

/// 渐弱期的有效音量：剩余越少声音越小，到点正好归零。剩余充足时原样返回。
pub(super) fn sleep_fade_volume(volume: f32, remaining: Duration) -> f32 {
    volume * (remaining.as_secs_f32() / SLEEP_FADE.as_secs_f32()).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::state::SleepTimer;

    /// 倒计时只在播放中走表：暂停冻结、恢复续走、到点报告一次。
    #[test]
    fn sleep_timer_only_counts_down_while_playing() {
        let mut timer = SleepTimer {
            remaining: Duration::from_secs(60),
            step: 0,
        };
        assert!(!advance_sleep_timer(
            &mut timer,
            Duration::from_secs(10),
            false
        ));
        assert_eq!(timer.remaining, Duration::from_secs(60), "暂停时不走表");

        assert!(!advance_sleep_timer(
            &mut timer,
            Duration::from_secs(10),
            true
        ));
        assert_eq!(timer.remaining, Duration::from_secs(50));

        assert!(advance_sleep_timer(
            &mut timer,
            Duration::from_secs(60),
            true
        ));
        assert_eq!(timer.remaining, Duration::ZERO, "到点清零");
        // 到点之后不能再重复报
        assert!(!advance_sleep_timer(
            &mut timer,
            Duration::from_secs(1),
            true
        ));
    }

    /// 渐弱只作用于尾段：剩余充足时音量原样，尾段线性压低，到点归零。
    #[test]
    fn fade_applies_only_in_the_tail() {
        let volume = 0.8;
        assert_eq!(sleep_fade_volume(volume, Duration::from_secs(60)), volume);
        let half = sleep_fade_volume(volume, Duration::from_secs_f32(7.5));
        assert!((half - 0.4).abs() < 1e-6, "剩余一半时音量减半，实际 {half}");
        assert_eq!(sleep_fade_volume(volume, Duration::ZERO), 0.0);
    }

    /// 「随便听听」的队列替换：旧队列被整体换掉、第一首起播、模式保留；
    /// 空列表不动队列只提示。
    #[test]
    fn random_queue_replaces_the_queue_and_starts_the_first_song() {
        let mut app = App::for_test();
        app.state.queue.append(crate::api::model::Song {
            name: "旧歌".to_string(),
            ..Default::default()
        });
        app.state
            .queue
            .set_mode(crate::app::queue::PlaybackMode::RepeatOne);

        let songs = vec![
            crate::api::model::Song {
                name: "A".to_string(),
                duration_ms: 100_000,
                ..Default::default()
            },
            crate::api::model::Song {
                name: "B".to_string(),
                duration_ms: 200_000,
                ..Default::default()
            },
        ];
        app.apply_random_queue("华语热歌", songs);

        assert_eq!(app.state.queue.len(), 2, "队列被替换");
        assert!(
            app.state.status.contains("华语热歌"),
            "{}",
            app.state.status
        );
        assert!(
            app.state.current.is_some(),
            "第一首应已就位（replace_with 定位到游标）"
        );
        let current_duration = app
            .state
            .current
            .as_ref()
            .map(|song| song.duration_ms)
            .unwrap_or(0);
        assert_eq!(app.state.duration_ms, current_duration, "时长跟 current 走");
        assert_eq!(
            app.state.queue.mode(),
            crate::app::queue::PlaybackMode::RepeatOne,
            "播放模式不应被替换冲掉"
        );
        assert_eq!(
            app.state.playback,
            crate::audio::engine::PlaybackState::Loading,
            "起播走了取链，进入缓冲态"
        );

        // 空列表：提示，队列原样
        let mut app = App::for_test();
        app.state.queue.append(crate::api::model::Song::default());
        app.apply_random_queue("空", Vec::new());
        assert_eq!(app.state.queue.len(), 1, "空列表不该动队列");
        assert!(app.state.status.contains("没拿到歌"));
    }

    /// 档位循环：关 → 15 → 30 → 60 → 120 → 关。切档按档位下标走，
    /// 渐弱尾段剩余不足一分钟也照样推进到正确的下一档。
    #[test]
    fn sleep_timer_cycles_through_the_steps() {
        let mut app = App::for_test();
        assert_eq!(app.state.sleep_timer, None);

        app.cycle_sleep_timer();
        assert_eq!(
            app.state.sleep_timer.map(|timer| timer.remaining),
            Some(Duration::from_secs(15 * 60)),
            "关 → 15 分钟"
        );

        // 走掉一段时间后按下一档：剩余 25 分钟说明当前在「30 分钟」档
        // （step 1），推到下一档 60，而不是从剩余时间重新起算
        app.state.sleep_timer = Some(SleepTimer {
            remaining: Duration::from_secs(25 * 60),
            step: 1,
        });
        app.cycle_sleep_timer();
        assert_eq!(
            app.state.sleep_timer.map(|timer| timer.remaining),
            Some(Duration::from_secs(60 * 60))
        );

        // 120 档（渐弱尾段只剩 3 秒）再按，循环回关，并把渐弱中的音量还原
        app.state.sleep_timer = Some(SleepTimer {
            remaining: Duration::from_secs(3),
            step: 3,
        });
        app.state.volume = 0.7;
        app.cycle_sleep_timer();
        assert_eq!(app.state.sleep_timer, None);
    }
}
