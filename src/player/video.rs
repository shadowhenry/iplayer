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

/// 用户手动调整的画面朝向，叠加在 ffmpeg 依据元数据自动摆正**之后**。
///
/// 语义（很要紧，改滤镜时别弄反）：`rot` 是把**屏幕上看到的那张画面**顺时针转过的
/// 角度，`flip_*` 作用在转完之后、也就是屏幕上 —— 所以菜单里的"左右翻转"永远是
/// 在眼前这张图上左右翻，而不是在原始坐标里翻。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Orientation {
    /// 顺时针角度，只取 0 / 90 / 180 / 270
    pub rot: u32,
    pub flip_h: bool,
    pub flip_v: bool,
}

impl Orientation {
    pub const IDENTITY: Self = Self {
        rot: 0,
        flip_h: false,
        flip_v: false,
    };

    pub fn is_identity(self) -> bool {
        self == Self::IDENTITY
    }

    /// 归一化成 `(顺时针角度, 是否左右翻转)`。因为 `上下翻转 = 左右翻转 ∘ 旋转 180°`，
    /// `(rot, flip_h, flip_v)` 有 16 种写法却只有 8 种效果（例如
    /// `rot=180` 和 `左右翻转+上下翻转` 是同一件事）。比较"效果是否相同"时先归一。
    pub fn canonical(self) -> (u32, bool) {
        let rot = if self.flip_v {
            (self.rot + 180) % 360
        } else {
            self.rot
        };
        (rot, self.flip_h ^ self.flip_v)
    }

    /// 两种写法是否画出同一张画面。
    pub fn same_effect(self, other: Self) -> bool {
        self.canonical() == other.canonical()
    }

    /// 旋转 90/270 时画面宽高互换。
    pub fn dims(self, w: u32, h: u32) -> (u32, u32) {
        if matches!(self.rot, 90 | 270) {
            (h, w)
        } else {
            (w, h)
        }
    }

    /// 接在 `scale=w:h` 后面的滤镜串（`w:h` 是**旋转前**的正立尺寸）。
    ///
    /// ffmpeg 的 `transpose=1` 是顺时针 90°，`transpose=2` 是逆时针 90°；
    /// 180° 用两次 hflip/vflip 表示（两者可交换，顺序无所谓）。
    pub fn filter_tail(self) -> String {
        let mut s = String::new();
        match self.rot {
            90 => s.push_str(",transpose=1"),
            180 => s.push_str(",hflip,vflip"),
            270 => s.push_str(",transpose=2"),
            _ => {}
        }
        if self.flip_h {
            s.push_str(",hflip");
        }
        if self.flip_v {
            s.push_str(",vflip");
        }
        s
    }

    /// 在当前看到的画面上再顺时针转 90°。
    pub fn rotated_cw(self) -> Self {
        Self {
            rot: (self.rot + 90) % 360,
            ..self
        }
    }

    /// 在当前看到的画面上再逆时针转 90°。
    pub fn rotated_ccw(self) -> Self {
        Self {
            rot: (self.rot + 270) % 360,
            ..self
        }
    }

    /// 在当前看到的画面上左右翻转。
    ///
    /// 屏幕空间的翻转要**先换算回原始坐标**：`水平翻转 ∘ 旋转 θ = 旋转 −θ ∘ 水平翻转`
    /// （上下翻转同理）。少了这一步，转了 90° 之后再点"左右翻转"就会变成上下翻。
    pub fn flipped_h(self) -> Self {
        Self {
            rot: (360 - self.rot) % 360,
            flip_h: !self.flip_h,
            ..self
        }
    }

    /// 在当前看到的画面上上下翻转。
    pub fn flipped_v(self) -> Self {
        Self {
            rot: (360 - self.rot) % 360,
            flip_v: !self.flip_v,
            ..self
        }
    }

    /// 给 toast / 面板看的中文描述。
    pub fn label(self) -> String {
        if self.is_identity() {
            return "原始".to_string();
        }
        let mut parts: Vec<&str> = Vec::new();
        match self.rot {
            90 => parts.push("顺时针 90°"),
            180 => parts.push("旋转 180°"),
            270 => parts.push("逆时针 90°"),
            _ => {}
        }
        if self.flip_h {
            parts.push("左右翻转");
        }
        if self.flip_v {
            parts.push("上下翻转");
        }
        parts.join(" · ")
    }

    /// 把**输出**像素 (ox, oy) 映回**输入**像素 (x, y)。
    ///
    /// 静态图片走 CPU 变换时用它（视频那条路是 ffmpeg 滤镜自己转），
    /// 两边的语义必须一致 —— `--selftest` 会拿真 ffmpeg 的产出来对这套映射。
    pub fn map_back(self, ox: u32, oy: u32, w: u32, h: u32) -> (u32, u32) {
        let (ow, oh) = self.dims(w, h);
        // 先把"屏幕上的翻转"撤掉
        let mut px = ox;
        let mut py = oy;
        if self.flip_h {
            px = ow - 1 - px;
        }
        if self.flip_v {
            py = oh - 1 - py;
        }
        // 再撤销旋转
        match self.rot {
            90 => (py, h - 1 - px),
            180 => (w - 1 - px, h - 1 - py),
            270 => (w - 1 - py, px),
            _ => (px, py),
        }
    }
}

pub struct VideoFrame {
    pub pts: f64,
    pub image: Arc<RenderImage>,
}

pub struct VideoTrack {
    path: PathBuf,
    /// 送给 `scale` 的**旋转前**正立宽度（真正读多少字节由 `orient.dims` 决定）
    pub out_w: u32,
    /// 同上，高度
    pub out_h: u32,
    /// 用户调的画面朝向
    pub orient: Orientation,
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
    /// 刚重开解码、新画面还没到。这段时间**旧画面继续留在屏上**，
    /// 别清成黑的（拖进度条时会一闪一闪），并且即便处于暂停也要允许挑帧。
    awaiting: bool,
    /// 这一轮重开一帧都没解出来（定位到末尾之外）。由 `advance` 记账。
    empty_spawn: bool,
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
            orient: Orientation::IDENTITY,
            fps: if fps > 0.0 { fps } else { 25.0 },
            rx: smol::channel::bounded(1).1,
            cancel: Cancel::new(),
            child: Arc::new(Mutex::new(None)),
            handle: None,
            ahead: VecDeque::new(),
            current: None,
            current_pts: -1.0,
            tail_pts: 0.0,
            awaiting: true,
            empty_spawn: false,
            drained: false,
            dropped: 0,
        };
        track.spawn(base)?;
        Ok(track)
    }

    /// 换一段继续解（seek 用）。会先干掉上一个解码进程。
    ///
    /// **刻意不清 `current`**：seek 之后 ffmpeg 要重新起进程、定位、解出第一帧，
    /// 这段时间里旧画面继续显示，屏幕才不会黑一下再亮一下。但 `current_pts` 必须
    /// 归 -1 —— 往回拖时新帧的 pts 比旧的小，不归零会被 `set_current` 的单调性挡掉，
    /// 画面就卡在旧位置不动了。
    fn spawn(&mut self, base: f64) -> Result<(), String> {
        self.kill();
        self.ahead.clear();
        self.current_pts = -1.0;
        self.tail_pts = base;
        self.awaiting = true;
        self.empty_spawn = false;
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
        let orient = self.orient;
        // 真正读出来的像素尺寸是旋转**之后**的（90/270 时宽高互换）
        let (rw, rh) = orient.dims(w, h);

        let cmd = build_command(&path, w, h, fps, base, orient)?;

        self.handle = Some(std::thread::spawn(move || {
            decode_loop(cmd, rw, rh, fps, base, tx, cancel, child_slot, ended);
        }));
        Ok(())
    }

    pub fn seek(&mut self, pos: f64) -> Result<(), String> {
        self.spawn(pos)
    }

    /// 换一个画面朝向：重开解码进程（跟 seek 同一条路径），`pos` 是当前播放位置，
    /// 这样画面会立刻以新角度从当前位置重新解出来。
    pub fn set_orientation(&mut self, orient: Orientation, pos: f64) -> Result<(), String> {
        // 比"效果"而不是比结构：`rot=180` 与「左右翻转+上下翻转」是同一张画面，
        // 没必要为它白重开一次解码（重开会让画面闪一下）。
        if self.orient.same_effect(orient) {
            self.orient = orient;
            return Ok(());
        }
        self.orient = orient;
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
                    // 这一轮重开一帧都没能上屏就把通道关了 —— 多半是定位到了
                    // 文件末尾之外。记一笔，`ended` 靠它脱身，不然播放位置会一直
                    // 往前走却永远没有画面。
                    // `current_pts < 0` 就是"本轮没上过屏"的判据（spawn 会把它归 -1）。
                    if self.awaiting && self.current_pts < 0.0 {
                        self.empty_spawn = true;
                    }
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
            // 新画面已经上屏，不再是"等着"的状态
            self.awaiting = false;
        }
    }

    /// 是否正在等 seek / 换角度之后的第一帧。
    ///
    /// 这一段时间里：① 旧画面留着别清；② 哪怕暂停也要继续挑帧（不然画面永远停在旧的）；
    /// ③ 主循环得继续要帧，否则新画面出来了也没人画。
    pub fn awaiting(&self) -> bool {
        self.awaiting && !self.drained
    }

    /// 解码是否已经走完，且最后一帧也已经显示过。
    ///
    /// 必须要求 `current_pts >= 0`：起播瞬间通道还是空的，那时
    /// `drained && ahead.is_empty()` 也成立，不挡住的话会立刻被判"播完"。
    pub fn ended(&self, position: f64) -> bool {
        if !(self.drained && self.ahead.is_empty()) {
            return false;
        }
        // 这一轮重开一帧都没解出来（`advance` 里记的账）——定位到末尾之外了，
        // 没有 current_pts 可比，但也不能一直僵着，直接算走完。
        if self.empty_spawn {
            return true;
        }
        self.current_pts >= 0.0 && position >= self.current_pts - LATE
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

fn build_command(
    path: &Path,
    w: u32,
    h: u32,
    fps: f64,
    base: f64,
    orient: Orientation,
) -> Result<Command, String> {
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
    let head = if steady {
        format!("fps={fps:.6},scale={w}:{h}:flags=bicubic")
    } else {
        format!("scale={w}:{h}:flags=bicubic")
    };
    // 用户的旋转/翻转接在缩放后面 —— 先缩好再转，输出尺寸才是确定的一块。
    let filter = format!("{head}{}", orient.filter_tail());

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

    // ── 画面朝向 ────────────────────────────────────────────────────────

    /// 只有 90/270 会换宽高。
    #[test]
    fn orientation_swaps_dims_only_for_quarter_turns() {
        assert_eq!(Orientation::IDENTITY.dims(4, 2), (4, 2));
        assert_eq!(Orientation { rot: 180, ..Default::default() }.dims(4, 2), (4, 2));
        assert_eq!(Orientation { rot: 90, ..Default::default() }.dims(4, 2), (2, 4));
        assert_eq!(Orientation { rot: 270, ..Default::default() }.dims(4, 2), (2, 4));
    }

    /// 滤镜串：转在前、翻在后（翻转作用在屏幕坐标里）。
    #[test]
    fn orientation_filter_chain() {
        let o = Orientation::IDENTITY;
        assert_eq!(o.filter_tail(), "");
        assert_eq!(Orientation { rot: 90, ..o }.filter_tail(), ",transpose=1");
        assert_eq!(Orientation { rot: 270, ..o }.filter_tail(), ",transpose=2");
        assert_eq!(Orientation { rot: 180, ..o }.filter_tail(), ",hflip,vflip");
        assert_eq!(
            Orientation { rot: 90, flip_h: true, ..o }.filter_tail(),
            ",transpose=1,hflip"
        );
        assert_eq!(
            Orientation { rot: 90, flip_h: true, flip_v: true }.filter_tail(),
            ",transpose=1,hflip,vflip"
        );
    }

    /// 四步转回原位；翻两次回到原位。
    #[test]
    fn orientation_group_laws() {
        let o = Orientation::IDENTITY;
        assert!(o.rotated_cw().rotated_cw().rotated_cw().rotated_cw().is_identity());
        assert!(o.rotated_ccw().rotated_ccw().rotated_ccw().rotated_ccw().is_identity());
        assert!(o.flipped_h().flipped_h().is_identity());
        assert!(o.flipped_v().flipped_v().is_identity());
        assert_eq!(o.rotated_cw().rotated_ccw(), o);
        assert_eq!(o.rotated_ccw().rot, 270);
        assert_eq!(o.rotated_cw().rot, 90);
    }

    /// **翻转作用在屏幕上**：转过 90° 之后再点"左右翻转"，
    /// 必须还是左右翻（rot 要换算成 −θ，否则就变成上下翻了）。
    #[test]
    fn flips_stay_in_screen_space_after_rotation() {
        let r90 = Orientation { rot: 90, ..Default::default() };
        assert_eq!(
            r90.flipped_h(),
            Orientation { rot: 270, flip_h: true, flip_v: false }
        );
        assert_eq!(
            r90.flipped_v(),
            Orientation { rot: 270, flip_h: false, flip_v: true }
        );
        // 180° 自身对称，翻转后角度不变
        let r180 = Orientation { rot: 180, ..Default::default() };
        assert_eq!(r180.flipped_h().rot, 180);
        // 转 180° 等价于左右翻转 + 上下翻转（这俩可交换）—— 写法不同、效果相同
        assert!(
            r180.same_effect(Orientation::IDENTITY.flipped_h().flipped_v()),
            "rot=180 应当与「左右翻转+上下翻转」等价"
        );
        assert!(!r180.is_identity());
    }

    /// 归一化 / 等价判定本身要靠谱：8 种效果、每种都分得开。
    #[test]
    fn orientation_has_eight_distinct_effects() {
        let o = Orientation::IDENTITY;
        let all = [
            o,
            o.rotated_cw(),
            o.rotated_cw().rotated_cw(),
            o.rotated_ccw(),
            o.flipped_h(),
            o.flipped_v(),
            o.rotated_cw().flipped_h(),
            o.rotated_cw().flipped_v(),
        ];
        for i in 0..all.len() {
            for j in (i + 1)..all.len() {
                assert!(
                    !all[i].same_effect(all[j]),
                    "{:?} 与 {:?} 应当是不同的朝向",
                    all[i],
                    all[j]
                );
            }
        }
        // 上下翻转 = 左右翻转 + 旋转 180°
        assert!(o.flipped_v().same_effect(o.rotated_cw().rotated_cw().flipped_h()));
    }

    /// `map_back` 必须是**双射**：输出里每个像素都能对回一个唯一的输入像素。
    #[test]
    fn orientation_map_is_a_bijection() {
        let (w, h) = (4u32, 3u32);
        let all = [
            Orientation::IDENTITY,
            Orientation { rot: 90, ..Default::default() },
            Orientation { rot: 180, ..Default::default() },
            Orientation { rot: 270, ..Default::default() },
            Orientation { flip_h: true, ..Default::default() },
            Orientation { flip_v: true, ..Default::default() },
            Orientation { rot: 90, flip_h: true, flip_v: true },
            Orientation { rot: 270, flip_h: true, flip_v: false },
        ];
        for o in all {
            let (ow, oh) = o.dims(w, h);
            let mut seen = vec![false; (w * h) as usize];
            for oy in 0..oh {
                for ox in 0..ow {
                    let (x, y) = o.map_back(ox, oy, w, h);
                    assert!(x < w && y < h, "{o:?} 映出界: ({ox},{oy}) -> ({x},{y})");
                    let i = (y * w + x) as usize;
                    assert!(!seen[i], "{o:?} 不是双射：({x},{y}) 被映了两次");
                    seen[i] = true;
                }
            }
            assert!(seen.iter().all(|s| *s), "{o:?} 漏掉了像素");
        }
    }

    /// 角点手工核对（和 ffmpeg 的 transpose 语义对齐，`--selftest` 也会真跑一遍）。
    #[test]
    fn orientation_corners() {
        let (w, h) = (4u32, 2u32);
        // 顺时针 90°：原图左上角跑到输出的右上角
        let cw = Orientation { rot: 90, ..Default::default() };
        assert_eq!(cw.dims(w, h), (2, 4));
        assert_eq!(cw.map_back(1, 0, w, h), (0, 0));
        // 逆时针 90°：左上角跑到输出的左下角
        let ccw = Orientation { rot: 270, ..Default::default() };
        assert_eq!(ccw.map_back(0, 3, w, h), (0, 0));
        // 左右 / 上下翻转
        let fh = Orientation { flip_h: true, ..Default::default() };
        assert_eq!(fh.map_back(3, 0, w, h), (0, 0));
        let fv = Orientation { flip_v: true, ..Default::default() };
        assert_eq!(fv.map_back(0, 1, w, h), (0, 0));
        // 顺时针 90° + 左右翻转（屏幕空间）：左上角落在输出左上角
        let mix = Orientation { rot: 90, flip_h: true, flip_v: false };
        assert_eq!(mix.map_back(0, 0, w, h), (0, 0));
    }

    /// 中文描述（toast / 面板标题用）。
    #[test]
    fn orientation_labels() {
        assert_eq!(Orientation::IDENTITY.label(), "原始");
        assert_eq!(
            Orientation { rot: 90, flip_h: true, ..Default::default() }.label(),
            "顺时针 90° · 左右翻转"
        );
        assert_eq!(
            Orientation { rot: 180, ..Default::default() }.label(),
            "旋转 180°"
        );
    }
}
