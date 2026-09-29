//! 多声道 → 立体声下混。
//!
//! # 为什么需要这一层
//!
//! 酷狗**存在多声道的无损文件**，而且不是罕见特例：实测
//! 《东京不太热 (DJ Z新豪版)》（`hash=1953202D07B954E785CA5249E1A64B3C`）的 `flac`
//! 档给的就是一个 **4.0 声道**（FL/FR/BL/BR）的 FLAC，42.5 MiB。
//!
//! 更麻烦的是那个文件的**前两个声道不是这首歌**：
//!
//! | 声道 | dBFS | 与同曲 128 kbps mp3 的相关性 |
//! |---|---|---|
//! | ch0 FL | −21.0 | +0.35 |
//! | ch1 FR | −20.6 | +0.39 |
//! | ch2 BL | −11.7 | **+0.90** |
//! | ch3 BR | −12.8 | **+0.89** |
//!
//! 四路全混与 mp3 的相关性是 +0.999——**歌在后两个声道里，前两个是别的东西**
//! （电平还低 6–10 dB，与前一对声道相关性仅 +0.03）。
//!
//! 而 rodio 0.22 的 `ChannelCountConverter` 在降声道时是**「保留每帧前 N 个样本、
//! 其余直接丢弃」**（`rodio-0.22.2/src/conversions/channels.rs:65-81`，
//! 单测 `remove_channels` 里 4→1 得到 `[1.0, 5.0]`，即只留第 0 声道）。
//! 调用链是 `Player::connect_new(stream.mixer())` → `src/mixer.rs` 的
//! `UniformSourceIterator::new(source, device.channels, device.sample_rate)` →
//! `ChannelCountConverter`。于是「4 声道文件 + 立体声设备」的结果是**只播前两个声道**，
//! 用户听到的就不是这首歌——表现成「音频版本和其他客户端不一致」。
//!
//! 对照：Chromium/Electron 的 `<audio>` 走的是标准下混（各声道加权），
//! 所以同一个文件在 moekoemusic 里正常、在我们这里错。
//!
//! # 为什么在这一层做
//!
//! 唯一的正确位置是**交给 rodio 之前**：只要源自己报的 `channels()` 是 2，
//! rodio 侧的 `ChannelCountConverter` 就成了空操作，不会再丢声道。
//! 反过来，在下游（mixer 之后）已经晚了——声道已经没了。
//!
//! # 下混规则
//!
//! 偶数索引声道 → L，奇数索引声道 → R，各自取平均。对 4.0 就是 `(FL+BL)/2` 与
//! `(FR+BR)/2`。实测（走 `build_decoder` 的真实解码路径，素材是酷狗给的原始文件）
//! 与正确立体声混音的相关系数 **+0.998 / +0.997**。
//!
//! ## 为什么是「平均」而不是「求和」
//!
//! 求和更贴近这个文件的立体声母版（实测 mp3 的 L ≈ `FL+BL`），但**会削顶**：
//! 只要两个声道高度相关，和就能到单声道的两倍。平均则天然安全——输出永远是输入
//! 子集的均值，`|out| <= max|in|`，任何输入都不可能削顶。
//!
//! 代价是电平：这一对声道互不相关（实测相关性 +0.03），而立体声母版是它们的和，
//! 所以平均之后整体比参照低约 **6 dB**。音量由用户掌握，而削顶是实打实的失真——
//! 两害相权取轻。哪天有人抱怨「这首歌比别的歌轻」，改的就是这里的除数，
//! 但改之前先想清楚削顶。
//!
//! ## 已知近似
//!
//! 5.1（FL/FR/FC/LFE/BL/BR）这种带中置与低频声道的布局里，奇偶分组会把 FC 算进 L、
//! LFE 算进 R。实测酷狗上的多声道文件是 4.0，而「近似但能听」远好于「丢声道、
//! 听到的不是这首歌」；真要精确处理得按 `channel_layout` 查系数表，收益配不上复杂度。

use std::num::NonZero;
use std::time::Duration;

use rodio::source::SeekError;
use rodio::{ChannelCount, SampleRate, Source};

/// 把多声道源下混成立体声。
///
/// `channels()` 在源声道数 > 2 时恒返回 2，否则原样透传——**这一点是整个修复的
/// 关键**：rodio 的 `ChannelCountConverter` 按「源报的声道数」决定要不要丢样本。
#[derive(Debug, Clone)]
pub struct StereoDownmix<S> {
    inner: S,
    /// 源自己的声道数。`<= 2` 时整条链退化成纯透传。
    source_channels: u16,
    /// 当前这一帧算出来的 L/R。`slot == 0` 时重算。
    pair: [f32; 2],
    slot: u8,
}

impl<S> StereoDownmix<S>
where
    S: Source<Item = f32>,
{
    pub fn new(inner: S) -> Self {
        let source_channels = inner.channels().get();
        Self {
            inner,
            source_channels,
            pair: [0.0; 2],
            slot: 0,
        }
    }

    /// 需不需要真的下混。单声道与立体声原样透传，一行都不改。
    fn mixes_down(&self) -> bool {
        self.source_channels > 2
    }
}

impl<S> Iterator for StereoDownmix<S>
where
    S: Source<Item = f32>,
{
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        if !self.mixes_down() {
            return self.inner.next();
        }

        if self.slot == 0 {
            // 读满一整帧再算 L/R。读到一半断掉（理论上不该发生）就丢掉这半帧，
            // 不做半个帧的输出——那会让左右声道错位一格，之后整条音频都反相。
            let channels = self.source_channels as usize;
            let (mut left, mut right) = (0.0f32, 0.0f32);
            let (mut left_count, mut right_count) = (0u32, 0u32);
            for index in 0..channels {
                let sample = self.inner.next()?;
                if index % 2 == 0 {
                    left += sample;
                    left_count += 1;
                } else {
                    right += sample;
                    right_count += 1;
                }
            }
            self.pair = [
                left / left_count.max(1) as f32,
                right / right_count.max(1) as f32,
            ];
        }

        let sample = self.pair[self.slot as usize];
        self.slot = (self.slot + 1) % 2;
        Some(sample)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        if !self.mixes_down() {
            return self.inner.size_hint();
        }
        let (min, max) = self.inner.size_hint();
        // 4 个样本进、2 个出；这里只求下界安全，取整即可
        (
            (min / self.source_channels as usize) * 2,
            max.map(|max| (max / self.source_channels as usize) * 2),
        )
    }
}

impl<S> Source for StereoDownmix<S>
where
    S: Source<Item = f32>,
{
    /// **刻意返回 `None`**，不转发内层的 span 长度。
    ///
    /// `UniformSourceIterator` 拿这个值当 `Take` 的上限。内层的长度是按**它自己的**
    /// 声道数算的（4 声道时是 4 的倍数），对我们「2 个样本一帧」的输出不保证是偶数；
    /// 一旦在半个帧上被截断，接下来的 L/R 就会错位一格。返回 `None` 表示
    /// 「没有 span 信息」，rodio 就不会截断。
    fn current_span_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> ChannelCount {
        if self.mixes_down() {
            // 2 是编译期常量，不会失败
            NonZero::new(2).expect("2 非零")
        } else {
            self.inner.channels()
        }
    }

    fn sample_rate(&self) -> SampleRate {
        self.inner.sample_rate()
    }

    fn total_duration(&self) -> Option<Duration> {
        self.inner.total_duration()
    }

    /// 必须转发，否则进度条与方向键 seek 全部静默失效（`Source::try_seek` 的默认
    /// 实现直接返回 `NotSupported`）。`LevelMeter` 踩过同一个坑，见
    /// `docs/MAINTENANCE.md` §5。
    fn try_seek(&mut self, position: Duration) -> Result<(), SeekError> {
        // 跳转后内层从新位置重新开始，当前这半帧作废
        self.slot = 0;
        self.inner.try_seek(position)
    }
}

/// 把任意解码器包成立体声源。`build_decoder` 的唯一出口。
pub fn to_stereo<S>(inner: S) -> Box<dyn Source<Item = f32> + Send>
where
    S: Source<Item = f32> + Send + 'static,
{
    Box::new(StereoDownmix::new(inner))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rodio::buffer::SamplesBuffer;

    /// 交错样本 → SamplesBuffer。`channels` 决定怎么切帧。
    fn buffer(samples: Vec<f32>, channels: u16, rate: u32) -> SamplesBuffer {
        SamplesBuffer::new(
            NonZero::new(channels).unwrap(),
            NonZero::new(rate).unwrap(),
            samples,
        )
    }

    /// 4 声道必须**混**（而不是像 rodio 那样只留前两个）。
    ///
    /// 这条就是本模块存在的理由：输入 `[10,20,30,40]` 的第 1 帧，
    /// 期望输出 `[(10+30)/2, (20+40)/2] = [20, 30]`。
    #[test]
    fn four_channel_frames_are_mixed_not_dropped() {
        let source = buffer(
            vec![10.0, 20.0, 30.0, 40.0, 100.0, 200.0, 300.0, 400.0],
            4,
            8000,
        );
        let mixed = StereoDownmix::new(source);
        assert_eq!(mixed.channels().get(), 2, "输出必须是立体声");
        let out: Vec<f32> = mixed.collect();
        assert_eq!(out, vec![20.0, 30.0, 200.0, 300.0]);
    }

    /// 单声道与立体声一个样本都不许动。
    ///
    /// 这条是「不引入回归」的凭据：绝大多数歌是立体声，下混层对它们必须是恒等。
    #[test]
    fn stereo_and_mono_pass_through_unchanged() {
        let stereo = vec![1.0, 2.0, 3.0, 4.0];
        let out: Vec<f32> = StereoDownmix::new(buffer(stereo.clone(), 2, 8000)).collect();
        assert_eq!(out, stereo);

        let mono = vec![5.0, 6.0];
        let mixed = StereoDownmix::new(buffer(mono.clone(), 1, 8000));
        assert_eq!(
            mixed.channels().get(),
            1,
            "单声道保持单声道，交给 rodio 去复制"
        );
        let out: Vec<f32> = mixed.collect();
        assert_eq!(out, mono);
    }

    /// 声道数与采样率要报对，否则 rodio 的重采样会算错。
    #[test]
    fn reports_two_channels_and_forwards_sample_rate() {
        let mixed = StereoDownmix::new(buffer(vec![0.0; 8], 4, 44100));
        assert_eq!(mixed.channels().get(), 2);
        assert_eq!(mixed.sample_rate().get(), 44100);
    }

    /// span 长度不能转发：内层是按它自己的声道数算的，可能不是 2 的倍数。
    #[test]
    fn span_len_is_withheld_so_frames_are_never_cut_in_half() {
        let source = buffer(vec![0.0; 16], 4, 8000);
        assert!(
            source.current_span_len().is_some(),
            "前提：内层确实报了 span"
        );
        let mixed = StereoDownmix::new(source);
        assert_eq!(mixed.current_span_len(), None);
    }

    /// 帧不完整时丢掉半帧，不能输出一个错位的样本。
    #[test]
    fn a_truncated_trailing_frame_is_dropped() {
        // 4 声道但只有 1 帧半
        let source = buffer(vec![10.0, 20.0, 30.0, 40.0, 1.0, 2.0], 4, 8000);
        let out: Vec<f32> = StereoDownmix::new(source).collect();
        assert_eq!(out, vec![20.0, 30.0], "半帧不输出，否则左右会错位一格");
    }

    /// `try_seek` 必须转发，否则整条音频变成不可跳转（而播放看着完全正常）。
    #[test]
    fn seek_is_forwarded() {
        let source = buffer(vec![0.0; 400], 4, 8000);
        let mut mixed = StereoDownmix::new(source);
        // SamplesBuffer 支持 seek；不转发的话这里会是 NotSupported
        mixed
            .try_seek(Duration::from_millis(10))
            .expect("seek 必须被转发到内层");
    }

    /// **端到端**：过一遍 rodio 的 mixer 之后，到达「设备」的必须是正确的内容。
    ///
    /// 这一条钉的正是 bug 发生的那一层：`Player::connect_new(stream.mixer())` 把源交给
    /// mixer，mixer 再按**输出设备**的声道数做一次归一化（`UniformSourceIterator` →
    /// `ChannelCountConverter`），而那个转换在降声道时是「丢掉多余声道」。
    /// 用 `rodio::mixer::mixer` 搭一个不依赖声卡的 mixer，就能把这一层单独跑出来——
    /// 上面那些测试只证明「我们这一层算得对」，这条才证明「设备最终听到的是对的」。
    ///
    /// 两段对照：源直接进 mixer → 听到的是前两个声道（**修复前的症状**）；
    /// 先下混再进 → 是混好的立体声。
    #[test]
    fn the_mixer_hears_the_song_only_because_we_downmixed_first() {
        fn close(got: &[f32], want: &[f32]) -> bool {
            got.len() == want.len()
                && got
                    .iter()
                    .zip(want)
                    .all(|(got, want)| (got - want).abs() < 1e-6)
        }

        // 复刻实测文件的形状：前两个声道弱、与歌无关，歌在后两个声道
        let mut samples = Vec::new();
        for _ in 0..8 {
            samples.extend_from_slice(&[0.1, -0.1, 0.8, -0.8]);
        }
        let source = buffer(samples, 4, 8000);
        let (channels, rate) = (NonZero::new(2).unwrap(), NonZero::new(8000).unwrap());

        // 对照：源直接进 mixer，后两个声道被丢掉
        let (input, output) = rodio::mixer::mixer(channels, rate);
        input.add(source.clone());
        let heard: Vec<f32> = output.take(4).collect();
        assert!(
            close(&heard, &[0.1, -0.1, 0.1, -0.1]),
            "对照前提不成立：mixer 竟然没有丢声道，实际 {heard:?}"
        );

        // 修复后：先下混，再进 mixer
        let (input, output) = rodio::mixer::mixer(channels, rate);
        input.add(StereoDownmix::new(source));
        let heard: Vec<f32> = output.take(4).collect();
        assert!(
            close(&heard, &[0.45, -0.45, 0.45, -0.45]),
            "到达设备的必须是 (0.1+0.8)/2，实际 {heard:?}"
        );
    }
}
