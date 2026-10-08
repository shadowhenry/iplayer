//! viz.rs 的回归测试。几何用比例坐标，频谱走真 Tap 与合成两条路。

use super::*;
use crate::player::spectrum::Tap;

const DT: f32 = 1.0 / 60.0;
const W: f32 = 960.0;
const H: f32 = 600.0;

/// 旧版 resize 规则：w/15 根柱，20–72 收敛。
#[test]
fn band_count_follows_width() {
    assert_eq!(band_count_for_width(100.0), 20);
    assert_eq!(band_count_for_width(320.0), 21);
    assert_eq!(band_count_for_width(960.0), 64);
    assert_eq!(band_count_for_width(4000.0), 72);
    assert_eq!(band_count_for_width(0.0), 24, "未量到尺寸时给个合理默认");
}

/// 没接 Tap（或一直读不到信号）时必须切到合成频谱，柱子不能是死的。
#[test]
fn falls_back_to_synthetic_without_tap() {
    let mut v = Viz::new();
    // 61 帧之后 fallback 生效，再给攻击留几帧
    for _ in 0..80 {
        v.step(DT, W, H, true, true, None);
    }
    assert!(v.fallback, "长时间无信号应当切合成频谱");
    assert!(
        v.levels.iter().any(|&l| l > 0.08),
        "合成频谱应当推起可见的柱子，实际 max={:?}",
        v.levels.iter().cloned().fold(0.0f32, f32::max)
    );
}

/// 真 Tap + 1kHz 正弦：能量应当落进对应频带，而不是一片死平。
#[test]
fn real_tap_sine_lands_in_band() {
    let tap = Tap::new();
    let rate = 48_000.0;
    // 喂 ~4096 个样本（几个窗口），1kHz 正弦，幅度 0.5
    let mut phase = 0.0f32;
    for _ in 0..64 {
        let mono: Vec<f32> = (0..64)
            .map(|_| {
                let s = (phase * std::f32::consts::TAU).sin() * 0.5;
                phase += 1000.0 / rate;
                s
            })
            .collect();
        tap.push(&mono);
    }

    let mut v = Viz::new();
    for _ in 0..30 {
        v.step(DT, W, H, true, true, Some((tap.as_ref(), rate)));
    }
    assert!(!v.saw_signal == false, "正弦应当被识别为真实信号");
    let peak_band = v
        .levels
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1))
        .map(|(i, &l)| (i, l))
        .unwrap();
    assert!(
        peak_band.1 > 0.3,
        "1kHz 正弦应当推起明显的柱子，实际 {peak_band:?}"
    );
    // 主峰带对应的频率区间必须盖住 1kHz（64 带、对数铺到 ~9.6kHz）
    let (lo, hi) = crate::player::spectrum::band_hz(peak_band.0, 64, 48_000.0);
    assert!(
        lo <= 1000.0 && hi >= 1000.0,
        "1kHz 主峰带位置异常：带 {} 覆盖 {lo:.0}–{hi:.0} Hz",
        peak_band.0
    );
}

/// 暂停后必须能收敛到 settled（tick 据此停掉帧循环）。
#[test]
fn settles_after_pause() {
    let mut v = Viz::new();
    for _ in 0..90 {
        v.step(DT, W, H, true, true, None);
    }
    assert!(v.busy());

    // 慢音符（vy≈0.035/s）从底部飘到顶要 ~30s —— 与旧版一致，暂停后动画
    // 还会空转一段，直到最后一个音符离场才 settled
    for _ in 0..2400 {
        v.step(DT, W, H, false, true, None);
    }
    assert!(v.settled(), "暂停 40 秒后应当完全归位");
}

/// 静音时柱子应当归零（音频流还在，但用户不想看摆动）。
#[test]
fn mute_flattens_bars() {
    let mut v = Viz::new();
    for _ in 0..60 {
        v.step(DT, W, H, true, true, None);
    }
    // 慢释放（dt*3.6）下需要一点时间才落到底
    for _ in 0..120 {
        v.step(DT, W, H, true, false, None);
    }
    assert!(
        v.levels.iter().all(|&l| l < 0.05),
        "静音后柱子应当落下去"
    );
}

/// 播放时音符应当持续生成并被正确剔除（数量有上限）。
#[test]
fn notes_spawn_and_cull() {
    let mut v = Viz::new();
    for _ in 0..120 {
        v.step(DT, W, H, true, true, None);
    }
    let n_playing = v.notes.len();
    assert!(n_playing > 0, "播放时应当有音符");
    assert!(n_playing <= ((W / 95.0).round() as usize).max(8), "音符数量不该超上限");

    for _ in 0..2400 {
        v.step(DT, W, H, false, true, None);
    }
    assert!(v.notes.is_empty(), "暂停后存量音符应当全部飘完离场");
}
