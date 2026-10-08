//! 播放时间轴。
//!
//! 有音频时以「音频实际播出的采样数」为准（音频钟），没有音频时用墙钟。
//! 视频帧的呈现只做一件事：把 pts <= 当前时间 的最新一帧显示出来，
//! 所以时钟是唯一的权威，视频落后就丢帧、超前就等待。

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// 音频位置。cpal 的回调每消费一批采样就更新一次，音频钟由此得到。
#[derive(Default)]
pub struct AudioPosition {
    /// 已播出的帧数（每声道）
    samples: AtomicU64,
    /// 采样率（每声道）
    rate: AtomicU32,
    /// 这一段的起始位置（秒）
    base_bits: AtomicU64,
    /// 倍速。ffmpeg 用 `atempo` 把音频压/拉过，所以真实播出的 N 秒
    /// 对应媒体时间的 `N × speed` 秒。
    speed_bits: AtomicU64,
}

impl AudioPosition {
    pub fn new(rate: u32, base: f64) -> Arc<Self> {
        let me = Arc::new(Self::default());
        me.rate.store(rate, Ordering::Relaxed);
        me.base_bits.store(base.to_bits(), Ordering::Relaxed);
        me.speed_bits.store(1.0f64.to_bits(), Ordering::Relaxed);
        me
    }

    pub fn advance(&self, frames: u64) {
        self.samples.fetch_add(frames, Ordering::Relaxed);
    }

    pub fn reset(&self, base: f64) {
        self.samples.store(0, Ordering::Relaxed);
        self.base_bits.store(base.to_bits(), Ordering::Relaxed);
    }

    /// 换倍速：以 `base` 为新起点，采样计数归零。
    pub fn rescale(&self, base: f64, speed: f64) {
        self.speed_bits.store(speed.to_bits(), Ordering::Relaxed);
        self.reset(base);
    }

    pub fn position(&self) -> f64 {
        let rate = self.rate.load(Ordering::Relaxed).max(1) as f64;
        let base = f64::from_bits(self.base_bits.load(Ordering::Relaxed));
        let speed = f64::from_bits(self.speed_bits.load(Ordering::Relaxed));
        base + self.samples.load(Ordering::Relaxed) as f64 / rate * speed
    }
}

struct State {
    /// 暂停时停在这里
    base: f64,
    /// 本段播放的起点墙钟（暂停时为 None）
    started: Option<Instant>,
    /// 播放倍速
    rate: f64,
}

/// 播放时间轴（可克隆，内部共享）。
#[derive(Clone)]
pub struct Timeline {
    inner: Arc<Mutex<State>>,
    audio: Arc<Mutex<Option<Arc<AudioPosition>>>>,
    duration: f64,
}

impl Timeline {
    pub fn new(duration: f64) -> Self {
        Self {
            inner: Arc::new(Mutex::new(State {
                base: 0.0,
                started: None,
                rate: 1.0,
            })),
            audio: Arc::new(Mutex::new(None)),
            duration,
        }
    }



    pub fn attach_audio(&self, pos: Arc<AudioPosition>) {
        *self.audio.lock().unwrap() = Some(pos);
    }

    pub fn is_playing(&self) -> bool {
        self.inner.lock().unwrap().started.is_some()
    }


    /// 当前播放位置（秒）。
    pub fn position(&self) -> f64 {
        let st = self.inner.lock().unwrap();
        let raw = match st.started {
            None => st.base,
            Some(t0) => {
                if let Some(a) = self.audio.lock().unwrap().as_ref() {
                    // 音频钟：与音频硬件同步，长时间播放不会漂移
                    a.position()
                } else {
                    st.base + t0.elapsed().as_secs_f64() * st.rate
                }
            }
        };
        let d = self.duration;
        if d > 0.0 { raw.clamp(0.0, d) } else { raw.max(0.0) }
    }

    pub fn play(&self) {
        let mut st = self.inner.lock().unwrap();
        if st.started.is_none() {
            st.started = Some(Instant::now());
        }
    }

    pub fn pause(&self) {
        let mut st = self.inner.lock().unwrap();
        if let Some(t0) = st.started.take() {
            let audio_pos = self.audio.lock().unwrap().as_ref().map(|a| a.position());
            st.base = match audio_pos {
                // 有音频钟就以它为准：它已经把倍速算进去了，两边不会打架
                Some(p) => p,
                None => st.base + t0.elapsed().as_secs_f64() * st.rate,
            };
            if let Some(a) = self.audio.lock().unwrap().as_ref() {
                a.reset(st.base);
            }
        }
    }

    /// 跳到 `pos`，并让音频钟从新位置重新计时。
    pub fn seek(&self, pos: f64) {
        let pos = if self.duration > 0.0 {
            pos.clamp(0.0, self.duration)
        } else {
            pos.max(0.0)
        };
        let mut st = self.inner.lock().unwrap();
        st.base = pos;
        st.started = st.started.map(|_| Instant::now());
        if let Some(a) = self.audio.lock().unwrap().as_ref() {
            a.reset(pos);
        }
    }

    /// 改倍速。`pos_then` 是变更瞬间的播放位置。
    pub fn set_rate(&self, rate: f64, pos_then: f64) {
        let mut st = self.inner.lock().unwrap();
        st.base = pos_then;
        st.rate = rate;
        st.started = st.started.map(|_| Instant::now());
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;
    use std::time::Duration;

    #[test]
    fn wall_clock_advances() {
        let t = Timeline::new(100.0);
        assert_eq!(t.position(), 0.0);
        t.play();
        sleep(Duration::from_millis(60));
        let p = t.position();
        // 上界给得宽松：机器负载高时 sleep 会明显超时，卡 0.5s 会偶发假红
        assert!(p > 0.02 && p < 2.0, "{p}");
        t.pause();
        let paused = t.position();
        sleep(Duration::from_millis(40));
        assert!((t.position() - paused).abs() < 1e-9);
    }

    #[test]
    fn seek_clamps() {
        let t = Timeline::new(10.0);
        t.seek(99.0);
        assert_eq!(t.position(), 10.0);
        t.seek(-5.0);
        assert_eq!(t.position(), 0.0);
    }

    #[test]
    fn audio_clock_wins() {
        let t = Timeline::new(100.0);
        let a = AudioPosition::new(1000, 0.0);
        t.attach_audio(a.clone());
        t.play();
        a.advance(500);
        assert!((t.position() - 0.5).abs() < 1e-9);
        t.seek(20.0);
        assert!((t.position() - 20.0).abs() < 1e-9);
        a.advance(250);
        assert!((t.position() - 20.25).abs() < 1e-9);
    }
}
