//! 播放电平采集。
//!
//! # 为什么是真的读采样
//!
//! 侧边栏那个小跳动条的数据来自这里。做法是把音频流包一层，采样原样透传给播放器，
//! 顺带把峰值记下来——所以它是**真的跟着音乐走**的：静音会掉到底，鼓点会顶到头。
//!
//! 拿随机数画动画当然更省事，但那样做出来的是装饰品，和音量、和音乐都没关系，
//! 属于自欺欺人。这里不这么做。

use std::num::NonZero;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::time::Duration;

use rodio::Source;
use rodio::source::SeekError;

use crate::audio::spectrum::WINDOW_SIZE;

/// 保留多少个电平格子。侧边栏宽度有限，28 格刚好铺满一行。
pub const LEVEL_BUCKETS: usize = 28;

/// 每 N 个采样聚合成一个格子。
///
/// 44100Hz 立体声下约 15ms 一格：视觉上跟手，又不会被单个采样的抖动带偏。
const SAMPLES_PER_BUCKET: usize = 1300;

/// 供频谱分析用的原始采样窗口。
///
/// 为什么和电平格分开存：侧边栏的小条只要 28 个音量格就够，但频谱要做 FFT，
/// 必须拿到**连续的**一段波形。两种数据用途不同，混在一起两边都别扭。
///
/// 同样是环形缓冲 + 原子量：音频线程每个采样写一次，主线程每帧读一次，
/// 谁也不会被对方卡住。f32 没有原子版本，所以存它的位模式。
#[derive(Debug, Clone)]
pub struct SampleWindow {
    samples: Arc<Vec<AtomicU32>>,
    cursor: Arc<AtomicUsize>,
}

impl Default for SampleWindow {
    fn default() -> Self {
        Self::new()
    }
}

impl SampleWindow {
    pub fn new() -> Self {
        let mut samples = Vec::with_capacity(WINDOW_SIZE);
        samples.resize_with(WINDOW_SIZE, || AtomicU32::new(0.0f32.to_bits()));
        Self {
            samples: Arc::new(samples),
            cursor: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// 追加一个（单声道）采样。
    pub fn push(&self, sample: f32) {
        let index = self.cursor.fetch_add(1, Ordering::Relaxed) % WINDOW_SIZE;
        self.samples[index].store(sample.to_bits(), Ordering::Relaxed);
    }

    /// 当前写入位置（自建窗以来累计推入的样本数）。
    ///
    /// cava 要的是「这一帧新到的样本」，不是「最近 N 个样本」——每帧重取固定长度
    /// 的窗口会让相邻两帧重叠，FFT 结果被同一段波形反复加权，柱子跟不上鼓点。
    /// 调用方保存上次的返回值，下次只取 `cursor - last` 个。
    pub fn cursor(&self) -> usize {
        self.cursor.load(Ordering::Relaxed)
    }

    /// 取出 `cursor - since` 个新样本（最多 `max` 个，超出时只保留最新的 `max` 个）。
    ///
    /// 返回空表示自 `since` 以来没有新样本（暂停 / 缓冲中），调用方应当跳过这一帧
    /// 而不是喂空数据——喂空会让 cava 的帧率估算与平滑系数跑偏。
    pub fn since(&self, since: usize, max: usize) -> Vec<f32> {
        let cursor = self.cursor.load(Ordering::Relaxed);
        let available = cursor.saturating_sub(since).min(WINDOW_SIZE);
        if available == 0 || max == 0 {
            return Vec::new();
        }
        let count = available.min(max);
        // 只取最新的 count 个：环里更旧的那些已经被后续推进覆盖过位置，
        // 继续往里读会拿到不连续的数据。
        let start = cursor - count;
        (start..cursor)
            .map(|position| {
                let index = position % WINDOW_SIZE;
                f32::from_bits(self.samples[index].load(Ordering::Relaxed))
            })
            .collect()
    }

    /// 清零。停止播放时调用，否则会留着上首歌的波形不动。
    pub fn clear(&self) {
        for sample in self.samples.iter() {
            sample.store(0.0f32.to_bits(), Ordering::Relaxed);
        }
    }
}

/// 播放电平：环形缓冲，存最近若干格子的峰值（0..=1000 的整数，避免浮点原子量）。
///
/// 用原子量而不是互斥锁：音频线程每隔一小段就写一次，主线程每帧读一次，
/// 无锁能保证任何一方都不会被对方卡住。
#[derive(Debug, Clone)]
pub struct AudioLevels {
    buckets: Arc<Vec<AtomicU32>>,
    cursor: Arc<AtomicUsize>,
    /// 频谱分析用的原始波形。与电平格共享同一份 Arc，两边看到的是同一段声音。
    window: Arc<SampleWindow>,
    /// 当前音源的采样率，由 [`LevelMeter`] 建好时写入。FFT 要靠它把
    /// bin 序号换算成频率，没有它就没法做对数分频。
    sample_rate: Arc<AtomicU32>,
}

impl Default for AudioLevels {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioLevels {
    pub fn new() -> Self {
        let mut buckets = Vec::with_capacity(LEVEL_BUCKETS);
        buckets.resize_with(LEVEL_BUCKETS, || AtomicU32::new(0));
        Self {
            buckets: Arc::new(buckets),
            cursor: Arc::new(AtomicUsize::new(0)),
            window: Arc::new(SampleWindow::new()),
            sample_rate: Arc::new(AtomicU32::new(0)),
        }
    }

    /// 写入一格峰值（入参 0.0 ~ 1.0）。
    fn push(&self, level: f32) {
        let value = (level.clamp(0.0, 1.0) * 1000.0) as u32;
        let index = self.cursor.fetch_add(1, Ordering::Relaxed) % LEVEL_BUCKETS;
        self.buckets[index].store(value, Ordering::Relaxed);
    }

    /// 追加一个（单声道）原始采样，供频谱分析用。
    fn push_sample(&self, sample: f32) {
        self.window.push(sample);
    }

    /// 记录采样率。换歌时会变（不同来源的文件采样率未必相同）。
    fn set_sample_rate(&self, sample_rate: u32) {
        self.sample_rate.store(sample_rate, Ordering::Relaxed);
    }

    /// 当前这段声音的采样率。cava 的分频规划要靠它，换歌时会变。
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate.load(Ordering::Relaxed)
    }

    /// 采样窗的写入游标，配合 [`AudioLevels::samples_since`] 取增量。
    pub fn sample_cursor(&self) -> usize {
        self.window.cursor()
    }

    /// 自 `since` 以来新到的单声道采样（最多 `max` 个）。
    pub fn samples_since(&self, since: usize, max: usize) -> Vec<f32> {
        self.window.since(since, max)
    }

    /// 按时间顺序取出全部格子（最旧 → 最新），归一化到 0.0 ~ 1.0。
    pub fn snapshot(&self) -> Vec<f32> {
        let cursor = self.cursor.load(Ordering::Relaxed);
        (0..LEVEL_BUCKETS)
            .map(|offset| {
                let index = (cursor + offset) % LEVEL_BUCKETS;
                self.buckets[index].load(Ordering::Relaxed) as f32 / 1000.0
            })
            .collect()
    }

    /// 清零。停止播放时调用，否则会留一段不动的柱子，看着像卡住了。
    pub fn clear(&self) {
        for bucket in self.buckets.iter() {
            bucket.store(0, Ordering::Relaxed);
        }
        self.window.clear();
    }
}

/// 把音源包起来：采样透传，同时把峰值记进 [`AudioLevels`]。
pub struct LevelMeter<S> {
    inner: S,
    levels: AudioLevels,
    peak: f32,
    counted: usize,
    /// 声道数与当前声道下标，用来只取一个声道——立体声的两个声道交错排列，
    /// 全塞进采样窗口等于把两条不同的波形混在一起，频谱会乱。
    channels: u16,
    channel: u16,
}

impl<S> LevelMeter<S>
where
    S: Source,
{
    pub fn new(inner: S, levels: AudioLevels) -> Self {
        let channels = inner.channels().get();
        levels.set_sample_rate(inner.sample_rate().get());
        Self {
            inner,
            levels,
            peak: 0.0,
            counted: 0,
            channels,
            channel: 0,
        }
    }
}

impl<S> Iterator for LevelMeter<S>
where
    S: Source,
{
    type Item = S::Item;

    fn next(&mut self) -> Option<Self::Item> {
        let sample = self.inner.next()?;

        let amplitude = sample.abs();
        if amplitude > self.peak {
            self.peak = amplitude;
        }

        // 只把第 0 声道写进采样窗口，保证窗口里是一条连续的波形
        if self.channel == 0 {
            self.levels.push_sample(sample);
        }
        self.channel = (self.channel + 1) % self.channels;

        self.counted += 1;
        if self.counted >= SAMPLES_PER_BUCKET {
            self.levels.push(self.peak);
            self.peak = 0.0;
            self.counted = 0;
        }

        Some(sample)
    }
}

impl<S> Source for LevelMeter<S>
where
    S: Source,
{
    // rodio 0.22 起叫 current_span_len（旧名是 current_frame_len）
    fn current_span_len(&self) -> Option<usize> {
        self.inner.current_span_len()
    }

    fn channels(&self) -> NonZero<u16> {
        self.inner.channels()
    }

    fn sample_rate(&self) -> NonZero<u32> {
        self.inner.sample_rate()
    }

    fn total_duration(&self) -> Option<Duration> {
        self.inner.total_duration()
    }

    /// 必须转发，否则进度条会完全失效。
    ///
    /// `Source::try_seek` 的**默认实现直接返回 `NotSupported`**。这个包装器如果漏掉它，
    /// rodio 就会认为整条音频不可跳转——点进度条、方向键 seek 全部变成静默失败，
    /// 而播放本身完全正常，极难察觉是这个包装层吞掉的。
    fn try_seek(&mut self, position: Duration) -> Result<(), SeekError> {
        self.inner.try_seek(position)
    }
}
