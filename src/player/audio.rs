//! 音频播放轨。
//!
//! ffmpeg 把音轨解成 `f32` 交错采样（顺便做重采样），塞进环形缓冲；
//! cpal 的输出回调从缓冲里取采样喂给声卡。**回调消费了多少帧，音频钟就走到哪里。**
//!
//! cpal 的 `Stream` 在 macOS 上不是 `Send`，所以它整个留在自己那条线程里，
//! 主线程只通过原子量（暂停位、停止位）和环形缓冲跟它打交道。

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use super::spectrum::Tap;
use super::timeline::AudioPosition;
use crate::ffmpeg::{self, Cancel};

/// 环形缓冲最多攒多少秒音频。攒满后解码线程停下来（背压）。
const BUFFER_SECONDS: f64 = 0.5;
/// 一次读多少采样（交错计）
const CHUNK: usize = 8192;

/// 解码端与播放端共用的环形缓冲。
struct Ring {
    data: Vec<f32>,
    head: usize,
    len: usize,
}

impl Ring {
    fn new(cap: usize) -> Self {
        Self {
            data: vec![0.0; cap.max(1024)],
            head: 0,
            len: 0,
        }
    }


    /// 尽力写入，返回实际写入的采样数。
    fn push(&mut self, src: &[f32]) -> usize {
        let cap = self.data.len();
        let n = src.len().min(cap - self.len);
        for (i, v) in src[..n].iter().enumerate() {
            self.data[(self.head + self.len + i) % cap] = *v;
        }
        self.len += n;
        n
    }

    /// 取出至多 `dst.len()` 个采样，返回实际取出数。
    fn pop(&mut self, dst: &mut [f32]) -> usize {
        let cap = self.data.len();
        let n = dst.len().min(self.len);
        for (i, slot) in dst[..n].iter_mut().enumerate() {
            *slot = self.data[(self.head + i) % cap];
        }
        self.head = (self.head + n) % cap;
        self.len -= n;
        n
    }

    fn clear(&mut self) {
        self.head = 0;
        self.len = 0;
    }
}

pub struct AudioTrack {
    path: PathBuf,
    ring: Arc<Mutex<Ring>>,
    pos: Arc<AudioPosition>,
    cancel: Arc<Cancel>,
    child: Arc<Mutex<Option<Child>>>,
    handle: Option<std::thread::JoinHandle<()>>,
    /// 音频设备线程：持有 cpal `Stream`，直到置位
    device_stop: Arc<AtomicBool>,
    device_handle: Option<std::thread::JoinHandle<()>>,
    paused: Arc<AtomicBool>,
    /// 音量增益（0.0–1.0），由回调实时读取
    volume: Arc<AtomicU32>,
    /// 可视化用的采样窗，音频回调往里写、UI 线程每帧取走
    tap: Arc<Tap>,
    pub rate: u32,
    pub channels: usize,
}

/// 把倍速拆成一串 `atempo`（单个 atempo 只支持 0.5–2.0）。
fn atempo_chain(speed: f64) -> String {
    let mut steps: Vec<String> = Vec::new();
    let mut s = speed.clamp(0.25, 4.0);
    while s > 2.0 {
        steps.push("atempo=2.0".to_string());
        s /= 2.0;
    }
    while s < 0.5 {
        steps.push("atempo=0.5".to_string());
        s /= 0.5;
    }
    steps.push(format!("atempo={s:.6}"));
    steps.join(",")
}

impl AudioTrack {
    /// 起一条音轨。任何一步失败都返回 `Err`，调用方可以静默退化成无声播放。
    pub fn start(path: &Path, base: f64, speed: f64) -> Result<Self, String> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| "没有可用的音频输出设备".to_string())?;
        let supported = device
            .default_output_config()
            .map_err(|e| format!("读取音频输出配置失败: {e}"))?;
        let sample_format = supported.sample_format();
        let config: cpal::StreamConfig = supported.into();
        let rate = config.sample_rate.0;
        let dev_channels = config.channels as usize;

        let ring = Arc::new(Mutex::new(Ring::new(
            (rate as f64 * dev_channels as f64 * BUFFER_SECONDS) as usize,
        )));
        let pos = AudioPosition::new(rate, base);
        let paused = Arc::new(AtomicBool::new(false));
        let volume = Arc::new(AtomicU32::new(1.0f32.to_bits()));
        let device_stop = Arc::new(AtomicBool::new(false));
        let tap = Tap::new();

        // --- 设备线程：建流并一直持有 ---
        let (ready_tx, ready_rx) = smol::channel::bounded::<Result<(), String>>(1);
        let device_handle = {
            let ring = ring.clone();
            let pos = pos.clone();
            let paused = paused.clone();
            let volume = volume.clone();
            let stop = device_stop.clone();
            let tap = tap.clone();
            let device = device.clone();
            let config = config.clone();
            std::thread::spawn(move || {
                let result: Result<cpal::Stream, String> = match sample_format {
                    cpal::SampleFormat::F32 => {
                        let (ring, pos, paused, volume) =
                            (ring.clone(), pos.clone(), paused.clone(), volume.clone());
                        let tap = tap.clone();
                        // 回调里不做分配：mono 只扩容一次，之后一直复用
                        let mut mono: Vec<f32> = Vec::new();
                        device
                            .build_output_stream(
                                &config,
                                move |data: &mut [f32], _| {
                                    fill(
                                        data,
                                        dev_channels,
                                        &ring,
                                        &pos,
                                        &paused,
                                        &volume,
                                        &tap,
                                        &mut mono,
                                    )
                                },
                                |e| eprintln!("[audio] 输出流错误: {e}"),
                                None,
                            )
                            .map_err(|e| format!("无法创建音频输出流: {e}"))
                    }
                    cpal::SampleFormat::I16 => {
                        let (ring, pos, paused, volume) =
                            (ring.clone(), pos.clone(), paused.clone(), volume.clone());
                        let tap = tap.clone();
                        // 回调里不做分配：scratch / mono 只扩容一次，之后一直复用
                        let mut scratch: Vec<f32> = Vec::new();
                        let mut mono: Vec<f32> = Vec::new();
                        device
                            .build_output_stream(
                                &config,
                                move |data: &mut [i16], _| {
                                    if scratch.len() < data.len() {
                                        scratch.resize(data.len(), 0.0);
                                    }
                                    let buf = &mut scratch[..data.len()];
                                    fill(
                                        buf,
                                        dev_channels,
                                        &ring,
                                        &pos,
                                        &paused,
                                        &volume,
                                        &tap,
                                        &mut mono,
                                    );
                                    for (o, v) in data.iter_mut().zip(buf.iter()) {
                                        *o = (v.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                                    }
                                },
                                |e| eprintln!("[audio] 输出流错误: {e}"),
                                None,
                            )
                            .map_err(|e| format!("无法创建音频输出流: {e}"))
                    }
                    other => Err(format!("不支持的音频采样格式 {other:?}")),
                };

                match result {
                    Ok(stream) => {
                        if let Err(e) = stream.play() {
                            let _ = ready_tx.send_blocking(Err(format!("无法启动音频输出: {e}")));
                            return;
                        }
                        let _ = ready_tx.send_blocking(Ok(()));
                        // 抱着 stream 不放，直到被要求停止
                        while !stop.load(Ordering::Relaxed) {
                            std::thread::sleep(Duration::from_millis(80));
                        }
                        let _ = stream.pause();
                    }
                    Err(e) => {
                        let _ = ready_tx.send_blocking(Err(e));
                    }
                }
            })
        };

        match ready_rx.recv_blocking() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                device_stop.store(true, Ordering::Relaxed);
                let _ = device_handle.join();
                return Err(e);
            }
            Err(_) => {
                device_stop.store(true, Ordering::Relaxed);
                let _ = device_handle.join();
                return Err("音频设备初始化中断".to_string());
            }
        }

        // --- 解码线程 ---
        let child_slot: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));
        let cancel = Cancel::new();
        let mut track = Self {
            path: path.to_path_buf(),
            ring,
            pos,
            cancel,
            child: child_slot,
            handle: None,
            device_stop,
            device_handle: Some(device_handle),
            paused,
            volume,
            tap,
            rate,
            channels: dev_channels,
        };
        track.spawn_decoder(base, speed)?;
        Ok(track)
    }

    /// 起（或重起）ffmpeg 解码线程。seek / 换倍速都走这里。
    fn spawn_decoder(&mut self, base: f64, speed: f64) -> Result<(), String> {
        self.kill_decoder();

        let mut cmd = ffmpeg::base_command()?;
        if base > 0.0 {
            cmd.arg("-ss").arg(format!("{base:.6}"));
        }
        cmd.arg("-i")
            .arg(&self.path)
            .arg("-vn")
            .arg("-sn")
            .arg("-dn")
            .arg("-ac")
            .arg("2")
            .arg("-ar")
            .arg(self.rate.to_string());
        if (speed - 1.0).abs() > 1e-6 {
            cmd.arg("-af").arg(atempo_chain(speed));
        }
        cmd.arg("-f")
            .arg("f32le")
            .arg("-")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let cancel = Cancel::new();
        self.cancel = cancel.clone();
        let child_slot = self.child.clone();
        let ring = self.ring.clone();
        let dev_channels = self.channels;
        self.handle = Some(std::thread::spawn(move || {
            decode_loop(cmd, ring, dev_channels, cancel, child_slot)
        }));
        Ok(())
    }

    fn kill_decoder(&mut self) {
        self.cancel.cancel();
        if let Some(mut c) = self.child.lock().unwrap().take() {
            let _ = c.kill();
        }
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }

    pub fn position_source(&self) -> Arc<AudioPosition> {
        self.pos.clone()
    }

    /// 可视化用的采样窗。UI 线程每帧从这里取一窗做 FFT。
    pub fn tap(&self) -> Arc<Tap> {
        self.tap.clone()
    }

    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Relaxed);
    }

    /// 音量 0.0–1.0（1.0 = 原始音量）。
    pub fn set_volume(&self, volume: f32) {
        self.volume
            .store(volume.clamp(0.0, 2.0).to_bits(), Ordering::Relaxed);
    }

    /// 跳到新位置（seek / 换倍速都走这里）：
    /// 重置音频钟、清空缓冲（否则会先播出旧声音）、用新参数重起解码线程。
    /// 音频设备不重建，所以不会有重新开流的爆音。
    pub fn seek_to(&mut self, pos: f64, speed: f64) {
        self.pos.rescale(pos, speed);
        self.kill_decoder();
        if let Ok(mut r) = self.ring.lock() {
            r.clear();
        }
        // 别让旧位置的波形在新位置上再闪一下
        self.tap.clear();
        if let Err(e) = self.spawn_decoder(pos, speed) {
            eprintln!("[audio] 重启解码失败: {e}");
        }
    }

    pub fn stop(&mut self) {
        self.kill_decoder();
        self.device_stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.device_handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for AudioTrack {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 音频回调：把环形缓冲里的数据填进输出缓冲，不够的部分补静音。
///
/// 暂停时只写静音、**不推进位置**，音频钟自然停住。
///
/// 顺便把最终播出去的声音（已含音量增益）折成单声道喂给 [`Tap`]，可视化看到的就是
/// 耳朵听到的。`mono` 是调用方持有的复用缓冲，回调里不做任何分配。
#[allow(clippy::too_many_arguments)]
fn fill(
    data: &mut [f32],
    channels: usize,
    ring: &Mutex<Ring>,
    pos: &AudioPosition,
    paused: &AtomicBool,
    volume: &AtomicU32,
    tap: &Tap,
    mono: &mut Vec<f32>,
) {
    let gain = f32::from_bits(volume.load(Ordering::Relaxed));
    let frames = if channels > 0 { data.len() / channels } else { 0 };
    if paused.load(Ordering::Relaxed) {
        data.fill(0.0);
        // 暂停 = 送出静音，频谱自然会落回去，不用单独清 Tap
        mono.clear();
        mono.resize(frames, 0.0);
        tap.push(mono);
        return;
    }
    match ring.try_lock() {
        Ok(mut r) => {
            let got = r.pop(data);
            if got < data.len() {
                data[got..].fill(0.0);
            }
        }
        Err(_) => data.fill(0.0),
    }
    if (gain - 1.0).abs() > 1e-6 {
        for v in data.iter_mut() {
            *v *= gain;
        }
    }

    mono.clear();
    if channels <= 1 {
        mono.extend_from_slice(&data[..frames]);
    } else {
        mono.reserve(frames);
        for fr in data[..frames * channels].chunks_exact(channels) {
            mono.push(fr.iter().sum::<f32>() / channels as f32);
        }
    }
    tap.push(mono);

    pos.advance(frames as u64);
}

fn decode_loop(
    mut cmd: std::process::Command,
    ring: Arc<Mutex<Ring>>,
    dev_channels: usize,
    cancel: Arc<Cancel>,
    child_slot: Arc<Mutex<Option<Child>>>,
) {
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[audio] 无法启动 ffmpeg: {e}");
            return;
        }
    };
    let mut stdout = match child.stdout.take() {
        Some(s) => s,
        None => return,
    };
    let mut stderr = child.stderr.take();
    let err_handle = std::thread::spawn(move || {
        let mut buf = String::new();
        if let Some(mut s) = stderr.take() {
            let _ = s.read_to_string(&mut buf);
        }
        buf
    });
    *child_slot.lock().unwrap() = Some(child);

    // ffmpeg 固定输出 2 声道交错 f32；设备可能是 1/2/N 声道，这里做一次映射
    const SRC_CHANNELS: usize = 2;
    let mut raw = vec![0u8; CHUNK * SRC_CHANNELS * 4];
    let mut mapped: Vec<f32> = Vec::with_capacity(CHUNK * dev_channels);

    'outer: loop {
        if cancel.is_cancelled() {
            break;
        }
        let mut filled = 0usize;
        while filled < raw.len() {
            match stdout.read(&mut raw[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        if filled < SRC_CHANNELS * 4 {
            break;
        }
        let usable = filled - (filled % (SRC_CHANNELS * 4));

        mapped.clear();
        for chunk in raw[..usable].chunks_exact(4) {
            mapped.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
        }
        if dev_channels != SRC_CHANNELS {
            let interleaved = std::mem::take(&mut mapped);
            for fr in interleaved.chunks_exact(SRC_CHANNELS) {
                if dev_channels == 1 {
                    mapped.push((fr[0] + fr[1]) * 0.5);
                } else {
                    for c in 0..dev_channels {
                        mapped.push(fr[c.min(SRC_CHANNELS - 1)]);
                    }
                }
            }
        }

        // 写入环形缓冲；满了就等一会儿，形成背压
        let mut offset = 0usize;
        while offset < mapped.len() {
            if cancel.is_cancelled() {
                break 'outer;
            }
            let written = match ring.try_lock() {
                Ok(mut r) => r.push(&mapped[offset..]),
                Err(_) => 0,
            };
            if written == 0 {
                std::thread::sleep(Duration::from_millis(4));
            } else {
                offset += written;
            }
        }
    }

    if let Some(mut c) = child_slot.lock().unwrap().take() {
        let _ = c.kill();
        let _ = c.wait();
    }
    if let Ok(err) = err_handle.join() {
        let err = err.trim();
        if !err.is_empty() && !cancel.is_cancelled() {
            eprintln!("[audio] ffmpeg: {err}");
        }
    }
}
