//! ffmpeg / ffprobe sidecar discovery and process helpers.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

/// Compile-time target triple (set by build.rs). Sidecars live in
/// `binaries/ffmpeg-<triple>` during development and are copied next to the
/// main executable as plain `ffmpeg` when the app is bundled.
pub const TARGET_TRIPLE: &str = env!("BUILD_TARGET_TRIPLE");

static FFMPEG: OnceLock<Option<PathBuf>> = OnceLock::new();
static FFPROBE: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Windows 上可执行文件必须带 `.exe`：`Path::is_file()` 不做扩展名补全，
/// 少了这个后缀整个 bundle 的 sidecar 都会找不到。
fn exe_name(name: &str) -> String {
    format!("{name}{}", std::env::consts::EXE_SUFFIX)
}

/// 一个工具要依次尝试的两个文件名：共享名与带目标三元组的名。
fn sidecar_files(name: &str) -> [String; 2] {
    [exe_name(name), exe_name(&format!("{name}-{TARGET_TRIPLE}"))]
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    let wanted = exe_name(name);
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(&wanted);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    // Common Homebrew locations (GUI apps often launch with a minimal PATH).
    for dir in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"] {
        let candidate = Path::new(dir).join(&wanted);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Every location a sidecar could live in, most specific first.
fn candidates(name: &str) -> Vec<PathBuf> {
    let files = sidecar_files(name);
    let mut out: Vec<PathBuf> = Vec::new();

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            // Bundled: right next to the main binary (Contents/MacOS/…, or the .app-adjacent folder)
            for f in &files {
                out.push(dir.join(f));
            }
            // Bundled alternative: Contents/Resources/…
            if let Some(contents) = dir.parent() {
                for f in &files {
                    out.push(contents.join("Resources").join(f));
                }
            }
            // `target/debug/iplayer` -> repo root -> binaries/
            if let Some(root) = dir.parent().and_then(|p| p.parent()) {
                out.push(root.join("binaries").join(&files[1]));
                out.push(root.join("binaries").join(&files[0]));
            }
        }
    }

    // Development: <repo>/binaries/
    let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("binaries");
    out.push(base.join(&files[1]));
    out.push(base.join(&files[0]));

    out
}

/// Verify a candidate actually runs before committing to it — a bundled binary
/// can be present but fail to load (e.g. missing dylibs).
fn works(path: &Path) -> bool {
    Command::new(path)
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn resolve(name: &str, cache: &'static OnceLock<Option<PathBuf>>) -> Option<PathBuf> {
    cache
        .get_or_init(|| {
            for c in candidates(name) {
                if c.is_file() && works(&c) {
                    return Some(c);
                }
            }
            if let Some(p) = find_in_path(name) {
                if works(&p) {
                    return Some(p);
                }
                // Last resort: trust it even if the probe failed.
                return Some(p);
            }
            None
        })
        .clone()
}

pub fn ffmpeg() -> Result<PathBuf, String> {
    resolve("ffmpeg", &FFMPEG)
        .ok_or_else(|| "未找到 ffmpeg。请将 ffmpeg 放入 binaries/ 或安装到系统 PATH。".to_string())
}

pub fn ffprobe() -> Result<PathBuf, String> {
    resolve("ffprobe", &FFPROBE)
        .ok_or_else(|| "未找到 ffprobe。请将 ffprobe 放入 binaries/ 或安装到系统 PATH。".to_string())
}

/// Human-readable picture of what the app found, shown in 设置 → 环境.
#[derive(Clone, Default)]
pub struct ToolStatus {
    pub ffmpeg: Option<String>,
    pub ffprobe: Option<String>,
    pub version: Option<String>,
}

pub fn status() -> ToolStatus {
    let ff = ffmpeg().ok();
    let fp = ffprobe().ok();
    let version = ff
        .as_ref()
        .and_then(|p| Command::new(p).arg("-version").output().ok())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.lines().next().map(|l| l.trim().to_string()));

    ToolStatus {
        ffmpeg: ff.map(|p| p.display().to_string()),
        ffprobe: fp.map(|p| p.display().to_string()),
        version,
    }
}

// ---------------------------------------------------------------------------
// process helpers
// ---------------------------------------------------------------------------

/// A one-shot cancellation flag shared with a long-running ffmpeg job.
///
/// Loop-based ffmpeg work (`-progress pipe:1`, or reading frame by frame) only
/// has to look at the flag between chunks to notice a cancel — no signals, no
/// pid bookkeeping, and it reacts within one pipe read.
#[derive(Default)]
pub struct Cancel {
    flag: AtomicBool,
}

impl Cancel {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }
}

#[cfg(target_os = "windows")]
fn hide_console(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(target_os = "windows"))]
fn hide_console(_cmd: &mut Command) {}

/// Build a base ffmpeg command with sane, quiet defaults.
pub fn base_command() -> Result<Command, String> {
    let bin = ffmpeg()?;
    let mut cmd = Command::new(bin);
    cmd.arg("-hide_banner").arg("-loglevel").arg("error");
    hide_console(&mut cmd);
    Ok(cmd)
}

pub fn base_ffprobe_command() -> Result<Command, String> {
    let bin = ffprobe()?;
    let mut cmd = Command::new(bin);
    cmd.arg("-hide_banner");
    hide_console(&mut cmd);
    Ok(cmd)
}

#[cfg(test)]
mod tests {
    use super::{sidecar_files, TARGET_TRIPLE};

    /// sidecar 命中两种命名：`ffmpeg`(+EXE_SUFFIX) 与 `ffmpeg-<triple>`(+EXE_SUFFIX)。
    /// Windows 少了 `.exe` 就整个找不到，这条测试把它钉住。
    #[test]
    fn sidecar_names_cover_both_spellings() {
        let files = sidecar_files("ffmpeg");
        assert_eq!(files[0], format!("ffmpeg{}", std::env::consts::EXE_SUFFIX));
        assert_eq!(files[1], format!("ffmpeg-{TARGET_TRIPLE}{}", std::env::consts::EXE_SUFFIX));
        #[cfg(target_os = "windows")]
        assert!(files.iter().all(|f| f.ends_with(".exe")));
    }
}
