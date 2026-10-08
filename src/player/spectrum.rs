//! 频谱分析 —— 给音频舞台的均衡器可视化供数。
//!
//! 数据流：cpal 输出回调 →（[`Tap`] 里滚动保留最近一窗）→ UI 线程每帧取走一窗，
//! 做一次 1024 点 FFT，按对数刻度归并成若干频段。
//!
//! 分工是刻意的：**回调只搬数据，FFT 放在 UI 线程**。输出回调有硬实时约束，
//! 宁可少几帧频谱，也不能让它在锁上等、或者多算几十微秒。

use std::f32::consts::TAU;
use std::sync::Mutex;

/// 分析窗长度。1024 点 @48kHz ≈ 21ms，频率分辨率 47Hz，对可视化足够灵敏。
pub const WINDOW: usize = 1024;

/// 频段的能量低于这个值就当没信号（走合成谱兜底）。
const SIGNAL: f32 = 0.02;

/// 响度映射的量程（dBFS）。满幅正弦（0 dBFS）正好顶满，−60 dB 以下视为无声。
const DB_MIN: f32 = -60.0;
const DB_MAX: f32 = 0.0;

/// Hann 窗的相干增益，用来把归一化幅度换算回真实幅度。
const HANN_GAIN: f32 = 0.5;

/* -------------------------------------------------------------------------- */
/* 采样搬运                                                                    */
/* -------------------------------------------------------------------------- */

/// 音频线程 → UI 线程的采样窗。
///
/// 音频线程只写、UI 线程只读，靠一把 `Mutex` 串起来；两边都用不会阻塞的拿法，
/// 拿不到就认输 —— 对可视化来说丢一两帧数据毫无代价。
pub struct Tap {
    inner: Mutex<Inner>,
}

struct Inner {
    buf: Vec<f32>,
    /// 下一个写入位置
    cursor: usize,
    /// 窗里攒了多少（上限 [`WINDOW`]）
    filled: usize,
}

impl Tap {
    pub fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            inner: Mutex::new(Inner {
                buf: vec![0.0; WINDOW],
                cursor: 0,
                filled: 0,
            }),
        })
    }

    /// 音频回调里调用。`try_lock` 拿不到就整批丢掉 —— 输出回调绝不能等锁，
    /// 少几帧频谱没人看得出来，卡一下就是爆音。
    pub fn push(&self, mono: &[f32]) {
        let Ok(mut inner) = self.inner.try_lock() else {
            return;
        };
        // 万一一次塞进来的比整窗还长，只留最后一段
        let mut src = if mono.len() > WINDOW {
            &mono[mono.len() - WINDOW..]
        } else {
            mono
        };
        while !src.is_empty() {
            let at = inner.cursor;
            let n = (WINDOW - at).min(src.len());
            inner.buf[at..at + n].copy_from_slice(&src[..n]);
            inner.cursor = (at + n) % WINDOW;
            inner.filled = (inner.filled + n).min(WINDOW);
            src = &src[n..];
        }
    }

    /// UI 线程取最近一窗，按时间先后写进 `out`。
    ///
    /// 返回窗里真正攒到的采样数；不够 `out.len()` 的部分在前面补零
    /// （相当于给这段波形加了一段静音引子）。
    pub fn snapshot(&self, out: &mut [f32]) -> usize {
        let Ok(inner) = self.inner.lock() else {
            return 0;
        };
        let n = inner.filled.min(out.len());
        let pad = out.len() - n;
        out[..pad].fill(0.0);
        // 最新的 n 个：从写指针往回数 n 个
        let start = (inner.cursor + WINDOW - n) % WINDOW;
        for (i, slot) in out[pad..].iter_mut().enumerate() {
            *slot = inner.buf[(start + i) % WINDOW];
        }
        n
    }

    /// 换文件 / 停止时清空，别把上一段的波形留下。
    pub fn clear(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.cursor = 0;
            inner.filled = 0;
        }
    }
}

/* -------------------------------------------------------------------------- */
/* 分析                                                                        */
/* -------------------------------------------------------------------------- */

/// 复用的 FFT 工作区：窗函数与旋转因子在构造时算好，之后每帧零分配。
pub struct Analyzer {
    n: usize,
    hann: Vec<f32>,
    tw_re: Vec<f32>,
    tw_im: Vec<f32>,
    re: Vec<f32>,
    im: Vec<f32>,
    mag: Vec<f32>,
}

impl Analyzer {
    pub fn new() -> Self {
        let n = WINDOW;
        let hann = (0..n)
            .map(|i| 0.5 - 0.5 * (TAU * i as f32 / (n - 1) as f32).cos())
            .collect();
        let tw_re = (0..n / 2)
            .map(|k| (-TAU * k as f32 / n as f32).cos())
            .collect();
        let tw_im = (0..n / 2)
            .map(|k| (-TAU * k as f32 / n as f32).sin())
            .collect();
        Self {
            n,
            hann,
            tw_re,
            tw_im,
            re: vec![0.0; n],
            im: vec![0.0; n],
            mag: vec![0.0; n / 2],
        }
    }

    /// 一窗采样 → `out.len()` 个 0–1 的频段能量。
    ///
    /// 采样短于 [`WINDOW`] 时按前面补零处理；`rate` 只影响频段的边界，不影响算法。
    pub fn bands(&mut self, samples: &[f32], rate: f32, out: &mut [f32]) {
        let _ = rate; // 频段边界由 [`band_hz`] 换算，分析本身与采样率无关
        let n = self.n;
        let take = samples.len().min(n);
        let pad = n - take;
        let off = samples.len() - take;
        for i in 0..n {
            let s = if i < pad { 0.0 } else { samples[off + i - pad] };
            self.re[i] = s * self.hann[i];
            self.im[i] = 0.0;
        }

        fft(&mut self.re, &mut self.im, &self.tw_re, &self.tw_im);

        let bins = n / 2;
        // Hann 窗把峰值压掉一半，这里乘回去，满幅正弦正好落在 0dB
        let scale = 2.0 / (n as f32 * HANN_GAIN);
        for k in 0..bins {
            let (r, i) = (self.re[k], self.im[k]);
            self.mag[k] = (r * r + i * i).sqrt() * scale;
        }

        let count = out.len();
        for (i, slot) in out.iter_mut().enumerate() {
            let (a, b) = band_bins(i, count, bins);
            let (mut sum, mut peak, mut cnt) = (0.0f32, 0.0f32, 0usize);
            for k in a..b {
                let m = self.mag[k];
                sum += m;
                peak = peak.max(m);
                cnt += 1;
            }
            // 高频在几十个近乎静音的 bin 上被摊薄，所以均值与峰值混着用
            let v = if cnt == 0 {
                0.0
            } else {
                (sum / cnt as f32) * 0.42 + peak * 0.58
            };
            // 高频抬升放在 dB 域做（最高 +9 dB 的二次曲线）：线性域乘增益会把
            // 主峰两边的泄漏瓣一起放大到饱和，整条谱糊成一片。
            let pos = if count <= 1 {
                0.0
            } else {
                i as f32 / (count - 1) as f32
            };
            let db = 20.0 * v.max(1e-6).log10() + 9.0 * pos * pos;
            let level = ((db - DB_MIN) / (DB_MAX - DB_MIN)).clamp(0.0, 1.0);
            // 轻微提亮中低段，让安静的曲子也有起伏
            *slot = level.powf(0.72);
        }
    }
}

/// 频段铺在音乐真正用到的低中频上：bin 1 到约 40% 处（48kHz 下 ≈ 9.6kHz），
/// 对数刻度 —— 和原版 `viz.js` 的取法一致。返回（最高 bin，公比）。
fn band_layout(bands: usize, bins: usize) -> (usize, f32) {
    let top = bins.saturating_sub(1).max(2);
    let hi = ((bins as f32 * 0.4).round() as usize).clamp(16.min(top), top);
    (hi, (hi as f32).powf(1.0 / bands.max(1) as f32))
}

/// 第 `i` 个频段覆盖的 bin 区间（左闭右开）。
fn band_bins(i: usize, bands: usize, bins: usize) -> (usize, usize) {
    let (_, ratio) = band_layout(bands, bins);
    let a = ratio.powi(i as i32).floor() as usize;
    let b = (ratio.powi(i as i32 + 1).floor() as usize).max(a + 1);
    (a.clamp(1, bins), b.min(bins))
}

/// 第 `i` 个频段覆盖的频率区间（Hz）。给测试和调试用。
pub fn band_hz(i: usize, bands: usize, rate: f32) -> (f32, f32) {
    let (a, b) = band_bins(i, bands, WINDOW / 2);
    let hz = rate / WINDOW as f32;
    (a as f32 * hz, b as f32 * hz)
}

/// 原地 radix-2 FFT。`re` / `im` 长度必须是 2 的幂，`tw_*` 是 `exp(-2πi k/n)`。
fn fft(re: &mut [f32], im: &mut [f32], tw_re: &[f32], tw_im: &[f32]) {
    let n = re.len();
    // 位反转置换
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
        let step = n / len;
        let mut base = 0;
        while base < n {
            for k in 0..half {
                let (cr, ci) = (tw_re[k * step], tw_im[k * step]);
                let (ur, ui) = (re[base + k], im[base + k]);
                let (xr, xi) = (re[base + k + half], im[base + k + half]);
                let vr = xr * cr - xi * ci;
                let vi = xr * ci + xi * cr;
                re[base + k] = ur + vr;
                im[base + k] = ui + vi;
                re[base + k + half] = ur - vr;
                im[base + k + half] = ui - vi;
            }
            base += len;
        }
        len <<= 1;
    }
}

/// 判定"这一窗到底有没有声音"，给可视化决定是否切兜底谱用。
pub fn is_silent(bands: &[f32]) -> bool {
    bands.iter().copied().fold(0.0f32, f32::max) <= SIGNAL
}

/* -------------------------------------------------------------------------- */
/* 测试                                                                        */
/* -------------------------------------------------------------------------- */

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f32, rate: f32, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| (TAU * freq * i as f32 / rate).sin())
            .collect()
    }

    /// 窗里永远只剩最新的 [`WINDOW`] 个采样，且顺序正确。
    #[test]
    fn tap_keeps_the_latest_window() {
        let tap = Tap::new();
        let data: Vec<f32> = (0..3000).map(|i| i as f32).collect();
        tap.push(&data);

        let mut out = [0.0f32; WINDOW];
        assert_eq!(tap.snapshot(&mut out), WINDOW);
        assert_eq!(out[0], 1976.0, "窗首应当是最新 1024 个里的第一个");
        assert_eq!(out[WINDOW - 1], 2999.0, "窗尾应当是最新那个采样");
    }

    /// 数据不足一窗时，前面补零、尾部是刚写进去的采样。
    #[test]
    fn tap_pads_a_short_window_in_front() {
        let tap = Tap::new();
        tap.push(&[1.0, 2.0, 3.0]);

        let mut out = [9.0f32; WINDOW];
        assert_eq!(tap.snapshot(&mut out), 3);
        assert!(out[..WINDOW - 3].iter().all(|v| *v == 0.0));
        assert_eq!(&out[WINDOW - 3..], &[1.0, 2.0, 3.0]);
    }

    /// 跨多批写入时环形缓冲要接得上，不能丢也不能乱序。
    #[test]
    fn tap_wraps_across_pushes() {
        let tap = Tap::new();
        for round in 0..40u32 {
            let chunk: Vec<f32> = (0..300).map(|i| (round * 300 + i) as f32).collect();
            tap.push(&chunk);
        }
        // 一共写了 12000 个，窗里应当正好是 10976..12000
        let mut out = [0.0f32; WINDOW];
        assert_eq!(tap.snapshot(&mut out), WINDOW);
        assert_eq!(out[0], 10976.0);
        assert_eq!(out[WINDOW - 1], 11999.0);
        assert!(out.windows(2).all(|w| w[1] > w[0]), "顺序不该乱");
    }

    #[test]
    fn clearing_drops_everything() {
        let tap = Tap::new();
        tap.push(&vec![1.0; WINDOW]);
        tap.clear();
        let mut out = [0.0f32; WINDOW];
        assert_eq!(tap.snapshot(&mut out), 0);
    }

    #[test]
    fn silence_is_flat() {
        let mut an = Analyzer::new();
        let mut out = [1.0f32; 32];
        an.bands(&[0.0; WINDOW], 48_000.0, &mut out);
        assert!(
            out.iter().all(|v| *v == 0.0),
            "静音时每一段都该是 0，实际 {:?}",
            &out[..6]
        );
        assert!(is_silent(&out));
    }

    /// 1kHz 的正弦必须落在覆盖它的那一段，而且那一段要接近满值。
    #[test]
    fn a_tone_lands_in_the_band_that_covers_it() {
        let rate = 48_000.0f32;
        let mut an = Analyzer::new();
        let mut out = [0.0f32; 32];
        an.bands(&sine(1000.0, rate, WINDOW), rate, &mut out);

        let (loudest, peak) = out
            .iter()
            .enumerate()
            .fold((0usize, -1.0f32), |acc, (i, v)| {
                if *v > acc.1 { (i, *v) } else { acc }
            });
        let (lo, hi) = band_hz(loudest, 32, rate);
        assert!(
            lo <= 1000.0 && 1000.0 < hi,
            "1kHz 应当落在第 {loudest} 段（{lo:.0}–{hi:.0}Hz）"
        );
        assert!(peak > 0.5, "满幅正弦的频段值不该这么低：{peak:.3}");
        assert!(!is_silent(&out));
    }

    /// 噪声打满时所有频段都得在 0–1 之间，不能溢出也不能全趴着。
    #[test]
    fn busy_signal_stays_in_range() {
        let rate = 44_100.0f32;
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 40) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0
        };
        let noise: Vec<f32> = (0..WINDOW).map(|_| next()).collect();

        let mut an = Analyzer::new();
        let mut out = [0.0f32; 48];
        an.bands(&noise, rate, &mut out);
        assert!(out.iter().all(|v| (0.0..=1.0).contains(v)), "{out:?}");
        assert!(out.iter().any(|v| *v > 0.1), "白噪声不该是一条平线");
    }

    /// 频段边界要单调递增、都落在有效 bin 里。
    #[test]
    fn band_edges_are_monotonic() {
        let bins = WINDOW / 2;
        for count in [20usize, 48, 72] {
            let mut last = 0usize;
            for i in 0..count {
                let (a, b) = band_bins(i, count, bins);
                assert!(a >= 1 && a < b && b <= bins, "第 {i} 段越界：{a}..{b}");
                assert!(a >= last, "第 {i} 段起点倒退了");
                last = a;
            }
        }
    }
}
