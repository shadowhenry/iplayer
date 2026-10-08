//! 视频解码轨。ffmpeg 把画面解成原始 BGRA 帧，经管道送上来，直接包成 GPUI 纹理。
//!
//! 三条原则：
//! 1. **不按 `-re` 限速**。ffmpeg 能多快解多快，靠有界通道做背压，永远是"提前备好"。
//! 2. **时钟说了算**。呈现端只挑 `pts <= 当前时间` 的最新一帧，落后就丢、超前就等。
//! 3. **seek 就是重开**。杀掉子进程，带 `-ss` 重新起一个，帧序号从 0 重新算。

use std::collections::VecDeque;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gpui_kit::RenderImage;
use image::{Frame as ImgFrame, ImageBuffer, Rgba};
use smallvec::SmallVec;

use crate::ffmpeg::{self, Cancel};

/// 解码线程与呈现端之间的通道容量（帧）。它同时就是背压深度。
const PIPELINE_DEPTH: usize = 3;
/// 呈现端本地再缓冲几帧，避免每帧都去摸一次通道。
const AHEAD: usize = 4;
/// 允许提前呈现的时间（秒），小于半帧间隔，避免卡在边界来回抖。
const TOL: f64 = 0.004;
/// 落后超过这个秒数的帧直接丢弃，不做"快进补帧"。
const LATE: f64 = 0.5;

pub struct VideoFrame {
    pub pts: f64,
    pub image: Arc<RenderImage>,
}

pub struct VideoTrack {
    path: PathBuf,
    pub out_w: u32,
    pub out_h: u32,
    fps: f64,
    rx: smol::channel::Receiver<VideoFrame>,
    cancel: Arc<Cancel>,
    child: Arc<Mutex<Option<Child>>>,
    handle: Option<std::thread::JoinHandle<()>>,
    ahead: VecDeque<VideoFrame>,
    /// 当前应该显示的画面
    pub current: Option<Arc<RenderImage>>,
    current_pts: f64,
    /// 解码器已吐出的最后一帧时间点
    tail_pts: f64,
    /// 通道已关闭且本地队列排空 —— 不会再有新帧了
    drained: bool,
    /// 统计：一共丢了多少帧
    pub dropped: u64,
}

impl VideoTrack {
    /// 起一条视频轨。`base` 是起始播放位置（秒）。
    pub fn start(
        path: &Path,
        out_w: u32,
        out_h: u32,
        fps: f64,
        base: f64,
    ) -> Result<Self, String> {
        let mut track = Self {
            path: path.to_path_buf(),
            out_w,
            out_h,
            fps: if fps > 0.0 { fps } else { 25.0 },
            rx: smol::channel::bounded(1).1,
            cancel: Cancel::new(),
            child: Arc::new(Mutex::new(None)),
            handle: None,
            ahead: VecDeque::new(),
            current: None,
            current_pts: -1.0,
            tail_pts: 0.0,
            drained: false,
            dropped: 0,
        };
        track.spawn(base)?;
        Ok(track)
    }

    /// 换一段继续解（seek 用）。会先干掉上一个解码进程。
    fn spawn(&mut self, base: f64) -> Result<(), String> {
        self.kill();
        self.ahead.clear();
        self.current = None;
        self.current_pts = -1.0;
        self.tail_pts = base;
        self.drained = false;
        self.dropped = 0;

        let (tx, rx) = smol::channel::bounded::<VideoFrame>(PIPELINE_DEPTH);
        self.rx = rx;
        let cancel = Cancel::new();
        self.cancel = cancel.clone();
        let child_slot = self.child.clone();
        let ended = Arc::new(AtomicBool::new(false));

        let path = self.path.clone();
        let (w, h, fps) = (self.out_w, self.out_h, self.fps);

        let cmd = build_command(&path, w, h, fps, base)?;

        self.handle = Some(std::thread::spawn(move || {
            decode_loop(cmd, w, h, fps, base, tx, cancel, child_slot, ended);
        }));
        Ok(())
    }

    pub fn seek(&mut self, pos: f64) -> Result<(), String> {
        self.spawn(pos)
    }

    /// 把 `position` 之前最新的那一帧设为当前画面。返回画面是否变化。
    pub fn advance(&mut self, position: f64) -> bool {
        let mut changed = false;

        // 1) 本地队列里已经到点的，按顺序推上去
        while let Some(front) = self.ahead.front() {
            if front.pts > position + TOL {
                break;
            }
            let f = self.ahead.pop_front().unwrap();
            if f.pts < position - LATE {
                self.dropped += 1;
            } else {
                self.set_current(f);
                changed = true;
            }
        }

        // 2) 从解码通道补充本地队列
        while self.ahead.len() < AHEAD {
            match self.rx.try_recv() {
                Ok(f) => {
                    self.tail_pts = f.pts;
                    if f.pts <= position + TOL {
                        if f.pts < position - LATE {
                            self.dropped += 1;
                        } else {
                            self.set_current(f);
                            changed = true;
                        }
                    } else {
                        self.ahead.push_back(f);
                    }
                }
                // ⚠️ 只有 `Closed` 才代表解码线程收工。
                // `Empty` 只是"这一刻还没解出来"（解码跟不上、刚 seek 完），
                // 把它当结束会让播放随机停住 —— 这个坑踩过一次了。
                Err(smol::channel::TryRecvError::Closed) => {
                    self.drained = true;
                    break;
                }
                Err(smol::channel::TryRecvError::Empty) => break,
            }
        }

        changed
    }

    fn set_current(&mut self, f: VideoFrame) {
        if f.pts >= self.current_pts {
            self.current = Some(f.image);
            self.current_pts = f.pts;
        }
    }

    /// 解码是否已经走完，且最后一帧也已经显示过。
    ///
    /// 必须要求 `current_pts >= 0`：起播瞬间通道还是空的，那时
    /// `drained && ahead.is_empty()` 也成立，不挡住的话会立刻被判"播完"。
    pub fn ended(&self, position: f64) -> bool {
        self.current_pts >= 0.0
            && self.drained
            && self.ahead.is_empty()
            && position >= self.current_pts - LATE
    }



    /// 杀掉解码进程并等线程退出。切换文件、关闭播放器时必须调用。
    pub fn kill(&mut self) {
        self.cancel.cancel();
        if let Some(mut child) = self.child.lock().unwrap().take() {
            let _ = child.kill();
        }
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for VideoTrack {
    fn drop(&mut self) {
        self.kill();
    }
}

fn build_command(path: &Path, w: u32, h: u32, fps: f64, base: f64) -> Result<Command, String> {
    let mut cmd = ffmpeg::base_command()?;
    if base > 0.0 {
        // `-ss` 放在 `-i` 之前 = 源内定位，秒开；输出时间戳会归零，帧序号从 0 重算。
        cmd.arg("-ss").arg(format!("{base:.6}"));
    }
    // 动图（GIF / 动画 WebP）的时间戳是逐帧给的、还可能是变帧率，
    // 而呈现端是按「帧序号 / fps」推算时间的 —— 先归一化成固定帧率才对得上，
    // 否则 GIF 会忽快忽慢。
    let steady = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .is_some_and(|e| e == "gif" || e == "webp");
    let filter = if steady {
        format!("fps={fps:.6},scale={w}:{h}:flags=bicubic")
    } else {
        format!("scale={w}:{h}:flags=bicubic")
    };

    cmd.arg("-i")
        .arg(path)
        .arg("-an")
        .arg("-sn")
        .arg("-dn")
        .arg("-vf")
        .arg(filter)
        .arg("-pix_fmt")
        .arg("bgra")
        .arg("-f")
        .arg("rawvideo")
        .arg("-")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    Ok(cmd)
}

#[allow(clippy::too_many_arguments)]
fn decode_loop(
    mut cmd: Command,
    w: u32,
    h: u32,
    fps: f64,
    base: f64,
    tx: smol::channel::Sender<VideoFrame>,
    cancel: Arc<Cancel>,
    child_slot: Arc<Mutex<Option<Child>>>,
    ended: Arc<AtomicBool>,
) {
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[player] 无法启动 ffmpeg: {e}");
            ended.store(true, Ordering::SeqCst);
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

    let n = (w as usize) * (h as usize) * 4;
    let mut index: u64 = 0;
    let mut buf = vec![0u8; n];

    loop {
        if cancel.is_cancelled() {
            break;
        }
        if read_exact(&mut stdout, &mut buf).is_err() {
            break;
        }
        let pts = base + index as f64 / fps;
        index += 1;
        let image = match ImageBuffer::<Rgba<u8>, Vec<u8>>::from_raw(w, h, buf) {
            Some(im) => Arc::new(RenderImage::new(SmallVec::from_vec(vec![ImgFrame::new(im)]))),
            None => break,
        };
        // 通道满就等一会儿再试 —— 用轮询而不是阻塞发送，
        // 这样取消时也能立刻退出，不会挂在满载通道上。
        let mut frame = Some(VideoFrame { pts, image });
        let mut closed = false;
        while let Some(f) = frame.take() {
            if cancel.is_cancelled() {
                closed = true;
                break;
            }
            match tx.try_send(f) {
                Ok(()) => {}
                Err(smol::channel::TrySendError::Full(f)) => {
                    frame = Some(f);
                    std::thread::sleep(Duration::from_millis(3));
                }
                Err(smol::channel::TrySendError::Closed(_)) => {
                    closed = true;
                    break;
                }
            }
        }
        if closed {
            break;
        }
        buf = vec![0u8; n];
    }

    // 收尾：杀掉并回收子进程，否则会留下孤儿
    if let Some(mut c) = child_slot.lock().unwrap().take() {
        let _ = c.kill();
        let _ = c.wait();
    }
    if let Ok(err) = err_handle.join() {
        let err = err.trim();
        if !err.is_empty() && !cancel.is_cancelled() {
            eprintln!("[player] ffmpeg: {err}");
        }
    }
    ended.store(true, Ordering::SeqCst);
    drop(tx);
}

/// `read_exact` 在子进程被 kill 时会返回 `BrokenPipe`/`UnexpectedEof`，两者都当作正常结束。
fn read_exact<R: Read>(r: &mut R, buf: &mut [u8]) -> std::io::Result<()> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) => return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof)),
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// 让画面刚好装进 `max_w × max_h`，保持宽高比，尺寸取偶数（H.264/许多滤镜要求）。
pub fn fit_size(src_w: u32, src_h: u32, rotate: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    // 旋转 90/270 时显示宽高互换（ffmpeg 的 autorotate 会先把画面摆正再缩放）
    let (dw, dh) = if matches!(rotate, 90 | 270) {
        (src_h.max(1), src_w.max(1))
    } else {
        (src_w.max(1), src_h.max(1))
    };
    let scale = f64::min(
        1.0,
        f64::min(max_w as f64 / dw as f64, max_h as f64 / dh as f64),
    );
    let w = ((dw as f64 * scale).round() as u32).max(2) & !1;
    let h = ((dh as f64 * scale).round() as u32).max(2) & !1;
    (w, h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_keeps_aspect_and_caps() {
        assert_eq!(fit_size(1920, 1080, 0, 1920, 1080), (1920, 1080));
        assert_eq!(fit_size(3840, 2160, 0, 1920, 1080), (1920, 1080));
        assert_eq!(fit_size(1280, 720, 0, 1920, 1080), (1280, 720));
        // 竖屏手机视频：旋转后按 1080x1920 计算
        assert_eq!(fit_size(1920, 1080, 90, 1920, 1080), (608, 1080));
        // 宽高比保持
        let (w, h) = fit_size(1000, 500, 0, 400, 400);
        assert_eq!((w, h), (400, 200));
    }
}
