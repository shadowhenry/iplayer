//! 音频舞台的“均衡器”可视化 —— 旧 Tauri 版 viz.js 的移植。
//!
//! 底部频谱柱（快起慢落 + 峰值帽）+ 上浮渐隐的音符粒子。频谱来自音频内核
//! 的 `Tap`（cpal 回调注入的单声道样本）；连续多帧读不到真实信号时退回
//! 程序化合成频谱，保证“均衡器”永远不是一块死黑。
//!
//! 画布换成了 GPUI div：柱与帽用绝对定位的小块，音符用带透明度的文本。
//! 旧版 canvas 的竖向渐变条在这个 gpui-pre 里没有对应 API，按黑白规范
//! 简化为实心墨水色（详见 render 处注释）。

use gpui_kit::gpui::{div, px, relative, AnyElement, Hsla, SharedString};
use gpui_kit::{IntoElement as _, ParentElement as _, Styled as _};

use crate::player::spectrum::{self, Analyzer, Tap};

/// 音符字符池，同旧版 ♪ ♫ ♬ ♩
const GLYPHS: [char; 4] = ['♪', '♫', '♬', '♩'];

/// 连续这么多帧没有真实信号才切合成频谱（旧版 60 帧 ≈ 1s）
const FALLBACK_AFTER: u32 = 60;

/// 每 ~15px 一根柱，收敛在 20–72 之间（旧版 resize 规则）
pub fn band_count_for_width(w: f32) -> usize {
    if w <= 1.0 {
        return 24;
    }
    ((w / 15.0).round() as usize).clamp(20, 72)
}

/// 同屏音符上限：按舞台宽给量，至少 12 个。
pub fn max_notes(w: f32) -> usize {
    ((w / 70.0).round() as usize).max(12)
}

struct Note {
    glyph: char,
    /// 横向占比 0..1（相对舞台宽）
    x: f32,
    /// 纵向占比（相对舞台高，1.0 = 底缘）
    y: f32,
    /// 字号/占位，px
    size: f32,
    alpha: f32,
    /// 每秒上浮的纵向占比
    vy: f32,
    wob: f32,
    wob_speed: f32,
    phase: f32,
}

/// 可视化状态机。`step` 每帧推进（由 App::tick 驱动），`render` 出绘制树。
pub struct Viz {
    w: f32,
    h: f32,
    levels: Vec<f32>,
    targets: Vec<f32>,
    peaks: Vec<f32>,
    notes: Vec<Note>,
    clock: f32,
    spawn_acc: f32,
    playing: bool,
    frames: u32,
    saw_signal: bool,
    fallback: bool,
    analyzer: Analyzer,
    rng: u32,
}

impl Viz {
    pub fn new() -> Self {
        // xorshift 种子取时钟纳米，避免每次启动音符布局完全一样
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as u32 ^ (d.as_secs() as u32))
            .unwrap_or(0x9E37_79B9)
            | 1;
        Self {
            w: 0.0,
            h: 0.0,
            levels: Vec::new(),
            targets: Vec::new(),
            peaks: Vec::new(),
            notes: Vec::new(),
            clock: 0.0,
            spawn_acc: 0.0,
            playing: false,
            frames: 0,
            saw_signal: false,
            fallback: false,
            analyzer: Analyzer::new(),
            rng: nanos,
        }
    }

    /// 换文件 / 停止 / 离开音频舞台时清场
    pub fn reset(&mut self) {
        self.levels.clear();
        self.targets.clear();
        self.peaks.clear();
        self.notes.clear();
        self.clock = 0.0;
        self.spawn_acc = 0.0;
        self.frames = 0;
        self.saw_signal = false;
        self.fallback = false;
    }

    /// 每帧推进。`src` 是播放器给的（Tap, 采样率）；None 表示暂无音频轨。
    pub fn step(
        &mut self,
        dt: f32,
        w: f32,
        h: f32,
        playing: bool,
        audible: bool,
        src: Option<(&Tap, f32)>,
    ) {
        self.w = w;
        self.h = h;
        self.playing = playing;
        let n = band_count_for_width(w);
        if self.levels.len() != n {
            self.levels.resize(n, 0.0);
            self.targets.resize(n, 0.0);
            self.peaks.resize(n, 0.0);
        }
        self.clock += dt;

        if !playing {
            // 暂停：目标归零，柱子慢慢落回地面，峰值帽跟着滑下来
            for i in 0..n {
                self.levels[i] += (0.0 - self.levels[i]) * (dt * 3.2).min(1.0);
                self.peaks[i] = self.levels[i].max(self.peaks[i] - dt * 0.9);
            }
            self.spawn_acc = 0.0;
            self.step_notes(dt);
            return;
        }

        // 真实信号：快照窗口 → FFT 分带。读不到样本或整窗死寂都按“没信号”计数。
        let mut live = false;
        if let Some((tap, rate)) = src {
            let mut buf = [0.0f32; spectrum::WINDOW];
            let got = tap.snapshot(&mut buf);
            if got > 0 {
                let silent = {
                    let Self {
                        analyzer, targets, ..
                    } = self;
                    analyzer.bands(&buf, rate, targets);
                    spectrum::is_silent(targets)
                };
                live = !silent;
            }
        }
        if live {
            self.saw_signal = true;
            self.frames = 0;
        } else {
            self.frames += 1;
        }
        // 旧版：压根没有分析器 → 直接合成；有分析器但连续 60 帧没信号 → 兜底
        self.fallback = audible
            && (src.is_none() || (!self.saw_signal && self.frames > FALLBACK_AFTER));

        if !audible {
            for t in &mut self.targets {
                *t = 0.0;
            }
        } else if self.fallback {
            self.read_synthetic();
        }

        // 快攻慢放 —— 这是“像音乐”而不是“像频闪”的关键（旧版 14 / 3.6）
        for i in 0..n {
            let t = self.targets[i];
            let k = if t > self.levels[i] {
                (dt * 14.0).min(1.0)
            } else {
                (dt * 3.6).min(1.0)
            };
            self.levels[i] += (t - self.levels[i]) * k;
            self.peaks[i] = self.levels[i].max(self.peaks[i] - dt * 0.55);
        }

        self.step_notes(dt);
    }

    /// 程序化合成频谱（旧版 readSynthetic）—— 多重正弦叠加出“音乐感”起伏
    fn read_synthetic(&mut self) {
        let t = self.clock;
        let n = self.levels.len();
        for (i, tgt) in self.targets.iter_mut().enumerate().take(n) {
            let pos = i as f32 / (n as f32 - 1.0).max(1.0);
            let w1 = 0.5 + 0.5 * (t * 1.9 + i as f32 * 0.55).sin();
            let w2 = 0.5 + 0.5 * (t * 3.7 + i as f32 * 1.7).sin();
            let beat = 0.5 + 0.5 * (t * 5.3).sin();
            let swell =
                0.35 + 0.65 * (0.5 + 0.5 * (t * 0.31 + i as f32 * 0.07).sin()).powf(1.6);
            let tilt = 1.18 - 0.5 * pos;
            *tgt = (swell * (0.42 * w1 + 0.34 * w2 + 0.24 * beat) * tilt * 1.45).min(1.0);
        }
    }

    fn step_notes(&mut self, dt: f32) {
        if self.w <= 1.0 {
            return;
        }
        let max = max_notes(self.w);
        if self.playing {
            // 播放时更密一些（约 2.2–3.6 个/秒），音符从底部往上飘
            self.spawn_acc += dt * (2.2 + self.rand() * 1.4);
            while self.spawn_acc >= 1.0 {
                self.spawn_acc -= 1.0;
                if self.notes.len() >= max {
                    break;
                }
                let nt = self.new_note(false);
                self.notes.push(nt);
            }
        }
        let h = self.h.max(1.0);
        self.notes.retain_mut(|nt| {
            nt.y += nt.vy * dt;
            nt.phase += nt.wob_speed * dt;
            // 旧版剔除：飘出顶部一截 or 沉到视口下缘更远处（占比坐标）
            nt.y >= -(nt.size * 1.6) / h && nt.y <= 1.0 + (0.5f32).max(nt.size * 3.0 / h)
        });
    }

    fn new_note(&mut self, seeded: bool) -> Note {
        let g = GLYPHS[(self.rand() * GLYPHS.len() as f32) as usize % GLYPHS.len()];
        // 尺寸随机跨度拉大：小到 12px、大到舞台高的 20%
        let size = (self.h * (0.04 + self.rand() * 0.16)).clamp(12.0, self.h * 0.20);
        let h = self.h.max(1.0);
        Note {
            glyph: g,
            x: 0.06 + self.rand() * 0.88,
            y: if seeded {
                0.1 + self.rand() * 0.9
            } else {
                // 从下缘之下起步，往上飘进画面
                1.0 + size * 0.6 / h
            },
            size,
            alpha: 0.13 + self.rand() * 0.24,
            vy: -(0.045 + self.rand() * 0.09),
            wob: 5.0 + self.rand() * 22.0,
            wob_speed: 0.4 + self.rand() * 0.9,
            phase: self.rand() * std::f32::consts::TAU,
        }
    }

    /// xorshift32 → 0..1
    fn rand(&mut self) -> f32 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.rng = x;
        (x >> 8) as f32 / 16_777_216.0
    }

    /// 暂停后柱子和音符都归位了就没必要再刷帧
    pub fn settled(&self) -> bool {
        !self.playing
            && self.notes.is_empty()
            && self.levels.iter().all(|&l| l < 0.004)
            && self.peaks.iter().all(|&p| p < 0.004)
    }

    /// 供 App::tick 决定要不要继续要帧
    pub fn busy(&self) -> bool {
        !self.settled()
    }

    /// 绘制树：音符层在下、频谱层在上。中央那行曲名由调用方
    /// （`app::App::now_playing`）另外叠在最上面，本层不掺和。
    /// `ink` 是墨水色（黑白规范下的 pal.text()）。
    pub fn render(&self, ink: Hsla) -> AnyElement {
        let mut overlay = div().absolute().size_full().overflow_hidden();

        // ── 音符层（旧版 drawNotes；canvas 的竖向渐变简化为整体透明度）──
        // x/y 是占比，乘上 step 时量到的舞台尺寸还原成 px
        for nt in &self.notes {
            let denom = (1.0 + nt.size * 3.0 / self.h.max(1.0)).max(0.001);
            let prog = (1.0 + nt.size / self.h.max(1.0) - nt.y) / denom;
            if prog <= 0.0 || prog >= 1.0 {
                continue;
            }
            let a = nt.alpha * (std::f32::consts::PI * prog).sin();
            if a <= 0.004 {
                continue;
            }
            let x = nt.x * self.w + nt.phase.sin() * nt.wob;
            overlay = overlay.child(
                div()
                    .absolute()
                    .left(px(x - nt.size))
                    .top(px(nt.y * self.h - nt.size * 0.5))
                    .w(px(nt.size * 2.0))
                    .h(px(nt.size))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(nt.size))
                    .text_color(ink.opacity(a))
                    .child(SharedString::from(nt.glyph.to_string())),
            );
        }

        // ── 频谱层（旧版 drawBars）── 容器全比例定位，窗口尺寸变化自适应
        let n = self.levels.len();
        if n > 0 && self.w > 1.0 && self.h > 1.0 {
            let gap = (self.w * 0.0045).max(2.0);
            let bw = ((self.w * 0.95 - gap * (n - 1) as f32) / n as f32).max(2.0);
            // 柱身只占自己那一格约 46% 宽，柱子更细
            let inset = (bw * 0.27).max(1.0);
            let r = (bw * 0.24).min(3.0);
            let max_h = self.h * 0.52;
            let min_h = bw * 0.5;

            let mut bars = div()
                .absolute()
                .left(relative(0.025))
                .right(relative(0.025))
                // 离底缘的距离（用户要求缩短一倍：0.14 → 0.07）
                .bottom(relative(0.07))
                .h(relative(0.52))
                .flex()
                .items_end()
                .gap(px(gap));

            for i in 0..n {
                let level = self.levels[i];
                let peak = self.peaks[i];
                let h_frac = (min_h / max_h).max(level);
                // 柔光晕（旧版先铺一层 alpha 0.07 的加宽柱）
                let halo = div()
                    .absolute()
                    .left(px(inset - 2.0))
                    .right(px(inset - 2.0))
                    .bottom(px(0.0))
                    .h(relative(h_frac))
                    .rounded_t(px(r + 2.0))
                    .bg(ink.opacity(0.07));
                // 柱身：旧版是 0.14→0.92 竖向渐变，这里没有渐变 API，
                // 折中为 0.82 实心（视觉上仍明显压得住 0.5 的峰值帽）
                let bar = div()
                    .absolute()
                    .left(px(inset))
                    .right(px(inset))
                    .bottom(px(0.0))
                    .h(relative(h_frac))
                    .rounded_t(px(r))
                    .bg(ink.opacity(0.82));
                // 峰值帽（在柱容器内，cap_frac 是相对柱容器高度的占比；
                // 2.5px 的悬浮间隙按估计的 max_h 折成占比）
                let cap_frac = ((min_h / max_h).max(peak) + 2.5 / max_h).min(1.0);
                let cap = div()
                    .absolute()
                    .left(px(inset))
                    .right(px(inset))
                    .bottom(relative(cap_frac))
                    .h(px(2.5))
                    .rounded(px(1.25))
                    .bg(ink.opacity(0.5));

                bars = bars.child(
                    div()
                        .relative()
                        .flex_1()
                        .min_w(px(0.0))
                        .h_full()
                        .child(halo)
                        .child(bar)
                        .child(cap),
                );
            }
            overlay = overlay.child(bars);
        }

        overlay.into_any_element()
    }
}

#[cfg(test)]
mod tests;
