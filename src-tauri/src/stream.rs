//! "Transcode while you watch" — the slow path, made bearable.
//!
//! A file whose video bitstream the webview cannot decode (WMV, MPEG-2, DivX,
//! Hi10P…) can only be played after a re-encode. Doing that up front means
//! staring at a progress bar for minutes, so instead iPlayer plays the re-encode
//! *while it happens*:
//!
//! * ffmpeg writes a fragmented MP4 (immediate `moov`, one fragment per GOP) to
//!   a pipe — nothing is written to disk, and because the frontend only pulls
//!   when its media buffer runs low, the encoder is throttled by playback
//!   itself instead of racing ahead;
//! * the webview pulls the bytes in small pieces and feeds them to a
//!   `MediaSource` buffer, so playback starts in a couple of seconds;
//! * seeking restarts the encoder at the target position rather than waiting for
//!   it to catch up, which keeps a scrub just as responsive on a two-hour film.
//!
//! Sessions are keyed by id; each one owns a reader thread that pumps the pipe
//! into a bounded channel (≈1 MiB of RAM), which both keeps the pull-back
//! pressure honest and gives `stream_read` something to block on.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, RecvTimeoutError, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde::Serialize;

use crate::ffmpeg;
use crate::media::{self, MediaInfo};

/// Largest slice handed back to the webview in one call.
pub const PULL_MAX: usize = 256 * 1024;
/// Read size per pipe read, and how many may sit in the channel (≈1 MiB).
const CHUNK: usize = 64 * 1024;
const QUEUE: usize = 16;
/// Keyframe interval, in seconds of media. A short GOP is what makes the first
/// fragment — and every post-seek fragment — ready in about a second.
const GOP_SECONDS: f64 = 2.0;
/// How often a blocked reader re-checks whether its session has been killed.
/// Without it, abandoning a session (the user jumped to another file) would
/// leave a thread parked on a full channel forever.
const TICK: Duration = Duration::from_millis(150);

pub struct Session {
    child: Mutex<Child>,
    rx: Mutex<std::sync::mpsc::Receiver<Vec<u8>>>,
    stderr: Arc<Mutex<String>>,
    produced: AtomicU64,
    finished: Arc<AtomicBool>,
    /// Set by `kill()`; the pump and reader threads watch it so they can wind
    /// up instead of waiting for a consumer that is never coming back.
    stopped: Arc<AtomicBool>,
}

static SESSIONS: OnceLock<Mutex<HashMap<String, Arc<Session>>>> = OnceLock::new();
static SEQ: AtomicU64 = AtomicU64::new(1);

fn sessions() -> &'static Mutex<HashMap<String, Arc<Session>>> {
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn next_id() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

// ---------------------------------------------------------------------------
// starting / stopping
// ---------------------------------------------------------------------------

#[derive(Serialize, Clone)]
pub struct StreamStart {
    pub id: String,
    /// The position the encoder actually started at (clamped to the duration).
    pub start: f64,
    pub duration: f64,
}

/// Probe the file and start re-encoding it from `start` seconds.
///
/// Blocking (ffprobe + process spawn); call it from `spawn_blocking`.
pub fn start(path: &str, start: f64) -> Result<StreamStart, String> {
    let src = PathBuf::from(path);
    if !src.is_file() {
        return Err(format!("文件不存在: {path}"));
    }
    let info = media::probe(&src)?;
    if !info.has_video {
        return Err("该文件没有视频轨，无法边转码边播放".to_string());
    }
    // Never start inside the last second — the encoder would emit nothing.
    let start = start.max(0.0).min((info.duration - 1.0).max(0.0));

    let id = next_id();
    let (tx, rx) = sync_channel::<Vec<u8>>(QUEUE);
    let finished = Arc::new(AtomicBool::new(false));
    let stopped = Arc::new(AtomicBool::new(false));
    let stderr_buf = Arc::new(Mutex::new(String::new()));

    // A session outlives its encoder for a moment (the frontend asks for the
    // failure text after the pipe closes), so old ones are collected here
    // rather than never.
    reap_finished();

    let mut cmd = build_command(&src, &info, start)?;
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("无法启动 ffmpeg: {e}"))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "无法读取 ffmpeg 输出".to_string())?;
    let stderr = child.stderr.take();

    // Pump the pipe into the bounded channel. When the frontend stops pulling,
    // the channel fills, this thread blocks, the pipe fills and ffmpeg blocks —
    // which is exactly the backpressure we want.
    {
        let finished = finished.clone();
        let stopped = stopped.clone();
        std::thread::spawn(move || {
            pump_pipe(stdout, tx, stopped);
            finished.store(true, Ordering::SeqCst);
        });
    }

    if let Some(mut err) = stderr {
        let buf = stderr_buf.clone();
        std::thread::spawn(move || {
            let mut text = String::new();
            let _ = err.read_to_string(&mut text);
            if let Ok(mut slot) = buf.lock() {
                slot.push_str(&text);
            }
        });
    }

    let sess = Arc::new(Session {
        child: Mutex::new(child),
        rx: Mutex::new(rx),
        stderr: stderr_buf,
        produced: AtomicU64::new(0),
        finished,
        stopped,
    });
    sessions().lock().unwrap().insert(id.clone(), sess);

    Ok(StreamStart {
        id,
        start,
        duration: info.duration,
    })
}

fn pump_pipe(mut stdout: ChildStdout, tx: SyncSender<Vec<u8>>, stopped: Arc<AtomicBool>) {
    let mut buf = vec![0u8; CHUNK];
    loop {
        if stopped.load(Ordering::SeqCst) {
            return;
        }
        match stdout.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                // A plain blocking `send` would park here for good once an
                // abandoned session's channel fills up; polling lets the thread
                // notice that nobody wants its output any more.
                let mut chunk = buf[..n].to_vec();
                loop {
                    if stopped.load(Ordering::SeqCst) {
                        return;
                    }
                    match tx.try_send(chunk) {
                        Ok(()) => break,
                        Err(TrySendError::Full(c)) => {
                            chunk = c;
                            std::thread::sleep(TICK);
                        }
                        Err(TrySendError::Disconnected(_)) => return,
                    }
                }
            }
        }
    }
}

/// ffmpeg arguments for the live stream: fragmented MP4 on stdout.
fn build_command(src: &Path, info: &MediaInfo, start: f64) -> Result<Command, String> {
    let mut cmd = ffmpeg::base_command()?;
    if start > 0.05 {
        // Input seeking: starts at the nearest keyframe and drops the rest, so
        // a seek costs a fraction of a second even 90 minutes in.
        cmd.args(["-ss", &format!("{start:.3}")]);
    }
    cmd.arg("-i").arg(src);
    cmd.args(["-map", "0:v:0", "-map", "0:a:0?", "-sn", "-dn"]);

    let enc = ffmpeg::h264_encoder(&ffmpeg::status().encoders);
    // Frame counts are not what we care about here: two seconds of media is the
    // granularity the player needs, whatever the frame rate (a 12 fps cartoon
    // and a 60 fps clip should both hand over a fragment just as fast).
    let fps = if info.fps.is_finite() && info.fps > 0.0 {
        info.fps
    } else {
        25.0
    };
    let gop = ((fps * GOP_SECONDS).round() as i64).clamp(12, 400);
    let gop = gop.to_string();
    cmd.args(["-c:v", enc]);
    if enc == "libx264" {
        cmd.args([
            "-preset",
            "veryfast",
            "-crf",
            "23",
            "-g",
            &gop,
            "-keyint_min",
            &gop,
            // No scene-cut keyframes: one fragment per GOP, always.
            "-sc_threshold",
            "0",
        ]);
    } else {
        cmd.args(["-b:v", "6000k", "-allow_sw", "1", "-g", &gop]);
    }
    // 10-bit / 4:4:4 / 4:2:2 sources have to come down to 8-bit 4:2:0.
    cmd.args(["-pix_fmt", "yuv420p"]);
    if info.has_audio {
        cmd.args(["-c:a", "aac", "-b:a", "192k", "-ac", "2"]);
    }
    cmd.args([
        "-movflags",
        // empty_moov: the header is complete before the first frame is encoded,
        // so the webview can start on the very first fragment.
        // negative_cts_offsets: keeps B-frame reordering out of an edit list,
        // which MediaSource implementations handle poorly.
        "+frag_keyframe+empty_moov+default_base_moof+negative_cts_offsets",
        // Hard cap on fragment length, for sources whose keyframes are far
        // apart despite the GOP setting.
        "-frag_duration",
        "2000000",
        "-f",
        "mp4",
        "pipe:1",
    ]);
    Ok(cmd)
}

fn lookup(id: &str) -> Option<Arc<Session>> {
    sessions().lock().ok()?.get(id).cloned()
}

/// Close the book on encoders that have already exited.
fn reap_finished() {
    let stale: Vec<String> = match sessions().lock() {
        Ok(map) => map
            .iter()
            .filter(|(_, s)| {
                s.child
                    .lock()
                    .map(|mut c| matches!(c.try_wait(), Ok(Some(_))))
                    .unwrap_or(false)
            })
            .map(|(id, _)| id.clone())
            .collect(),
        Err(_) => Vec::new(),
    };
    for id in stale {
        if let Ok(mut map) = sessions().lock() {
            map.remove(&id);
        }
    }
}

/// Ends a session: kill ffmpeg and forget it. Safe to call twice.
pub fn stop(id: &str) {
    let sess = match sessions().lock() {
        Ok(mut map) => map.remove(id),
        Err(_) => None,
    };
    if let Some(sess) = sess {
        kill(&sess);
    }
}

fn kill(sess: &Session) {
    // Flag first: the reader thread may be parked on a full channel and has to
    // learn about this before it can wind up.
    sess.stopped.store(true, Ordering::SeqCst);
    if let Ok(mut child) = sess.child.lock() {
        let _ = child.kill();
        let _ = child.wait();
    }
    // Closing stdout unblocks the reader thread, which drops the channel
    // sender, which unblocks anyone waiting in `pull`.
    sess.finished.store(true, Ordering::SeqCst);
}

/// Kill every live encoder — called when the app exits so no ffmpeg is orphaned.
pub fn kill_all() {
    let ids: Vec<String> = match sessions().lock() {
        Ok(map) => map.keys().cloned().collect(),
        Err(_) => Vec::new(),
    };
    for id in ids {
        stop(&id);
    }
}

// ---------------------------------------------------------------------------
// reading
// ---------------------------------------------------------------------------

/// Block until at least one byte is available (or the stream ends) and return
/// up to `max` bytes. An empty result means the encoder is done — or that the
/// session was killed, which the caller can tell apart by its own generation
/// counter. Waiting in short slices means a killed session never leaves this
/// call (and its thread) parked forever.
pub fn pull(id: &str, max: usize) -> Result<Vec<u8>, String> {
    let sess = lookup(id).ok_or_else(|| "转码会话已结束".to_string())?;
    let max = max.clamp(1, PULL_MAX);
    let mut out: Vec<u8> = Vec::new();

    {
        let rx = sess
            .rx
            .lock()
            .map_err(|_| "转码会话状态异常".to_string())?;
        let first = loop {
            if sess.stopped.load(Ordering::SeqCst) {
                return Ok(out);
            }
            match rx.recv_timeout(TICK) {
                Ok(chunk) => break chunk,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => return Ok(out),
            }
        };
        out.extend_from_slice(&first);
        while out.len() < max {
            match rx.try_recv() {
                Ok(more) => out.extend_from_slice(&more),
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }
    }

    sess.produced
        .fetch_add(out.len() as u64, Ordering::Relaxed);
    Ok(out)
}

#[derive(Serialize, Clone)]
pub struct StreamStatus {
    /// Total bytes handed to the webview.
    pub produced: u64,
    /// ffmpeg has stopped producing.
    pub finished: bool,
    /// Non-zero when ffmpeg exited with a failure.
    pub failed: bool,
    /// Last line of ffmpeg's stderr, when it failed.
    pub error: Option<String>,
}

pub fn status(id: &str) -> Result<StreamStatus, String> {
    let sess = lookup(id).ok_or_else(|| "转码会话已结束".to_string())?;
    let mut failed = false;
    let mut done = sess.finished.load(Ordering::SeqCst);

    if let Ok(mut child) = sess.child.lock() {
        match child.try_wait() {
            Ok(Some(st)) => {
                done = true;
                failed = !st.success();
            }
            Ok(None) => {}
            Err(_) => {}
        }
    }

    let error = if failed {
        sess.stderr
            .lock()
            .ok()
            .map(|s| s.trim().lines().last().unwrap_or("").to_string())
            .filter(|s| !s.is_empty())
    } else {
        None
    };

    Ok(StreamStatus {
        produced: sess.produced.load(Ordering::Relaxed),
        finished: done,
        failed,
        error,
    })
}
