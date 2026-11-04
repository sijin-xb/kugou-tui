//! FFT 与频谱参数的公共部分。
//!
//! 算法本体在 [`crate::audio::cava`]，这里只留两样东西：
//!
//! * [`fft`] —— 原地 radix-2 复变换，cava 的小/大两个窗口都用它；
//! * [`BAND_COUNT`] / [`WINDOW_SIZE`] —— 采样窗与柱数的共享常量。
//!
//! 早先这里还有一个自己写的「FFT + 对数分频 + dB 归一」的 `analyze`。它被
//! cava 移植取代了：两者做的是同一件事，但 cava 的 autosens 与三段平滑在
//! 实际观感上明显更好，留两套只会让「为什么屏幕上和配置文件对不上」变成
//! 一个新的排查方向。

/// 环形采样窗的容量。
///
/// cava 的低频段用 `2 * base_fft_size` 点的内部缓冲，44.1kHz 下 `base` 是
/// 4096，也就是 16384 点。窗口至少要有这么大，否则低频那段会一直是半截旧数据。
/// 96kHz / 192kHz 下 cava 内部需要更多，但那些采样率下每帧新增的采样也多，
/// 取 32768 对常见帧率（20~60fps）都够用；不够时 `take_since` 会截断，
/// 表现只是低频窗口少喂一点，不会错位。
pub const WINDOW_SIZE: usize = 32_768;

/// 频段数。渲染时再按实际列数聚合，这里取足够细的分辨率即可。
///
/// 128 而不是 64：cava 的分频是逐段规划 bin 的，柱数越多低频那几根越窄、
/// 越能分开贝斯与底鼓。渲染侧本来就会按终端宽度聚合，多给的段不会浪费。
pub const BAND_COUNT: usize = 128;

/// 原地 radix-2 FFT。`re` / `im` 长度相同且必须是 2 的幂。
///
/// 长度不合要求时直接返回（不 panic）：这是显示用的旁路数据，
/// 不该因为它把播放搞崩。
pub fn fft(re: &mut [f32], im: &mut [f32]) {
    let n = re.len();
    if n != im.len() || !n.is_power_of_two() {
        return;
    }

    // 位反转置换：把输入重排成迭代版 FFT 需要的顺序，省掉递归
    let mut j = 0usize;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }

    let mut len = 2;
    while len <= n {
        let half = len / 2;
        // 旋转因子 w = e^(-2πi/len)，取负号对应正向变换
        let step = -2.0 * std::f32::consts::PI / len as f32;
        let mut start = 0;
        while start < n {
            for k in 0..half {
                let (sin, cos) = (step * k as f32).sin_cos();
                let a = start + k;
                let b = a + half;
                let tr = re[b] * cos - im[b] * sin;
                let ti = re[b] * sin + im[b] * cos;
                re[b] = re[a] - tr;
                im[b] = im[a] - ti;
                re[a] += tr;
                im[a] += ti;
            }
            start += len;
        }
        len <<= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(frequency: f32, sample_rate: u32, len: usize) -> Vec<f32> {
        (0..len)
            .map(|i| {
                0.8 * (2.0 * std::f32::consts::PI * frequency * i as f32 / sample_rate as f32).sin()
            })
            .collect()
    }

    #[test]
    fn fft_of_dc_lands_entirely_in_bin_zero() {
        let n = 8;
        let mut re = vec![1.0f32; n];
        let mut im = vec![0.0f32; n];
        fft(&mut re, &mut im);

        assert!((re[0] - n as f32).abs() < 1e-4, "直流分量 = 所有采样之和");
        for i in 1..n {
            assert!(
                re[i].abs() < 1e-4 && im[i].abs() < 1e-4,
                "其余 bin 应当为零"
            );
        }
    }

    #[test]
    fn fft_puts_a_pure_tone_in_its_own_bin() {
        let n = 8;
        let sample_rate = 32;
        let mut re = sine(4.0, sample_rate, n);
        let mut im = vec![0.0f32; n];
        fft(&mut re, &mut im);

        let magnitude = (re[1] * re[1] + im[1] * im[1]).sqrt();
        assert!(
            (magnitude - 0.8 * n as f32 / 2.0).abs() < 1e-3,
            "bin 1 应当是主峰，实际 {magnitude}"
        );
        let off_peak = (re[3] * re[3] + im[3] * im[3]).sqrt();
        assert!(off_peak < 1e-3, "bin 3 不该有能量，实际 {off_peak}");
    }

    #[test]
    fn fft_ignores_malformed_input() {
        let mut re = vec![1.0f32; 6];
        let mut im = vec![0.0f32; 6];
        fft(&mut re, &mut im);
        assert_eq!(re, vec![1.0f32; 6], "输入应原样保留");
    }
}
