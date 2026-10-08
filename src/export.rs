//! 导出工具箱：截图 / 提取音频 / 转 GIF。
//!
//! 三件事都交给 ffmpeg 做一次性任务，跑在**后台线程**里；主线程只读一个原子
//! 进度值，所以界面不会被编码卡住。任务可以随时取消（杀子进程 + 删掉半成品）。
//!
//! 进度靠 `-progress pipe:1`：ffmpeg 会往 stdout 吐 `key=value` 行，
//! 我们只关心 `out_time_us=`，拿它除以预期时长就是百分比。
//!
//! 一个任务由若干「步骤」串成，每个步骤占进度条的一段区间；带 `fallback`
//! 标记的步骤只在前一步失败时才跑 —— 音频「先试直接复制、不行再转 AAC」
//! 就是靠它实现的。

use std::io::{BufRead, BufReader, Read};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::ffmpeg;

/// 用户主动取消时的错误文案。
pub const CANCELLED: &str = "已取消";

/// 转 GIF 的默认参数。太大了文件爆炸，太小了看不清，这个档位比较平衡。
pub const GIF_FPS: u32 = 12;
pub const GIF_WIDTH: u32 = 480;
pub const GIF_SECONDS: f64 = 5.0;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Snapshot,
    Audio,
    Gif,
}

impl Kind {
    pub const ALL: [Kind; 3] = [Kind::Snapshot, Kind::Audio, Kind::Gif];

    pub fn label(self) -> &'static str {
        match self {
            Kind::Snapshot => "截图",
            Kind::Audio => "提取音频",
            Kind::Gif => "转 GIF",
        }
    }

    /// 面板上按钮的第二行小字。
    pub fn hint(self) -> &'static str {
        match self {
            Kind::Snapshot => "当前画面 · PNG",
            Kind::Audio => "整条音轨 · M4A",
            Kind::Gif => "当前位置起 5 秒",
        }
    }

    /// 保存对话框的默认后缀。
    pub fn ext(self) -> &'static str {
        match self {
            Kind::Snapshot => "png",
            Kind::Audio => "m4a",
            Kind::Gif => "gif",
        }
    }

    pub fn filter(self) -> (&'static str, &'static [&'static str]) {
        match self {
            Kind::Snapshot => ("PNG 图片", &["png"]),
            Kind::Audio => ("M4A 音频", &["m4a"]),
            Kind::Gif => ("GIF 动图", &["gif"]),
        }
    }
}

// ---------------------------------------------------------------------------
// 进度与结果
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Progress {
    percent: AtomicU32,
    cancel: AtomicBool,
    /// `None` = 还在跑。跑完由工作线程写一次，App 取走。
    outcome: Mutex<Option<Result<(), String>>>,
}

impl Progress {
    fn set(&self, value: f64) {
        let pct = value.clamp(0.0, 100.0).round() as u32;
        self.percent.store(pct, Ordering::Relaxed);
    }
}

/// 一个正在跑的导出任务。
pub struct Export {
    pub dest: PathBuf,
    pub label: String,
    progress: Arc<Progress>,
    handle: Option<std::thread::JoinHandle<()>>,
}

struct Step {
    cmd: Command,
    /// 占总进度的哪一段
    span: Range<f64>,
    /// 只在前一步失败时才跑
    fallback: bool,
}

impl Export {
    /// 当前帧截图（全分辨率无损 PNG，不是窗口截屏）。
    pub fn snapshot(src: &Path, at: f64, dest: PathBuf) -> Result<Self, String> {
        let mut cmd = ffmpeg::base_command()?;
        cmd.arg("-ss")
            .arg(format!("{:.3}", at.max(0.0)))
            .arg("-i")
            .arg(src)
            .arg("-frames:v")
            .arg("1")
            .arg("-update")
            .arg("1")
            .arg("-y")
            .arg(&dest);
        Ok(Self::start(
            dest,
            format!("截图 @ {}", crate::media::format_time(at)),
            vec![Step {
                cmd,
                span: 0.0..100.0,
                fallback: false,
            }],
            0.0,
            Vec::new(),
        ))
    }

    /// 抽音轨。先试 `copy`（不重编码，秒出），装不进 m4a 的再转 AAC。
    pub fn audio(src: &Path, dest: PathBuf, duration: f64) -> Result<Self, String> {
        let copy = {
            let mut c = ffmpeg::base_command()?;
            c.arg("-i")
                .arg(src)
                .arg("-vn")
                .arg("-sn")
                .arg("-dn")
                .arg("-map")
                .arg("0:a:0")
                .arg("-c:a")
                .arg("copy")
                .arg("-y")
                .arg(&dest);
            c
        };
        let encode = {
            let mut c = ffmpeg::base_command()?;
            c.arg("-i")
                .arg(src)
                .arg("-vn")
                .arg("-sn")
                .arg("-dn")
                .arg("-map")
                .arg("0:a:0")
                .arg("-c:a")
                .arg("aac")
                .arg("-b:a")
                .arg("192k")
                .arg("-y")
                .arg(&dest);
            c
        };
        Ok(Self::start(
            dest,
            "提取音频".to_string(),
            vec![
                Step {
                    cmd: copy,
                    span: 0.0..100.0,
                    fallback: false,
                },
                Step {
                    cmd: encode,
                    span: 0.0..100.0,
                    fallback: true,
                },
            ],
            duration,
            Vec::new(),
        ))
    }

    /// 从 `start` 起截 `dur` 秒转成 GIF。走标准的两遍调色板法：
    /// 先 palettegen 生成最优调色板，再用 paletteuse 套回去 ——
    /// 单遍 GIF 的色带会非常难看。
    pub fn gif(src: &Path, start: f64, dur: f64, dest: PathBuf) -> Result<Self, String> {
        let dur = dur.max(0.2);
        let palette = temp_path("palette", "png");

        let mut palette_cmd = ffmpeg::base_command()?;
        palette_cmd
            .arg("-ss")
            .arg(format!("{:.3}", start.max(0.0)))
            .arg("-t")
            .arg(format!("{dur:.3}"))
            .arg("-i")
            .arg(src)
            .arg("-vf")
            .arg(format!(
                "fps={GIF_FPS},scale={GIF_WIDTH}:-2:flags=lanczos,palettegen=stats_mode=diff"
            ))
            .arg("-y")
            .arg(&palette);

        let mut use_it = ffmpeg::base_command()?;
        use_it
            .arg("-ss")
            .arg(format!("{:.3}", start.max(0.0)))
            .arg("-t")
            .arg(format!("{dur:.3}"))
            .arg("-i")
            .arg(src)
            .arg("-i")
            .arg(&palette)
            .arg("-lavfi")
            .arg(format!(
                "fps={GIF_FPS},scale={GIF_WIDTH}:-2:flags=lanczos[x];\
                 [x][1:v]paletteuse=dither=bayer:bayer_scale=5:diff_mode=rectangle"
            ))
            .arg("-loop")
            .arg("0")
            .arg("-y")
            .arg(&dest);

        Ok(Self::start(
            dest,
            format!("转 GIF · {GIF_WIDTH}px {GIF_FPS}fps {:.0} 秒", dur),
            vec![
                // 调色板这一遍很快，给它 0–35% 的权重
                Step {
                    cmd: palette_cmd,
                    span: 0.0..35.0,
                    fallback: false,
                },
                Step {
                    cmd: use_it,
                    span: 35.0..100.0,
                    fallback: false,
                },
            ],
            dur,
            vec![palette],
        ))
    }

    pub fn percent(&self) -> u32 {
        self.progress.percent.load(Ordering::Relaxed)
    }

    pub fn cancel(&self) {
        self.progress.cancel.store(true, Ordering::SeqCst);
    }

    /// 跑完了就返回结果，否则 `None`。只会返回一次。
    pub fn poll(&self) -> Option<Result<(), String>> {
        self.progress.outcome.lock().unwrap().take()
    }

    /// 阻塞到任务结束。给命令行自测 / 脚本用，界面里不要调它（会卡住 UI）。
    pub fn wait(&mut self) -> Result<(), String> {
        self.join();
        self.progress
            .outcome
            .lock()
            .unwrap()
            .clone()
            .unwrap_or(Ok(()))
    }

    pub fn join(&mut self) {
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }

    fn start(
        dest: PathBuf,
        label: String,
        steps: Vec<Step>,
        total: f64,
        cleanup: Vec<PathBuf>,
    ) -> Self {
        let progress = Arc::new(Progress::default());
        let p = progress.clone();
        let dest_for_cleanup = dest.clone();
        let handle = std::thread::spawn(move || {
            let stopped = Arc::new(AtomicBool::new(false));
            let mut outcome: Result<(), String> = Ok(());
            for mut step in steps {
                if step.fallback && outcome.is_ok() {
                    continue;
                }
                outcome = run(&mut step.cmd, total, &step.span, &p, &stopped);
                if matches!(&outcome, Err(e) if e == CANCELLED) {
                    break;
                }
            }
            for path in &cleanup {
                let _ = std::fs::remove_file(path);
            }
            if outcome.is_err() {
                // 半成品留着只会让人以为成功了
                let _ = std::fs::remove_file(&dest_for_cleanup);
            } else {
                p.set(100.0);
            }
            *p.outcome.lock().unwrap() = Some(outcome);
        });
        Self {
            dest,
            label,
            progress,
            handle: Some(handle),
        }
    }
}

impl Drop for Export {
    fn drop(&mut self) {
        self.cancel();
        self.join();
    }
}

// ---------------------------------------------------------------------------
// 跑一个 ffmpeg 任务，同时把进度读出来
// ---------------------------------------------------------------------------

fn run(
    cmd: &mut Command,
    total: f64,
    span: &Range<f64>,
    p: &Progress,
    stopped: &AtomicBool,
) -> Result<(), String> {
    cmd.arg("-progress")
        .arg("pipe:1")
        .arg("-nostats")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| format!("无法启动 ffmpeg: {e}"))?;
    let stdout = child.stdout.take().ok_or("拿不到 ffmpeg 的输出管道")?;
    let stderr = child.stderr.take();

    // stderr 必须有人读，否则管道写满 64KB 后 ffmpeg 会卡死
    let err_thread = std::thread::spawn(move || {
        let mut buf = String::new();
        if let Some(mut s) = stderr {
            let _ = s.read_to_string(&mut buf);
        }
        buf
    });

    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    let mut killed = false;

    loop {
        if p.cancel.load(Ordering::SeqCst) {
            let _ = child.kill();
            killed = true;
            break;
        }
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }
        let line = line.trim();
        if let Some(v) = line.strip_prefix("out_time_us=") {
            if let Ok(us) = v.trim().parse::<f64>() {
                let ratio = if total > 0.0 {
                    (us / 1_000_000.0 / total).clamp(0.0, 1.0)
                } else {
                    1.0
                };
                p.set(span.start + ratio * (span.end - span.start));
            }
        } else if line == "progress=end" {
            p.set(span.end);
        }
    }

    let status = child.wait();
    let stderr = err_thread.join().unwrap_or_default();
    stopped.store(true, Ordering::SeqCst);

    if killed {
        return Err(CANCELLED.to_string());
    }
    match status {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(describe(&stderr, s.code())),
        Err(e) => Err(format!("等待 ffmpeg 失败: {e}")),
    }
}

/// ffmpeg 的错误往往有好几行且以 `[xxx @ 0x…]` 打头，掐头去尾只留有用的。
fn describe(stderr: &str, code: Option<i32>) -> String {
    let useful: Vec<&str> = stderr
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect();
    let tail = useful
        .iter()
        .rev()
        .take(2)
        .rev()
        .copied()
        .collect::<Vec<_>>()
        .join("；");
    if tail.is_empty() {
        format!("ffmpeg 退出码 {}", code.unwrap_or(-1))
    } else {
        tail
    }
}

/// 临时文件名（进程号 + 纳秒，够用了），放在系统临时目录。
pub fn temp_path(stem: &str, ext: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("iplayer-{stem}-{}-{nanos}.{ext}", std::process::id()))
}

/// 默认输出路径：和源文件放一起，加一个有意义的后缀。
pub fn default_dest(src: &Path, kind: Kind, tag: Option<&str>) -> PathBuf {
    let dir = src.parent().unwrap_or(Path::new("."));
    let stem = src
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "output".to_string());
    let extra = tag.map(|t| format!("-{t}")).unwrap_or_default();
    dir.join(format!("{stem}{extra}.{}", kind.ext()))
}

/// 文件名里不能出现的字符换成 `-`（时间戳就靠它）。
pub fn safe_tag(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_dest_sits_next_to_source() {
        let p = default_dest(Path::new("/tmp/clip.mp4"), Kind::Snapshot, Some("00-01-05"));
        assert_eq!(p, PathBuf::from("/tmp/clip-00-01-05.png"));

        let p = default_dest(Path::new("/tmp/clip.mp4"), Kind::Audio, None);
        assert_eq!(p, PathBuf::from("/tmp/clip.m4a"));

        let p = default_dest(Path::new("/tmp/clip.mp4"), Kind::Gif, None);
        assert_eq!(p, PathBuf::from("/tmp/clip.gif"));
    }

    #[test]
    fn tags_are_filename_safe() {
        assert_eq!(safe_tag("00:01:05.4"), "00-01-05-4");
        assert_eq!(safe_tag("12:34"), "12-34");
    }

    #[test]
    fn temp_paths_are_unique() {
        let a = temp_path("palette", "png");
        let b = temp_path("palette", "png");
        assert_ne!(a, b);
        assert_eq!(a.extension().unwrap(), "png");
    }

    #[test]
    fn error_keeps_the_useful_tail() {
        let s = "[Parsed_x @ 0x1] Error while opening\n[out#0/gif @ 0x2] Could not write header\n";
        let msg = describe(s, Some(1));
        assert!(msg.contains("Could not write header"));
        assert_eq!(describe("", Some(2)), "ffmpeg 退出码 2");
    }
}
