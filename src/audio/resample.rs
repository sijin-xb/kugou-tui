//! 抗混叠重采样：hi-res 母版降设备率时替代 rodio 的线性插值。
//!
//! # 为什么必须自己做
//!
//! rodio 的 `SampleRateConverter` 是**无滤波的线性插值**（rodio
//! `conversions/sample_rate.rs` 自述「simple linear interpolation」）。44.1k → 48k
//! 这类**上采样**用它没问题（上采样不产生新频率成分）；但 88.2 / 96 / 176.4 kHz
//! 的 hi-res 母版在 48 kHz 设备上是**降采样**——线性插值没有抗混叠滤波，超过
//! 输出奈奎斯特（24 kHz）的内容会原样镜像折叠回可听频带，高频本身还多一层
//! 约 1 dB 的滚降。
//!
//! 实测案例：《起风了》（88.2 kHz 母版，>24 kHz 能量约 -50 dB，音频 MD5 与
//! moekoemusic 拿到的文件完全一致）在 moekoemusic（Electron，走 Chromium 的
//! SincResampler，带抗混叠）里干净，在这里偏「毛」——两边听感不同的病根就
//! 在这一步，不在文件。
//!
//! 所以交给 rodio 之前，把降采样这一步用 rubato 的 sinc 重采样器做掉，并把
//! `sample_rate()` 声明成设备率——rodio 侧的转换器随之变成恒等变换。
//! 上采样不包装（无混叠问题，不值得付 sinc 的 CPU），原样透传给 rodio。
//!
//! # 与 seek 的配合
//!
//! sinc 滤波器有状态（历史样本窗）。`try_seek` 转发给内层源之后必须**重建
//! 重采样器**并清空输出队列，否则跳转后的开头会混进跳转前的样本尾巴——
//! 表现是跳转瞬间一声「咯啦」。

use std::collections::VecDeque;
use std::time::Duration;

use rodio::Source;
use rodio::source::SeekError;
use rubato::{Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType};

/// 每次喂给重采样器的帧数。
///
/// 1024 帧 @48 kHz ≈ 21 ms：一次 sinc 卷积的摊销开销足够低，这点缓冲
/// 对「点进度条到出声」的几十毫秒感知没有贡献。
const CHUNK_SIZE: usize = 1024;

/// 把音源适配到设备采样率，交给 rodio 前调用。
///
/// * 等率 / 上采样 → 原样返回（rodio 的线性插值只在这两种情况无害）；
/// * **降采样** → 包一层 sinc 重采样，`sample_rate()` 报设备率。
pub fn to_device_rate(
    source: Box<dyn Source<Item = f32> + Send>,
    device_rate: u32,
) -> Box<dyn Source<Item = f32> + Send> {
    if source.sample_rate().get() <= device_rate {
        return source;
    }
    match Resampled::new(source, device_rate) {
        Ok(resampled) => Box::new(resampled),
        // 重采样器建不起来的话源已经被移动走、拿不回来了。参数是常量，
        // 这条路不可达；真发生就给一段静音（在 48k 声明的空源），
        // 绝不 panic——音频链路崩了界面也活不成。
        Err(_) => Box::new(SilentSource),
    }
}

/// [`to_device_rate`] 不可达分支的占位：一个立即结束的静音源。
struct SilentSource;

impl Iterator for SilentSource {
    type Item = f32;
    fn next(&mut self) -> Option<f32> {
        None
    }
}

impl Source for SilentSource {
    fn channels(&self) -> rodio::ChannelCount {
        rodio::ChannelCount::try_from(2).expect("2 非零")
    }
    fn sample_rate(&self) -> rodio::SampleRate {
        rodio::SampleRate::try_from(48000).expect("48000 非零")
    }
    fn current_span_len(&self) -> Option<usize> {
        None
    }
    fn total_duration(&self) -> Option<std::time::Duration> {
        None
    }
}

/// sinc 重采样的 `Source` 适配器。
///
/// 数据流：内层源（交织样本迭代器）→ 按[`CHUNK_SIZE`]帧解交织 → rubato
/// sinc 卷积 → 输出队列 → 交织 yield。任一时刻队列里至多一个块的输出，
/// 内存占用与块大小同量级，不随曲目长度增长。
struct Resampled<S> {
    inner: S,
    device_rate: u32,
    resampler: SincFixedIn<f32>,
    /// rubato 的输入侧：按通道解交织，各通道容量 [`CHUNK_SIZE`]。
    input: Vec<Vec<f32>>,
    /// `input` 里已填的帧数（< [`CHUNK_SIZE`] 时只出现在内层取空之后）。
    input_filled: usize,
    /// 重采样出的交织样本，依次 yield。
    output: VecDeque<f32>,
    /// 重采样出的交织样本暂存（`process_into_buffer` 的输出侧）。
    out_buffer: Vec<Vec<f32>>,
    /// 流结束的状态机：正常喂块 → 喂一次不满的尾巴 → 一次 flush → 结束。
    stage: Stage,
    /// [`Stage::Tail`] 的 flush 是否已经跑过（它只允许跑一次，见 Stage 文档）。
    flushed: bool,
}

/// [`Resampled`] 的收尾状态机。
///
/// **Drain 只跑一次**：`process_partial(None)` 会拿零输入再跑一整块，sinc
/// 对全零输入的输出**恰好是零但永远产得出**——不限次的话迭代器永不结束
/// （第一版就挂死在这里）。sinc_len = 256 < CHUNK_SIZE = 1024，一次 None
/// 必然把滤波器记忆全部推干净。
#[derive(Debug, Clone, Copy, PartialEq)]
enum Stage {
    /// 正常喂整块。
    Normal,
    /// 内层已取空：先喂一次手头的不满块，再 flush 一次，然后结束。
    Tail,
}

impl<S> Resampled<S>
where
    S: Source<Item = f32> + Send,
{
    fn new(inner: S, device_rate: u32) -> Result<Self, rubato::ResamplerConstructionError> {
        let channels = usize::from(inner.channels().get());
        let ratio = Self::ratio(inner.sample_rate().get(), device_rate);
        let resampler = SincFixedIn::<f32>::new(ratio, 1.0, Self::params(), CHUNK_SIZE, channels)?;
        let out_capacity = resampler.output_frames_max();
        Ok(Self {
            device_rate,
            input: vec![vec![0.0; CHUNK_SIZE]; channels],
            out_buffer: vec![vec![0.0; out_capacity]; channels],
            inner,
            resampler,
            output: VecDeque::new(),
            input_filled: 0,
            stage: Stage::Normal,
            flushed: false,
        })
    }

    /// 重采样比例（输出 / 输入）。只包装降采样，恒 < 1。
    fn ratio(source_rate: u32, device_rate: u32) -> f64 {
        f64::from(device_rate) / f64::from(source_rate)
    }

    fn params() -> SincInterpolationParameters {
        SincInterpolationParameters {
            sinc_len: 256,
            // 截止相对「输入/输出里较低的那个奈奎斯特」（rubato 文档原话）。
            // 我们只包装降采样，较低的一侧恒为输出——0.95 是文档的推荐起点，
            // 留 5% 过渡带让滤波器滚降得动。
            f_cutoff: 0.95,
            oversampling_factor: 256,
            interpolation: SincInterpolationType::Cubic,
            window: rubato::WindowFunction::BlackmanHarris2,
        }
    }

    /// 从内层拉帧解交织进 `input`，返回这轮是否拉到了帧。
    fn fill_input(&mut self) -> bool {
        let channels = usize::from(self.channels().get());
        let mut pulled = 0usize;
        'frames: while self.input_filled + pulled < CHUNK_SIZE {
            for (_channel, slot) in self.input.iter_mut().enumerate().take(channels) {
                match self.inner.next() {
                    // 一个通道断了就当整帧结束：rodio 的源按完整帧产出
                    Some(sample) => {
                        slot[self.input_filled + pulled] = sample;
                    }
                    None => break 'frames,
                }
            }
            pulled += 1;
        }
        self.input_filled += pulled;
        pulled > 0
    }

    /// 跑一次整块卷积，把输出塞进 `output` 队列。
    fn process_chunk(&mut self) -> usize {
        let channels = usize::from(self.channels().get());
        let produced_buffers = self
            .resampler
            .process_into_buffer(&self.input, &mut self.out_buffer, None)
            .map(|(_, output_frames)| {
                (0..channels)
                    .map(|channel| self.out_buffer[channel][..output_frames].to_vec())
                    .collect::<Vec<_>>()
            });
        Self::absorb(produced_buffers, &mut self.output)
    }

    /// 结尾 flush：零输入跑一块，把滤波器记忆推出来（见 [`Stage`] 文档）。
    fn process_flush(&mut self) -> usize {
        let produced = self.resampler.process_partial::<Vec<f32>>(None, None);
        Self::absorb(produced, &mut self.output)
    }

    /// 内层取空后的**最后一次**喂入：把手头不满一块的尾巴交给 rubato。
    /// 喂完这一下，之后只允许 `process_flush`。
    fn process_chunk_tail(&mut self) -> usize {
        let produced = self
            .resampler
            .process_partial::<Vec<f32>>(Some(&self.input), None);
        Self::absorb(produced, &mut self.output)
    }

    /// 把 rubato 的按通道输出交织塞进输出队列。
    fn absorb(
        produced: Result<Vec<Vec<f32>>, rubato::ResampleError>,
        output: &mut VecDeque<f32>,
    ) -> usize {
        let Ok(buffers) = produced else {
            // rubato 对形状不对的输入返回 Err；我们的输入形状是常量构造的，
            // 到不了这里。真到了就当没有输出（静音一段好过 panic）。
            return 0;
        };
        let mut count = 0usize;
        let frames = buffers.first().map(Vec::len).unwrap_or(0);
        for frame in 0..frames {
            for buffer in &buffers {
                // rubato 的 into_buffer 版会先清空输出缓冲再填，所以这里
                // 按实际长度取，不预支容量
                if let Some(&sample) = buffer.get(frame) {
                    output.push_back(sample);
                    count += 1;
                }
            }
        }
        count
    }

    /// 重建滤波器、清空缓冲。seek 之后必须调用，否则跳转后的开头
    /// 会混入跳转前的样本尾巴。
    fn reset(&mut self) {
        let ratio = Self::ratio(self.inner.sample_rate().get(), self.device_rate);
        if let Ok(resampler) = SincFixedIn::<f32>::new(
            ratio,
            1.0,
            Self::params(),
            CHUNK_SIZE,
            usize::from(self.channels().get()),
        ) {
            self.resampler = resampler;
        }
        self.output.clear();
        for slot in &mut self.input {
            slot.fill(0.0);
        }
        self.input_filled = 0;
        self.stage = Stage::Normal;
        self.flushed = false;
    }
}

impl<S> Iterator for Resampled<S>
where
    S: Source<Item = f32> + Send,
{
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        loop {
            if let Some(sample) = self.output.pop_front() {
                return Some(sample);
            }

            match self.stage {
                Stage::Normal => {
                    self.input_filled = 0;
                    if self.fill_input() {
                        self.process_chunk();
                        continue;
                    }
                    // 内层取空：喂一次手头的不满块（可能是零块），转 Tail
                    self.process_chunk_tail();
                    self.stage = Stage::Tail;
                }
                Stage::Tail => {
                    if !self.flushed {
                        // 一次 None 把 sinc 的滤波器记忆推干净（见 Stage 文档）。
                        // **只跑一次**：再跑产出的是无限多的精确零样本——
                        // 迭代器永不结束，第一版就挂死在这里。
                        self.process_flush();
                        self.flushed = true;
                        continue;
                    }
                    if self.output.is_empty() {
                        // 复位状态机，等待 seek 之后的重用
                        self.stage = Stage::Normal;
                        self.flushed = false;
                        return None;
                    }
                }
            }
        }
    }
}

impl<S> Source for Resampled<S>
where
    S: Source<Item = f32> + Send,
{
    fn channels(&self) -> rodio::ChannelCount {
        self.inner.channels()
    }

    /// 声明成设备率——这正是包装的意义：rodio 侧的转换变成恒等。
    fn sample_rate(&self) -> rodio::SampleRate {
        rodio::SampleRate::try_from(self.device_rate).expect("设备采样率非零")
    }

    fn total_duration(&self) -> Option<Duration> {
        // 重采样不改变时长（sinc 的十几毫秒群延迟忽略不计）
        self.inner.total_duration()
    }

    /// 块处理输出,「下一帧长度」没有稳定值——None 表示不承诺。
    fn current_span_len(&self) -> Option<usize> {
        None
    }

    fn try_seek(&mut self, position: Duration) -> Result<(), SeekError> {
        self.inner.try_seek(position)?;
        // 内层已经跳过去了；本层的滤波器状态是跳转前的历史，必须丢掉
        self.reset();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rodio::ChannelCount;

    /// 一个可seek的正弦波源，测试用。
    struct Sine {
        rate: u32,
        frequency: f32,
        amplitude: f32,
        position: u64,
        length: u64,
    }

    impl Sine {
        fn new(rate: u32, frequency: f32, seconds: f32) -> Self {
            Self {
                rate,
                frequency,
                amplitude: 0.5,
                position: 0,
                length: (f64::from(rate) * f64::from(seconds)) as u64,
            }
        }

        fn sample_at(&self, position: u64) -> f32 {
            (2.0 * std::f32::consts::PI * self.frequency * position as f32 / self.rate as f32).sin()
                * self.amplitude
        }
    }

    impl Iterator for Sine {
        type Item = f32;
        fn next(&mut self) -> Option<f32> {
            if self.position >= self.length {
                return None;
            }
            let sample = self.sample_at(self.position);
            self.position += 1;
            Some(sample)
        }
    }

    impl Source for Sine {
        fn channels(&self) -> ChannelCount {
            ChannelCount::try_from(1).expect("1 非零")
        }
        fn sample_rate(&self) -> rodio::SampleRate {
            rodio::SampleRate::try_from(self.rate).expect("测试采样率非零")
        }
        fn total_duration(&self) -> Option<Duration> {
            Some(Duration::from_secs_f64(
                self.length as f64 / f64::from(self.rate),
            ))
        }
        fn current_span_len(&self) -> Option<usize> {
            None
        }
        fn try_seek(&mut self, position: Duration) -> Result<(), SeekError> {
            let target = (f64::from(self.rate) * position.as_secs_f64()) as u64;
            self.position = target.min(self.length);
            Ok(())
        }
    }

    fn rms(samples: impl Iterator<Item = f32>) -> f32 {
        let (sum, count) = samples.fold((0.0f64, 0usize), |(sum, count), sample| {
            (sum + f64::from(sample) * f64::from(sample), count + 1)
        });
        (sum / count.max(1) as f64).sqrt() as f32
    }

    #[test]
    fn equal_or_higher_source_rate_passes_through() {
        // 44.1k 源在 48k 设备上是上采样——原样透传（同一类型的 Box 无法直接
        // 比较，这里用「输出采样率仍是源率」来识别透传）
        let source = Box::new(Sine::new(44100, 1000.0, 1.0)) as Box<dyn Source<Item = f32> + Send>;
        let wrapped = to_device_rate(source, 48000);
        assert_eq!(wrapped.sample_rate().get(), 44100, "上采样应透传给 rodio");

        let source = Box::new(Sine::new(48000, 1000.0, 1.0)) as Box<dyn Source<Item = f32> + Send>;
        let wrapped = to_device_rate(source, 48000);
        assert_eq!(wrapped.sample_rate().get(), 48000, "等率应透传");
    }

    /// 降采样后的源必须**声明设备率**——这是整件事的意义：rodio 侧的
    /// 转换器因此变成恒等。
    #[test]
    fn downsampled_source_declares_the_device_rate() {
        let source = Box::new(Sine::new(88200, 1000.0, 1.0)) as Box<dyn Source<Item = f32> + Send>;
        let wrapped = to_device_rate(source, 48000);
        assert_eq!(wrapped.sample_rate().get(), 48000);
    }

    /// 输出长度必须与输入时长一致（±滤波器边沿）。
    #[test]
    fn output_length_matches_the_input_duration() {
        let source = Box::new(Sine::new(88200, 1000.0, 1.0)) as Box<dyn Source<Item = f32> + Send>;
        let wrapped = to_device_rate(source, 48000);
        let count = wrapped.count();
        // 1 秒 @48k = 48000。多出来的部分是**结尾静音**：FixedIn 的输入块固定
        // 1024 帧，最后一块不足的部分以零填充（静音），再加一次 flush 的滤波器
        // 尾——合计最多 ~30 ms，听不见，也不影响进度条（时长声明来自内层源）。
        assert!(
            (46800..=51000).contains(&count),
            "1 秒的 88.2k 源应产出 ~48000 样本（含至多一块的静音尾），实际 {count}"
        );
    }

    /// **抗混叠是本模块存在的理由**：30 kHz 的音（在 48k 设备的奈奎斯特之外，
    /// 本来就该被滤掉）降采样后不得在可听频带里冒出来。
    ///
    /// rodio 的线性插值在这个场景会把 30 kHz 镜像到 18 kHz、幅度几乎不减；
    /// sinc 版必须把它压到 -26 dB（原幅度的 5%）以下。
    #[test]
    fn ultrasonic_tone_is_suppressed_not_aliased() {
        let source = Box::new(Sine::new(88200, 30000.0, 0.5)) as Box<dyn Source<Item = f32> + Send>;
        let wrapped = to_device_rate(source, 48000);
        let output_rms = rms(wrapped);
        let input_rms = 0.5 * std::f32::consts::FRAC_1_SQRT_2;
        assert!(
            output_rms < input_rms * 0.05,
            "30kHz 应被抗混叠滤波压掉（输出 RMS {output_rms:.4}，应 < {:.4}）",
            input_rms * 0.05
        );
    }

    /// 带内信号必须保真地过去：1 kHz 正弦降采样后幅度应保住（±1.5 dB）。
    #[test]
    fn in_band_tone_passes_with_its_amplitude() {
        let source = Box::new(Sine::new(88200, 1000.0, 1.0)) as Box<dyn Source<Item = f32> + Send>;
        let wrapped = to_device_rate(source, 48000);
        // 跳过开头的滤波器建立段，取中段量 RMS（要落在真实信号区，
        // 别滑进结尾那 ~30 ms 静音尾）
        let mid: Vec<f32> = wrapped.skip(9600).take(19200).collect();
        let output_rms = rms(mid.into_iter());
        let input_rms = 0.5 * std::f32::consts::FRAC_1_SQRT_2;
        let ratio = output_rms / input_rms;
        assert!(
            (0.84..=1.19).contains(&ratio),
            "带内 1 kHz 应保幅过去（输出/输入 RMS = {ratio:.3}）"
        );
    }

    /// seek 之后输出必须从头再来，且不能带着跳转前的样本尾巴。
    #[test]
    fn seek_restarts_the_output_from_the_target() {
        let source = Box::new(Sine::new(88200, 1000.0, 2.0)) as Box<dyn Source<Item = f32> + Send>;
        let mut wrapped = to_device_rate(source, 48000);
        // 消耗 1 秒，然后跳回 0.5 秒处
        let _ = wrapped.by_ref().take(48000).count();
        wrapped
            .try_seek(Duration::from_secs_f64(0.5))
            .expect("seek");

        // 0.5 秒处的相位 = sin(2π·1000·0.5) = 0，且正处于上升段——
        // 开头几个样本应与 0.5 秒处的原始采样一致（相位连续）
        let first: Vec<f32> = wrapped.take(48).collect();
        assert!(!first.is_empty(), "seek 后应继续出样本");
        let expected_phase =
            (2.0 * std::f32::consts::PI * 1000.0 * 0.5) % (2.0 * std::f32::consts::PI);
        let expected_sign = expected_phase.sin();
        let mean: f32 = first.iter().sum::<f32>() / first.len() as f32;
        assert!(
            mean.signum() == expected_sign.signum() || mean.abs() < 0.05,
            "seek 后的输出应从目标位置继续（均值 {mean}，期望符号 {expected_sign}）"
        );
    }
}
