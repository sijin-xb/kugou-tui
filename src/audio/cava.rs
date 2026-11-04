//! cava 算法移植（上游 cavacore.c，MIT，作者 Karl Staveström）。
//!
//! 与上游逐行对应，差异只在语言和数据结构：
//!
//! * 上游用 FFTW 跑两个 r2c 实数变换（低频 8192 点、中高频 4096 点）；
//!   这里用 `caps::fft` 的 radix-2 复变换，长度不变，取模长后与上游等价。
//! * 上游把 `cava_in` 当 double 收，这里收 f32——`SampleWindow` 本来就是 f32。
//! * 声道数固定为 1：`LevelMeter` 只把**一个声道**写进采样窗（见 `levels.rs`），
//!   所以 left/right 两套缓冲不需要存在。
//!
//! 三个可调旋钮沿用上游语义：`noise_reduction` (0..1)、`autosens`（自动增益）、
//! `low_cut_off` / `high_cut_off`（Hz）。上游配置里的 `integral` / `gravity` /
//! `monstercat` 在这个实现里是由 `noise_reduction` 与实测帧率推导出来的，
//! 不单独暴露——对外只有一条可调的平滑强度，比上游少四个互相耦合的参数。
//!
//! 不移植的部分：双声道、`scaling_mode = decibel`、`shmem` 输入、SDL/ncurses
//! 输出。都是本项目用不到的。

use crate::audio::spectrum::fft;

/// 低频段使用的 FFT 点数（采样率 44.1kHz 时）。
///
/// 上游按采样率倍增：8.125k~16.25k 用 1024，依此类推到 705k 用 65536。
/// 低频多给点数是为了频率分辨率——20Hz 与 25Hz 在 4096 点上落在同一个 bin。
const BASE_FFT_SIZE: usize = 4096;

/// 低频段与中高频段的 FFT 点数之比。
const BASS_FFT_RATIO: usize = 2;

/// 低频段与中高频段的分界频率（Hz）。上游写死 100，不对用户暴露。
const BASS_CUTOFF_HZ: f32 = 100.0;

/// 上游把 Hann 窗两端各收 1 个样本（`/ (n - 1)` 而非 `/ n`），这里保持一致。
fn hann_window(size: usize) -> Vec<f32> {
    (0..size)
        .map(|i| 0.5 * (1.0 - (2.0 * std::f32::consts::PI * i as f32 / (size - 1) as f32).cos()))
        .collect()
}

/// 按采样率选 FFT 点数，与上游 cavacore.c 的分档一致。
fn fft_size_for_rate(rate: u32) -> usize {
    let mut size = 512;
    if rate > 8125 && rate <= 16250 {
        size *= 2;
    } else if rate > 16250 && rate <= 32500 {
        size *= 4;
    } else if rate > 32500 && rate <= 75000 {
        size *= 8;
    } else if rate > 75000 && rate <= 150000 {
        size *= 16;
    } else if rate > 150000 && rate <= 300000 {
        size *= 32;
    } else if rate > 300000 && rate <= 705600 {
        size *= 64;
    } else if rate > 705600 {
        size *= 128;
    }
    size
}

/// 一次分析的全部状态。跨帧保留平滑历史与 autosens 增益。
#[derive(Debug, Clone)]
pub struct Cava {
    bars: usize,
    rate: u32,
    autosens: bool,
    noise_reduction: f32,

    bass_size: usize,
    mid_size: usize,

    bass_window: Vec<f32>,
    mid_window: Vec<f32>,

    /// 滑动输入缓冲，长度 `bass_size * 2`。新样本从尾部压入，旧的从头挤掉。
    input: Vec<f32>,

    lower_cut_off: Vec<usize>,
    upper_cut_off: Vec<usize>,
    bass_cut_off_bar: usize,
    eq: Vec<f32>,

    fall: Vec<f32>,
    memory: Vec<f32>,
    peak: Vec<f32>,
    prev: Vec<f32>,

    sens: f32,
    sens_init: bool,
    framerate: f32,
    frame_skip: u32,

    // ---- 预分配的热路径缓冲 ----
    //
    // 这些每帧都要用，原来在 `execute` 里现场 `vec!`/`collect`。每帧约 170 KiB
    // 的分配本身不算多，但 30fps 下每秒上百次会让堆反复切碎，glibc 因此留住了
    // 本可归还的页——实测 RSS 比基线高 6 MiB，其中真正「在用」的不到 1 MiB。
    // 改为只写不分配之后，稳态 RSS 与缓冲区实际大小成正比。
    bass_scratch: Vec<f32>,
    mid_scratch: Vec<f32>,
    bass_re: Vec<f32>,
    bass_im: Vec<f32>,
    mid_re: Vec<f32>,
    mid_im: Vec<f32>,
    bass_magnitudes: Vec<f32>,
    mid_magnitudes: Vec<f32>,
    out: Vec<f32>,
}

impl Cava {
    /// 一组测试用参数：autosens 开、`noise_reduction` 1.0、50Hz~10kHz。
    ///
    /// 这组值与我在本机跑 `libcava 1.0.0` 取 ground truth 时用的一致（见
    /// `a_pure_tone_lights_the_expected_band`）。生产路径不走这里——它按
    /// `Config` 里的 `cava_*` 项构造。
    #[cfg(test)]
    pub fn new(bars: usize, rate: u32) -> Self {
        Self::with_options(bars, rate, 1.0, true, 50, 10_000)
    }

    /// `noise_reduction` 建议 0..1；上游注释推荐 0.77。
    pub fn with_options(
        bars: usize,
        rate: u32,
        noise_reduction: f32,
        autosens: bool,
        low_cut_off: u32,
        high_cut_off: u32,
    ) -> Self {
        let bars = bars.max(1);
        let rate = rate.max(1);
        let high_cut_off = high_cut_off.min(rate / 2);
        let low_cut_off = low_cut_off
            .max(1)
            .min(high_cut_off.saturating_sub(1).max(1));

        let mid_size = if rate <= 8125 {
            // 上游这一档保持 512；但 `BASE_FFT_SIZE` 是 4096 起点，这里按比例折算。
            BASE_FFT_SIZE / 8
        } else {
            fft_size_for_rate(rate)
        };
        let bass_size = mid_size * BASS_FFT_RATIO;

        let (lower_cut_off_bins, upper_cut_off_bins, bass_cut_off_bar, eq) =
            Self::plan_bands(bars, rate, low_cut_off, high_cut_off, bass_size, mid_size);

        Self {
            bars,
            rate,
            autosens,
            noise_reduction: noise_reduction.clamp(0.0, 1.0),
            bass_size,
            mid_size,
            bass_window: hann_window(bass_size),
            mid_window: hann_window(mid_size),
            input: vec![0.0; bass_size * 2],
            lower_cut_off: lower_cut_off_bins,
            upper_cut_off: upper_cut_off_bins,
            bass_cut_off_bar,
            eq,
            fall: vec![0.0; bars],
            memory: vec![0.0; bars],
            peak: vec![0.0; bars],
            prev: vec![0.0; bars],
            // 上游：sens 从 1.0 起，sens_init 为真时开局加快收敛
            sens: 1.0,
            sens_init: true,
            framerate: 75.0,
            frame_skip: 1,
            bass_scratch: vec![0.0; bass_size],
            mid_scratch: vec![0.0; mid_size],
            bass_re: vec![0.0; bass_size],
            bass_im: vec![0.0; bass_size],
            mid_re: vec![0.0; mid_size],
            mid_im: vec![0.0; mid_size],
            bass_magnitudes: vec![0.0; bass_size / 2 + 1],
            mid_magnitudes: vec![0.0; mid_size / 2 + 1],
            out: vec![0.0; bars],
        }
    }

    /// 把对数分布的频段边界换算成 bin 下标。
    ///
    /// 逐行对应上游 `cava_init` 的 "process: calculate cutoff frequencies and eq"：
    /// 低频段（< 100Hz）走 `bass_size` 的 bin，其余走 `mid_size` 的 bin。
    /// 相邻频段挤在同一个 bin 上时，上游把后面的频段往后推一格（"pushing the
    /// spectrum up"）——不推的话低频那几根柱子会永远一模一样。
    fn plan_bands(
        bars: usize,
        rate: u32,
        low_cut_off: u32,
        high_cut_off: u32,
        bass_size: usize,
        mid_size: usize,
    ) -> (Vec<usize>, Vec<usize>, usize, Vec<f32>) {
        // 上游这里是 `p->rate / p->FFTbassbufferSize`，两个整数相除后再赋给
        // float——44100/8192 得 5，不是 5.38。改成浮点除会让频段边界整体右移，
        // 低频那几根的位置和 cava 对不上。
        let min_bandwidth = (rate / bass_size as u32) as f32;
        let frequency_constant =
            (low_cut_off as f32 / high_cut_off as f32).log10() / (1.0 / (bars as f32 + 1.0) - 1.0);

        let mut lower = vec![0usize; bars + 1];
        let mut upper = vec![0usize; bars + 1];
        let mut cut_off_frequency = vec![0.0f32; bars + 1];
        let mut relative = vec![0.0f32; bars + 1];
        let mut bass_cut_off_bar = 0usize;
        // 上游的 `first_bar` 是**循环外**的变量：它在「进中高频段的那一根」被置 1，
        // 之后每根又清零，但**下一次迭代开头不会重置**。写成循环内局部变量会让
        // 每根都重新判断，低频段边界就会回退（`lower[n] < lower[n-1]`）。
        let mut first_bar = true;

        for n in 0..=bars {
            let mut coefficient = -frequency_constant;
            coefficient += (n as f32 + 1.0) / (bars as f32 + 1.0) * frequency_constant;
            cut_off_frequency[n] = high_cut_off as f32 * 10f32.powf(coefficient);

            if n > 0 && cut_off_frequency[n - 1] >= cut_off_frequency[n] {
                cut_off_frequency[n] = cut_off_frequency[n - 1] + min_bandwidth;
            }

            relative[n] = cut_off_frequency[n] / (rate as f32 / 2.0);

            if cut_off_frequency[n] < BASS_CUTOFF_HZ {
                // 低频段：走大窗口的 bin
                lower[n] = (relative[n] * (bass_size / 2) as f32) as usize;
                bass_cut_off_bar += 1;
                if bass_cut_off_bar > 1 {
                    first_bar = false;
                }
                lower[n] = lower[n].min(bass_size / 2);
            } else {
                // 中高频段：走小窗口的 bin
                lower[n] = (relative[n] * (mid_size / 2) as f32).ceil() as usize;
                if n == bass_cut_off_bar {
                    first_bar = true;
                    if n > 0 {
                        // 上一根（最后一根低频）的上界要用**大窗口**的 bin 算
                        upper[n - 1] = (relative[n] * (bass_size / 2) as f32).max(0.0) as usize;
                        upper[n - 1] = upper[n - 1].saturating_sub(1);
                    }
                } else {
                    first_bar = false;
                }
                lower[n] = lower[n].min(mid_size / 2);
            }

            if n > 0 {
                if !first_bar {
                    upper[n - 1] = lower[n].saturating_sub(1);
                    if lower[n] <= lower[n - 1] {
                        let limit = if n < bass_cut_off_bar {
                            bass_size / 2 + 1
                        } else {
                            mid_size / 2 + 1
                        };
                        if lower[n - 1] + 1 < limit {
                            lower[n] = lower[n - 1] + 1;
                            upper[n - 1] = lower[n] - 1;
                        }
                    }
                } else if upper[n - 1] < lower[n - 1] {
                    upper[n - 1] = lower[n - 1] + 1;
                }
            }

            relative[n] = if n < bass_cut_off_bar {
                lower[n] as f32 / (bass_size / 2) as f32
            } else {
                lower[n] as f32 / (mid_size / 2) as f32
            };
            cut_off_frequency[n] = relative[n] * (rate as f32 / 2.0);
        }

        // 上游的 "hard coded eq"：FFTW 输出幅度极大，先除以 2^28 归一，
        // 再按频率抬升高频（^0.85）补偿音乐能量天然集中低频，最后按频段
        // 覆盖的 bin 数摊平——宽频段聚了更多 bin，不摊的话高频柱子会虚高。
        let mut eq = vec![0.0f32; bars];
        for n in 0..bars {
            let mut value = (1.0 / 2f32.powi(28)) * cut_off_frequency[n + 1].powf(0.85);
            let log2_size = if n < bass_cut_off_bar {
                (bass_size as f32).log2()
            } else {
                (mid_size as f32).log2()
            };
            value /= log2_size;
            let span = upper[n].saturating_sub(lower[n]) + 1;
            value /= span.max(1) as f32;
            eq[n] = value;
        }

        for n in 0..bars {
            if upper[n] < lower[n] {
                upper[n] = lower[n];
            }
        }

        (lower, upper, bass_cut_off_bar, eq)
    }

    /// 喂入新采样（单声道、归一化到 -1..1），返回 `bars` 个 0..1 的电平。
    ///
    /// 上游这里还有一层 `scaling_mode == decibel` 时除以 32768 的归一化；
    /// 本项目采样窗存的就是 -1..1 的 f32，不需要。
    /// 返回的 `&[f32]` 指向内部缓冲，下一次 `execute` 会覆盖它。
    /// 调用方应当立刻拷贝或用完，不要跨帧持有。
    pub fn execute(&mut self, samples: &[f32]) -> &[f32] {
        if samples.is_empty() {
            self.frame_skip += 1;
            self.out.fill(0.0);
            return &self.out;
        }

        // 上游：按实际到达的采样数估算帧率，autosens 与平滑都按它修正，
        // 这样 30fps 与 144fps 下的手感才一致。除以 64 是低通。
        self.framerate -= self.framerate / 64.0;
        self.framerate += (self.rate * self.frame_skip) as f32 / samples.len() as f32 / 64.0;
        self.frame_skip = 1;

        let new_samples = samples.len().min(self.input.len());
        let len = self.input.len();
        // 缓冲按上游的**倒序**语义存放：`input[0]` 是最新样本，越往后越旧。
        //
        // 这不是风格偏好。上游 `cava_execute` 里 `input_buffer[n]` 直接配
        // `multiplier[n]`，而 Hann 窗两端为 0——也就是**最新的几个样本在窗的
        // 零端、最旧的样本在另一端**。反过来存（正序、尾部最新）会让中频段取
        // 到的窗口跨越不同时刻的采样，频率泄漏大三个数量级：实测噪声底
        // 1.7e-2 对上游的 3.8e-5，柱子会糊成一片宽丘而不是尖峰。
        //
        // 顺序必须是「先整体右移、再倒写前段」：先写会让刚写进去的新样本被
        // 右移搬走。
        let keep = len - new_samples;
        self.input.copy_within(0..keep, new_samples);
        for (n, sample) in samples[samples.len() - new_samples..].iter().enumerate() {
            self.input[new_samples - n - 1] = *sample;
        }

        // 两段都取 `input` 的**开头**：那里是最新的样本，也是上游的取法。
        for (slot, (sample, window)) in self
            .bass_scratch
            .iter_mut()
            .zip(self.input[..self.bass_size].iter().zip(&self.bass_window))
        {
            *slot = sample * window;
        }
        for (slot, (sample, window)) in self
            .mid_scratch
            .iter_mut()
            .zip(self.input[..self.mid_size].iter().zip(&self.mid_window))
        {
            *slot = sample * window;
        }

        // 两段 FFT 的输入/输出都复用缓冲，形状对不上时直接跳过（不会发生，
        // 缓冲长度在构造时按 `bass_size`/`mid_size` 定死）。
        let (Some(bass_magnitudes), Some(mid_magnitudes)) = (
            magnitude_into(
                &self.bass_scratch,
                &mut self.bass_re,
                &mut self.bass_im,
                &mut self.bass_magnitudes,
            ),
            magnitude_into(
                &self.mid_scratch,
                &mut self.mid_re,
                &mut self.mid_im,
                &mut self.mid_magnitudes,
            ),
        ) else {
            self.out.fill(0.0);
            return &self.out;
        };

        // 每个频段把覆盖的 bin 的模长加起来，再乘 eq
        for (n, slot) in self.out.iter_mut().enumerate() {
            let magnitudes = if n < self.bass_cut_off_bar {
                &bass_magnitudes
            } else {
                &mid_magnitudes
            };
            let low = self.lower_cut_off[n].min(magnitudes.len().saturating_sub(1));
            let high = self.upper_cut_off[n].min(magnitudes.len().saturating_sub(1));
            let mut sum = 0.0f32;
            for value in &magnitudes[low..=high.max(low)] {
                sum += value;
            }
            *slot = sum * self.eq[n];
        }

        if self.autosens {
            for value in &mut self.out {
                *value *= self.sens;
            }
        }

        // 开局定标：autosens 从 sens=1 出发按每帧 ~+9% 几何爬升，音量正常的
        // 信号要 2~3 秒才能爬到可用增益——表现为「切到可视化页要等几秒柱子
        // 才动」。上游 cavacore 也这样（sens_init 只是把爬速加倍），CLI 用户
        // 不在意，但切页即看的体验等不起。
        //
        // 这里在 sens_init 窗口内用**本帧 autosens 前的原始峰值**直接把 sens
        // 解析定标到「峰值略超 1」的位置：等于把几何爬升的极限一步算出来。
        // 只在 sens_init 期间生效（第一个 overshoot 就关掉它），之后的动态
        // 与上游完全一致——顶格压低、安静抬高都不受影响。前几帧仍按上游
        // 原样输出（memory/peak 等平滑历史在积累），只是增益提前到位。
        if self.autosens && self.sens_init {
            let raw_peak = self.out.iter().cloned().fold(0.0f32, f32::max);
            if raw_peak > 1e-9 {
                // 目标：定标后峰值落在 1.2（略顶格，下一帧 autosens 自然接管）。
                // 增益限到 4000 倍：极弱信号（如暂停后的残余噪声）不该被拉满，
                // 那种场景交给上游式的缓慢爬升更稳。
                let target_sens = (1.2 / raw_peak).min(4_000.0);
                self.sens = self.sens.max(target_sens);
            }
        }

        let framerate_mod = 66.0 / self.framerate.max(1.0);
        let gravity_mod = framerate_mod.powf(2.5) * 2.0 / self.noise_reduction.max(0.01);
        let integral_mod = framerate_mod.powf(0.1);

        let mut overshoot = false;
        for (n, slot) in self.out.iter_mut().enumerate() {
            let mut value = *slot;

            // 回落：用一个逐渐变大的 fall 系数把值从「近期峰值」往下拉，
            // 形成 cava 那种「冲上去、慢慢掉」的形状。noise_reduction 太低时
            // 跳过，因为那本来就是「要快」的档位。
            if value < self.prev[n] && self.noise_reduction > 0.1 {
                value = self.peak[n] * (1.0 - self.fall[n] * self.fall[n] * gravity_mod);
                if value < 0.0 {
                    value = 0.0;
                }
                self.fall[n] += 0.028;
            } else {
                self.peak[n] = value;
                self.fall[n] = 0.0;
            }
            self.prev[n] = value;

            // 积分：把上一帧的记忆按 noise_reduction 掺进来，压掉抖动
            value += self.memory[n] * self.noise_reduction / integral_mod;
            self.memory[n] = value;

            if self.autosens && value > 1.0 {
                overshoot = true;
                value = 1.0;
            }
            *slot = value.clamp(0.0, 1.0);
        }

        // autosens：顶格了就把整体增益压低一点，长期没顶格就缓慢抬高。
        // 单调的一点增益漂移，正是 cava「任何音量下都能铺满屏幕」的来源。
        if self.autosens {
            if overshoot {
                self.sens *= 1.0 - 0.02 * framerate_mod;
                self.sens_init = false;
            } else if samples.iter().any(|sample| *sample != 0.0) {
                self.sens *= 1.0 + 0.001 * framerate_mod;
                if self.sens_init {
                    self.sens *= 1.0 + 0.1 * framerate_mod;
                }
            }
        }

        &self.out
    }

    /// 停止播放时清零平滑历史，否则下一首开头的柱子会从上一首的形状往下掉。
    pub fn reset(&mut self) {
        self.fall.iter_mut().for_each(|value| *value = 0.0);
        self.memory.iter_mut().for_each(|value| *value = 0.0);
        self.peak.iter_mut().for_each(|value| *value = 0.0);
        self.prev.iter_mut().for_each(|value| *value = 0.0);
        self.input.iter_mut().for_each(|value| *value = 0.0);
        self.sens = 1.0;
        self.sens_init = true;
    }

    pub fn bars(&self) -> usize {
        self.bars
    }
}

/// 实数序列的幅度谱（正频率一半），写入调用方提供的缓冲，不分配。
///
/// 上游走 FFTW 的 r2c；这里把实部填进复变换的实部、虚部留 0，结果相同，
/// 代价是算了两倍的点数——在 4096/8192 这个量级上可忽略（实测 < 1ms）。
///
/// 缓冲长度对不上时返回 `None`（构造时按 `bass_size`/`mid_size` 定死，
/// 正常路径不会走到）。
fn magnitude_into<'a>(
    samples: &[f32],
    re: &'a mut [f32],
    im: &'a mut [f32],
    magnitudes: &'a mut [f32],
) -> Option<&'a [f32]> {
    let n = samples.len();
    if re.len() != n || im.len() != n || magnitudes.len() != n / 2 + 1 {
        return None;
    }
    re.copy_from_slice(samples);
    im.fill(0.0);
    fft(re, im);
    for (slot, index) in magnitudes.iter_mut().zip(0..=n / 2) {
        *slot = (re[index] * re[index] + im[index] * im[index]).sqrt();
    }
    Some(magnitudes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(frequency: f32, rate: u32, len: usize, amplitude: f32) -> Vec<f32> {
        (0..len)
            .map(|i| {
                amplitude * (2.0 * std::f32::consts::PI * frequency * i as f32 / rate as f32).sin()
            })
            .collect()
    }

    /// 喂够时长后，440Hz 应当点亮中低频那一侧，而不是随便哪根。
    ///
    /// 期望值来自本机 `libcava 1.0.0` 的实测输出（同样的输入序列）：
    /// 峰值在第 9 根、高度 0.0678。这里只断言位置，不断言绝对高度——
    /// autosens 的收敛曲线对浮点实现敏感，卡死具体数值会让测试变得又脆又没意义。
    #[test]
    fn a_pure_tone_lights_the_expected_band() {
        let mut cava = Cava::new(24, 44_100);
        let mut out = vec![0.0f32; 24];
        for _ in 0..80 {
            out = cava.execute(&sine(440.0, 44_100, 512, 0.5)).to_vec();
        }
        let peak = out
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .map(|(index, _)| index)
            .expect("总该有一根");

        assert!(
            (7..=12).contains(&peak),
            "440Hz 的峰值落在第 {peak} 根，不在预期区间（cava 1.0.0 实测为第 9 根）"
        );
        assert!(out[peak] > 0.02, "峰值应当有可见高度，实际 {}", out[peak]);
        // 低频那几根不该被 440Hz 点亮太多，否则分频没起作用
        assert!(
            out[0] < out[peak] / 4.0,
            "最低频段被误点亮：{} vs 峰值 {}",
            out[0],
            out[peak]
        );
    }

    /// 换一个频率，峰值也跟着换位置——证明不是恒定输出。
    #[test]
    fn different_frequencies_peak_at_different_bars() {
        let peak_of = |frequency: f32| {
            let mut cava = Cava::new(24, 44_100);
            let mut out = vec![0.0f32; 24];
            for _ in 0..80 {
                out = cava.execute(&sine(frequency, 44_100, 512, 0.5)).to_vec();
            }
            out.iter()
                .enumerate()
                .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
                .map(|(index, _)| index)
                .unwrap()
        };
        assert!(peak_of(300.0) < peak_of(6000.0), "低频的峰值必须比高频靠左");
    }

    /// 全零输入不能把输出顶到满格，也不能产生 NaN。
    #[test]
    fn silence_stays_at_zero_without_nan() {
        let mut cava = Cava::new(16, 44_100);
        let mut out = vec![0.0f32; 16];
        for _ in 0..50 {
            out = cava.execute(&vec![0.0f32; 512]).to_vec();
        }
        for value in &out {
            assert!(value.is_finite(), "输出必须有限，实际 {value}");
            assert!(*value <= 0.01, "静音不该亮起来，实际 {value}");
        }
    }

    /// autosens 的作用：响度很低的正弦也能被抬到看得见的高度。
    #[test]
    fn autosens_lifts_a_quiet_signal_into_view() {
        let mut cava = Cava::new(16, 44_100);
        let mut out = vec![0.0f32; 16];
        for _ in 0..600 {
            out = cava.execute(&sine(1000.0, 44_100, 512, 0.002)).to_vec();
        }
        let max = out.iter().fold(0.0f32, |max, value| max.max(*value));
        assert!(max > 0.2, "autosens 应当把极小信号抬起来，实际最高 {max}");
    }

    /// 断流（空输入）不应 panic，也不该把已有输出抹掉。
    #[test]
    fn empty_input_is_tolerated() {
        let mut cava = Cava::new(8, 44_100);
        let out = cava.execute(&[]).to_vec();
        assert_eq!(out.len(), 8);
        assert!(out.iter().all(|value| *value == 0.0));
    }

    /// 频段边界不能越界，且**同一窗口内**必须单调——这是 `plan_bands` 那段
    /// 「推频谱」逻辑的回归。
    ///
    /// 不跨窗口比大小：低频段（<100Hz）的 bin 下标是相对 8192 点窗口的，中高频段
    /// 是相对 4096 点窗口的，两者不可比。上游 cavacore 本身就不保证跨窗口单调
    /// （24 根、44.1kHz 下第 4 根 lower=12 而第 3 根是 18），照抄它才是对的。
    #[test]
    fn planned_bands_stay_in_range_and_monotonic_within_each_window() {
        for bars in [8usize, 24, 64, 128] {
            let cava = Cava::new(bars, 44_100);
            for n in 0..bars {
                let (low, high) = (cava.lower_cut_off[n], cava.upper_cut_off[n]);
                assert!(low <= high, "{bars} 根时第 {n} 段的边界反了：{low}..{high}");
                let limit = if n < cava.bass_cut_off_bar {
                    cava.bass_size / 2
                } else {
                    cava.mid_size / 2
                };
                assert!(
                    high <= limit,
                    "{bars} 根时第 {n} 段上界 {high} 超出窗口半宽 {limit}"
                );

                // 同一窗口内的相邻两段：后一段的下界不能比前一段更小
                if n > 0 && (n < cava.bass_cut_off_bar) == (n - 1 < cava.bass_cut_off_bar) {
                    assert!(
                        low >= cava.lower_cut_off[n - 1],
                        "{bars} 根时第 {n} 段的下界比前一段还小：{low} < {}",
                        cava.lower_cut_off[n - 1]
                    );
                }
            }
        }
    }

    /// 极短采样率下也不能 panic（`high_cut_off` 会被压到奈奎斯特以下）。
    #[test]
    fn an_unusual_sample_rate_does_not_panic() {
        let mut cava = Cava::new(8, 8_000);
        for _ in 0..20 {
            let out = cava.execute(&sine(440.0, 8_000, 256, 0.5)).to_vec();
            assert_eq!(out.len(), 8);
        }
    }
}
