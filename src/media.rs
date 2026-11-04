//! Media discovery and probing.
//!
//! Playback is fully native (ffmpeg decodes straight to frames), so there is no
//! "can the webview take this?" planning step any more — every format ffmpeg
//! can open is playable as-is. What is left here is scanning folders and asking
//! ffprobe what a file actually contains.

use std::fs;
use std::path::Path;
use std::time::UNIX_EPOCH;

use serde::Serialize;

pub const VIDEO_EXTS: &[&str] = &[
    "mp4", "m4v", "mov", "mkv", "avi", "flv", "wmv", "webm", "mpg", "mpeg", "m2v", "ts", "m2ts",
    "mts", "rmvb", "rm", "3gp", "3g2", "ogv", "vob", "asf", "divx", "f4v", "mxf", "dv", "amv",
];
pub const AUDIO_EXTS: &[&str] = &[
    "mp3", "m4a", "aac", "flac", "wav", "wave", "ogg", "oga", "opus", "wma", "ape", "alac", "aif",
    "aiff", "mka", "ac3", "dts", "amr",
];
pub const IMAGE_EXTS: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "webp", "bmp", "svg", "svgz", "ico", "tif", "tiff", "avif", "exr",
    "psd",
];

/// 我们对外声明的"能打开什么"。**只有这一份**，三个地方都从它派生：
/// `kind_of`（扫描时判类型）、文件对话框的过滤器、以及打进 `Info.plist` 的
/// `CFBundleDocumentTypes`（`iplayer --doc-types` 吐给打包脚本）。
///
/// 为什么非要扩展名而不仅是 UTI：`rmvb`、`ape`、`ts`、`flv` 这类在 macOS 上
/// **根本没有对应的 UTI**，只声明 `public.movie` / `public.audio` 的话，
/// 它们在访达的"打开方式"里永远看不到 iPlayer。所以每个扩展名都单独申一个
/// `com.iplayer.<ext>`（见 [`doc_types_plist`] 的 `UTImportedTypeDeclarations`），
/// 并声明它 conform 到 `public.movie` / `public.audio`，LaunchServices 这才会把
/// ".mkv 归 iPlayer 管"记进数据库 —— 否则双击还是用别的播放器开。
pub struct DocType {
    /// 给系统看的中文名，会出现在"打开方式"列表的说明里
    pub name: &'static str,
    /// 大类内容类型（UTI）。`CFBundleDocumentTypes` 会引用它们，
    /// 设默认打开方式时也是基础。
    pub utis: &'static [&'static str],
    /// 具体子类 UTI：这些格式在 macOS 上有独立 UTI，单独设默认比只设大类更稳
    ///（系统按具体 UTI 查默认时不会回退到父类）。设默认失败（系统不认这个 UTI）
    /// 会被 [`native::set_default_role_handler`] 静默跳过，所以可以放心列全。
    pub set_utis: &'static [&'static str],
    /// 是否声明为默认（`LSHandlerRank=Default`）。音视频为 true —— 这就是
    /// "默认用 iPlayer 打开"的关键；图片为 false，只进"打开方式"、不抢 Preview。
    pub rank_default: bool,
    pub exts: &'static [&'static str],
}

pub const DOC_TYPES: &[DocType] = &[
    DocType {
        name: "视频",
        utis: &["public.movie", "public.video"],
        set_utis: &[
            "public.mpeg",
            "public.mpeg-4",
            "com.apple.quicktime-movie",
            // 别名 / 备用的 matroska、wma UTI：不同机器上这两个格式的权威 UTI
            // 可能不同（本机 mkv 是 com.blurayplayersoftware.matroska、wma 是
            // com.microsoft.windows-media-wma），多设几个候选，哪个存在就争哪个；
            // 不存在的会被 LSSetDefaultRoleHandler 静默跳过，无害。
            "org.matroska.mkv",
            "com.microsoft.wma",
        ],
        rank_default: true,
        exts: VIDEO_EXTS,
    },
    DocType {
        name: "音频",
        utis: &["public.audio"],
        set_utis: &[
            "public.mp3",
            "public.mpeg-4-audio",
            "com.apple.m4a-audio",
            "public.aiff-audio",
            "com.microsoft.waveform-audio",
            "org.xiph.ogg-audio",
            "org.xiph.flac",
            "public.aac-audio",
            "public.ac3-audio",
            // wma 的备用 UTI（本机是 com.microsoft.windows-media-wma，见 std_uti）
            "com.microsoft.wma",
        ],
        rank_default: true,
        exts: AUDIO_EXTS,
    },
    DocType {
        name: "图片",
        utis: &["public.image", "public.svg-image"],
        set_utis: &[],
        rank_default: false,
        exts: IMAGE_EXTS,
    },
];

/// 所有支持的扩展名（文件对话框的过滤器用）。
pub fn all_exts() -> Vec<&'static str> {
    DOC_TYPES
        .iter()
        .flat_map(|d| d.exts.iter().copied())
        .collect()
}

/// 所有要设成默认的 UTI（`--set-default` / 界面里那颗链条图标逐个去设）。
///
/// 只收 `rank_default` 的音视频组：大类 + 具体子类 + 每个扩展名的自定义 UTI。
/// 图片不在内，避免抢 Preview 的默认。返回 `String` 是因为自定义的
/// `com.iplayer.<ext>` 是运行时拼出来的。
pub fn all_utis() -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |u: String| {
        if !out.iter().any(|x| *x == u) {
            out.push(u);
        }
    };
    for d in DOC_TYPES {
        if !d.rank_default {
            continue;
        }
        for u in d.utis {
            push(u.to_string());
        }
        for u in d.set_utis {
            push(u.to_string());
        }
        for e in d.exts {
            push(format!("com.iplayer.{e}"));
            // 也设标准 UTI（mp4→com.apple.quicktime-movie、mkv→org.matroska.mkv、
            // wma→com.microsoft.wma…）：文件双击时系统解析到的是它而不是我们的自定义
            // UTI，不设默认的话 QuickTime / VLC / XLPlayer 这种「认领了标准 UTI」的
            // 应用就会继续当默认。set_utis 只覆盖一部分，这里用 std_uti 把其余冷门
            // 标准 UTI 也一起设上。
            if let Some(s) = std_uti(e) {
                push(s.to_string());
            }
        }
    }
    out
}

/// 某些扩展名在 macOS 上有**标准的** UTI，文件双击时系统解析到的就是它
///（而不是我们自申的 `com.iplayer.<ext>`）。要想让 iPlayer 当这些文件的默认，
/// 必须也「认领」标准 UTI —— 在 `CFBundleDocumentTypes` 里引用它、并把它
/// 加进 [`all_utis`] 让 `LSSetDefaultRoleHandler` 去设默认。返回 `None` 表示
/// 这个扩展名没有公认标准 UTI（系统里压根不存在，只能用我们自申的
/// `com.iplayer.<ext>`，靠它去争默认）。
///
/// ⚠️ 这里的映射必须和**系统实际的** UTI 对齐，否则 `LSSetDefaultRoleHandler`
/// 会吃 `paramErr(-50)` 直接跳过，那个格式就永远争不到默认。映射不是拍脑袋
/// 想的 —— 用 `UTTypeCreatePreferredIdentifierForTag("public.filename-extension", …)`
/// 在目标机器上查过（`src/native.rs` 的改文件关联流程里也依赖它）。例如：
/// `.mp4` → `public.mpeg-4`（不是 quicktime-movie）、`.mkv` →
/// `com.blurayplayersoftware.matroska`（不是 `org.matroska.mkv`，那个在本机
/// 根本没注册）、`.wma` → `com.microsoft.windows-media-wma`、`.opus` →
/// `org.xiph.ogg-audio`（opus 在系统里没有独立 UTI，和 ogg 共用）。
fn std_uti(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "mp4" => "public.mpeg-4",
        "mov" => "com.apple.quicktime-movie",
        "m4v" => "com.apple.m4v-video",
        "mkv" => "com.blurayplayersoftware.matroska",
        "webm" => "org.webmproject.webm",
        "mpg" | "mpeg" => "public.mpeg",
        "m2v" => "public.mpeg-2-video",
        "m2ts" | "mts" => "public.avchd-mpeg-2-transport-stream",
        "ts" => "public.mpeg-2-transport-stream",
        "mp3" => "public.mp3",
        "m4a" => "com.apple.m4a-audio",
        "aac" => "public.aac-audio",
        "flac" => "org.xiph.flac",
        "wav" | "wave" => "com.microsoft.waveform-audio",
        "ogg" | "oga" | "opus" => "org.xiph.ogg-audio",
        "wma" => "com.microsoft.windows-media-wma",
        "wmv" => "com.microsoft.windows-media-wmv",
        "avi" => "public.avi",
        "flv" | "f4v" => "com.adobe.flash.video",
        "rmvb" | "rm" => "com.real.realmedia-vbr",
        "aif" | "aiff" => "public.aiff-audio",
        "ac3" => "public.ac3-audio",
        _ => return None,
    })
}

/// `iplayer --doc-types` 吐给打包脚本的 XML 片段（嵌进 `Info.plist`）。
///
/// 两段：
/// ① `UTImportedTypeDeclarations` —— 给没有系统 UTI 的格式（mkv/avi/flv/rmvb/wma…）
///    各自申一个 `com.iplayer.<ext>`，声明它 conform 到 `public.movie` / `public.audio`
///    / `public.image`。没有这段，这些扩展名在系统里就没有"主人"，永远关联不上本应用。
/// ② `CFBundleDocumentTypes` —— 声明本应用能打开这些类型。`rank_default` 的音视频
///    用 `LSHandlerRank=Default`（这就是"默认用 iPlayer 打开"的关键）；图片用
///    `Alternate`，只进"打开方式"、不抢 Preview。
///
/// 与其在 shell 里再抄一遍几十个扩展名（抄漏一个那个格式就永远关联不上），
/// 不如让这里当唯一来源。
pub fn doc_types_plist() -> String {
    // ① UTImportedTypeDeclarations
    let mut imported = String::from("<key>UTImportedTypeDeclarations</key>\n    <array>\n");
    for d in DOC_TYPES {
        let parent = d.utis[0]; // public.movie / public.audio / public.image
        for e in d.exts {
            let u = format!("com.iplayer.{e}");
            imported.push_str("        <dict>\n");
            imported.push_str(&format!(
                "            <key>UTTypeIdentifier</key><string>{u}</string>\n"
            ));
            imported.push_str(
                "            <key>UTTypeTagSpecification</key>\n            <dict>\n",
            );
            imported.push_str(&format!(
                "                <key>public.filename-extension</key>\n                <array><string>{e}</string></array>\n"
            ));
            imported.push_str("            </dict>\n");
            imported.push_str("            <key>UTTypeConformsTo</key>\n            <array>\n");
            imported.push_str(&format!("                <string>{parent}</string>\n"));
            imported.push_str("            </array>\n");
            imported.push_str(&format!(
                "            <key>UTTypeDescription</key><string>iPlayer {e} 文件</string>\n"
            ));
            imported.push_str("        </dict>\n");
        }
    }
    imported.push_str("    </array>\n");

    // ② CFBundleDocumentTypes
    let mut types = String::from("<key>CFBundleDocumentTypes</key>\n    <array>\n");
    for d in DOC_TYPES {
        types.push_str("        <dict>\n");
        types.push_str(&format!(
            "            <key>CFBundleTypeName</key><string>{}</string>\n",
            d.name
        ));
        // Viewer：只读打开。
        types.push_str("            <key>CFBundleTypeRole</key><string>Viewer</string>\n");
        let rank = if d.rank_default { "Default" } else { "Alternate" };
        types.push_str(&format!(
            "            <key>LSHandlerRank</key><string>{rank}</string>\n"
        ));
        types.push_str("            <key>LSItemContentTypes</key>\n            <array>\n");
        for u in d.utis {
            types.push_str(&format!("                <string>{u}</string>\n"));
        }
        for e in d.exts {
            types.push_str(&format!("                <string>com.iplayer.{e}</string>\n"));
            // 也认领标准 UTI（mp4→com.apple.quicktime-movie、mkv→org.matroska.mkv…）：
            // 文件双击时系统解析到的是它，不认领就争不到默认。
            if let Some(s) = std_uti(e) {
                types.push_str(&format!("                <string>{s}</string>\n"));
            }
        }
        types.push_str("            </array>\n");
        types.push_str("            <key>CFBundleTypeExtensions</key>\n            <array>\n");
        for e in d.exts {
            types.push_str(&format!("                <string>{e}</string>\n"));
        }
        types.push_str("            </array>\n        </dict>\n");
    }
    types.push_str("    </array>");

    format!("{imported}{types}")
}

#[derive(Serialize, Clone)]
pub struct MediaFile {
    pub name: String,
    pub path: String,
    pub ext: String,
    pub size: u64,
    pub modified: u64,
    /// one of: `video` | `audio` | `image`
    pub kind: String,
}

pub fn kind_of(ext: &str) -> Option<&'static str> {
    if VIDEO_EXTS.contains(&ext) {
        Some("video")
    } else if AUDIO_EXTS.contains(&ext) {
        Some("audio")
    } else if IMAGE_EXTS.contains(&ext) {
        Some("image")
    } else {
        None
    }
}

fn modified_secs(meta: &fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn scan_dir(dir: &Path, recursive: bool) -> Result<Vec<MediaFile>, String> {
    if !dir.is_dir() {
        return Err(format!("不是一个文件夹: {}", dir.display()));
    }
    let mut out = Vec::new();
    collect(dir, recursive, &mut out, 0)?;
    out.sort_by(|a, b| natural_cmp(&a.name, &b.name));
    Ok(out)
}

fn collect(dir: &Path, recursive: bool, out: &mut Vec<MediaFile>, depth: u32) -> Result<(), String> {
    if depth > 6 {
        return Ok(());
    }
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.is_dir() {
            if recursive {
                collect(&path, true, out, depth + 1)?;
            }
            continue;
        }
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if let Some(kind) = kind_of(&ext) {
            out.push(MediaFile {
                name,
                path: path.to_string_lossy().to_string(),
                ext,
                size: meta.len(),
                modified: modified_secs(&meta),
                kind: kind.to_string(),
            });
        }
    }
    Ok(())
}

/// "ep2" sorts before "ep10".
pub fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let mut ai = a.char_indices().peekable();
    let mut bi = b.char_indices().peekable();
    loop {
        match (ai.peek(), bi.peek()) {
            (None, None) => return std::cmp::Ordering::Equal,
            (None, Some(_)) => return std::cmp::Ordering::Less,
            (Some(_), None) => return std::cmp::Ordering::Greater,
            (Some((_, ac)), Some((_, bc))) => {
                let (ac, bc) = (*ac, *bc);
                if ac.is_ascii_digit() && bc.is_ascii_digit() {
                    let mut an = String::new();
                    while let Some((_, c)) = ai.peek() {
                        if c.is_ascii_digit() {
                            an.push(*c);
                            ai.next();
                        } else {
                            break;
                        }
                    }
                    let mut bn = String::new();
                    while let Some((_, c)) = bi.peek() {
                        if c.is_ascii_digit() {
                            bn.push(*c);
                            bi.next();
                        } else {
                            break;
                        }
                    }
                    let a_num: u128 = an.parse().unwrap_or(0);
                    let b_num: u128 = bn.parse().unwrap_or(0);
                    match a_num.cmp(&b_num) {
                        std::cmp::Ordering::Equal => continue,
                        other => return other,
                    }
                } else {
                    let al = ac.to_lowercase().next().unwrap_or(ac);
                    let bl = bc.to_lowercase().next().unwrap_or(bc);
                    match al.cmp(&bl) {
                        std::cmp::Ordering::Equal => {
                            ai.next();
                            bi.next();
                        }
                        other => return other,
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// GIF 结构解析
// ---------------------------------------------------------------------------

/// GIF 的几个关键指标。只走块头、不碰 LZW 数据，所以再大的图也是微秒级。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GifInfo {
    /// 逻辑屏幕尺寸
    pub width: u32,
    pub height: u32,
    /// 图像描述符个数 = 帧数。1 表示静态图。
    pub frames: usize,
}

impl GifInfo {
    pub fn is_animated(&self) -> bool {
        self.frames > 1
    }
}

/// 解析 GIF 头与所有块的长度字段，数出一共几帧。
///
/// GIF 的布局是：6 字节签名 + 7 字节逻辑屏幕描述符 + 可选全局调色板，
/// 然后是一串块 —— `0x2C` 图像描述符、`0x21` 扩展、`0x3B` 结束。
/// 每个块的长度都在块头里写着，所以顺序跳过就行。
pub fn gif_info(path: &Path) -> Option<GifInfo> {
    use std::io::{Read as _, Seek as _, SeekFrom};

    let file = fs::File::open(path).ok()?;
    let mut r = std::io::BufReader::new(file);
    let mut head = [0u8; 13];
    r.read_exact(&mut head).ok()?;
    if &head[0..3] != b"GIF" {
        return None;
    }
    let width = u16::from_le_bytes([head[6], head[7]]) as u32;
    let height = u16::from_le_bytes([head[8], head[9]]) as u32;
    let packed = head[10];

    // 全局调色板：3 字节一项，2^(N+1) 项
    if packed & 0x80 != 0 {
        let entries = 1u64 << ((packed & 0x07) + 1);
        r.seek(SeekFrom::Current((3 * entries) as i64)).ok()?;
    }

    let mut frames = 0usize;
    let mut one = [0u8; 1];
    loop {
        if r.read_exact(&mut one).is_err() {
            break;
        }
        match one[0] {
            // 图像描述符
            0x2C => {
                let mut desc = [0u8; 9];
                if r.read_exact(&mut desc).is_err() {
                    break;
                }
                frames += 1;
                let local = desc[8];
                if local & 0x80 != 0 {
                    let entries = 1u64 << ((local & 0x07) + 1);
                    if r.seek(SeekFrom::Current((3 * entries) as i64)).is_err() {
                        break;
                    }
                }
                // LZW 最小码长 1 字节 + 数据子块
                if r.read_exact(&mut one).is_err() || skip_sub_blocks(&mut r).is_err() {
                    break;
                }
            }
            // 扩展块：先吃掉标签字节，再按子块跳过
            0x21 => {
                if r.read_exact(&mut one).is_err() || skip_sub_blocks(&mut r).is_err() {
                    break;
                }
            }
            // 结束符
            0x3B => break,
            // 认不出来的字节：放弃解析，当作静态图
            _ => break,
        }
        // 帧数够多就够判断"是不是动画"了，不必读完
        if frames > 64 {
            break;
        }
    }

    if frames == 0 {
        return None;
    }
    Some(GifInfo {
        width,
        height,
        frames,
    })
}

/// 子块序列：`长度 + 数据` 重复，直到长度为 0。
fn skip_sub_blocks<R: std::io::Read + std::io::Seek>(r: &mut R) -> std::io::Result<()> {
    // 方法由 trait bound 提供，不用再 use 一次
    use std::io::SeekFrom;
    let mut len = [0u8; 1];
    loop {
        r.read_exact(&mut len)?;
        if len[0] == 0 {
            return Ok(());
        }
        r.seek(SeekFrom::Current(len[0] as i64))?;
    }
}

// ---------------------------------------------------------------------------
// probing
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
struct ProbeStream {
    #[serde(default)]
    codec_type: String,
    #[serde(default)]
    codec_name: String,
    #[serde(default)]
    codec_long_name: String,
    #[serde(default)]
    pix_fmt: String,
    #[serde(default)]
    profile: String,
    #[serde(default)]
    width: u32,
    #[serde(default)]
    height: u32,
    #[serde(default)]
    avg_frame_rate: String,
    #[serde(default)]
    r_frame_rate: String,
    #[serde(default)]
    sample_aspect_ratio: String,
    #[serde(default)]
    channels: u32,
    #[serde(default)]
    channel_layout: String,
    #[serde(default)]
    sample_rate: String,
    #[serde(default)]
    bit_rate: String,
    #[serde(default)]
    has_b_frames: u32,
    /// 视频旋转角度（手机竖拍常见 90/270），来自 side data 的 displaymatrix。
    #[serde(default)]
    tags: StreamTags,
}

#[derive(serde::Deserialize, Default)]
struct StreamTags {
    #[serde(default)]
    rotate: String,
}

/// 容器级标签（MP3 的 ID3、FLAC/OGG 的 Vorbis comment 都出现在这里）。
#[derive(serde::Deserialize, Default)]
struct FormatTags {
    /// 曲名。没打过标签就是空串 —— 调用方自己退回文件名。
    #[serde(default)]
    title: String,
}

#[derive(serde::Deserialize)]
struct ProbeFormat {
    #[serde(default)]
    format_long_name: String,
    #[serde(default)]
    duration: String,
    #[serde(default)]
    tags: FormatTags,
    #[serde(default)]
    bit_rate: String,
    #[serde(default)]
    size: String,
}

#[derive(serde::Deserialize)]
struct ProbeRoot {
    #[serde(default)]
    streams: Vec<ProbeStream>,
    #[serde(default)]
    format: Option<ProbeFormat>,
}

/// Everything the info panel and the playback pipeline need to know.
#[derive(Serialize, Clone, Default)]
pub struct MediaInfo {
    pub path: String,
    pub ext: String,
    /// 容器标签里的曲名（ID3 `title` 等）。没标签就是空串，
    /// 音频舞台显示时再退回文件名 —— 见 `app::song_title`。
    pub title: String,
    /// one of: `video` | `audio`
    pub kind: String,
    pub duration: f64,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub vcodec: String,
    pub vcodec_long: String,
    pub acodec: String,
    pub acodec_long: String,
    /// e.g. `yuv420p`, `yuv420p10le`
    pub pix_fmt: String,
    /// e.g. `High`, `High 10`, `High 4:4:4 Predictive`
    pub profile: String,
    pub format_name: String,
    pub bitrate: u64,
    pub size: u64,
    pub channels: u32,
    pub channel_layout: String,
    pub sample_rate: u32,
    pub audio_bitrate: u64,
    pub has_video: bool,
    pub has_audio: bool,
    /// >0 when the video stream reorders frames (B-frames).
    pub has_b_frames: u32,
    /// 旋转角度（0/90/180/270）。渲染时需要交换宽高。
    pub rotate: u32,
    /// 像素宽高比，`1:1` 表示方形像素。
    pub sar: String,
    /// 帧数。只有 GIF 会填（自己数的），其它一律 0 = 未知。
    pub frames: u32,
}

fn parse_fps(s: &str) -> f64 {
    let mut parts = s.split('/');
    let num = parts.next().and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
    let den = parts.next().and_then(|v| v.parse::<f64>().ok()).unwrap_or(1.0);
    if den > 0.0 && num > 0.0 { num / den } else { 0.0 }
}

pub fn probe(path: &Path) -> Result<MediaInfo, String> {
    let mut cmd = super::ffmpeg::base_ffprobe_command()?;
    cmd.arg("-v")
        .arg("error")
        .arg("-print_format")
        .arg("json")
        .arg("-show_format")
        .arg("-show_streams")
        .arg(path);

    let out = super::ffmpeg::run_output(&mut cmd).map_err(|e| format!("无法运行 ffprobe: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("无法解析媒体信息: {}", err.trim()));
    }

    let root: ProbeRoot =
        serde_json::from_slice(&out.stdout).map_err(|e| format!("解析媒体信息失败: {e}"))?;

    let video = root.streams.iter().find(|s| s.codec_type == "video");
    let audio = root.streams.iter().find(|s| s.codec_type == "audio");
    let fmt = root.format.as_ref();

    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    let has_video = video.is_some();
    let has_audio = audio.is_some();

    // GIF：ffprobe 报的帧率对动图不太可靠，自己数一遍帧数最踏实。
    let gif = (ext == "gif").then(|| gif_info(path)).flatten();
    let frames = gif.map(|g| g.frames as u32).unwrap_or(0);
    let duration = fmt
        .map(|f| f.duration.parse::<f64>().unwrap_or(0.0))
        .unwrap_or(0.0);
    let mut fps = video
        .map(|v| {
            let a = parse_fps(&v.avg_frame_rate);
            if a > 0.0 { a } else { parse_fps(&v.r_frame_rate) }
        })
        .unwrap_or(0.0);
    if fps <= 0.0 && duration > 0.0 && frames > 1 {
        // 兜底：动画 GIF 的平均帧率 = 帧数 / 总时长
        fps = frames as f64 / duration;
    }

    Ok(MediaInfo {
        path: path.to_string_lossy().to_string(),
        ext,
        title: fmt
            .map(|f| f.tags.title.trim().to_string())
            .unwrap_or_default(),
        kind: if has_video { "video" } else { "audio" }.to_string(),
        duration,
        width: video.map(|v| v.width).unwrap_or(0),
        height: video.map(|v| v.height).unwrap_or(0),
        fps,
        vcodec: video.map(|v| v.codec_name.clone()).unwrap_or_default(),
        vcodec_long: video.map(|v| v.codec_long_name.clone()).unwrap_or_default(),
        acodec: audio.map(|a| a.codec_name.clone()).unwrap_or_default(),
        acodec_long: audio.map(|a| a.codec_long_name.clone()).unwrap_or_default(),
        pix_fmt: video.map(|v| v.pix_fmt.clone()).unwrap_or_default(),
        profile: video.map(|v| v.profile.clone()).unwrap_or_default(),
        format_name: fmt.map(|f| f.format_long_name.clone()).unwrap_or_default(),
        bitrate: fmt
            .map(|f| f.bit_rate.parse::<u64>().unwrap_or(0))
            .unwrap_or(0),
        size: fmt
            .map(|f| f.size.parse::<u64>().unwrap_or(0))
            .unwrap_or(0),
        channels: audio.map(|a| a.channels).unwrap_or(0),
        channel_layout: audio
            .map(|a| a.channel_layout.clone())
            .unwrap_or_default(),
        sample_rate: audio
            .and_then(|a| a.sample_rate.parse::<u32>().ok())
            .unwrap_or(0),
        audio_bitrate: audio
            .and_then(|a| a.bit_rate.parse::<u64>().ok())
            .unwrap_or(0),
        has_video,
        has_audio,
        has_b_frames: video.map(|v| v.has_b_frames).unwrap_or(0),
        rotate: video
            .and_then(|v| v.tags.rotate.parse::<u32>().ok())
            .filter(|r| matches!(r, 90 | 180 | 270))
            .unwrap_or(0),
        sar: video
            .map(|v| v.sample_aspect_ratio.clone())
            .unwrap_or_default(),
        frames,
    })
}


/// `3725.4` -> `01:02:05`
pub fn format_time(secs: f64) -> String {
    if !secs.is_finite() || secs < 0.0 {
        return "00:00".to_string();
    }
    let total = secs.floor() as u64;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h:02}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

/// `"1.5 GB"` — for the info panel.
pub fn format_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b < KB {
        format!("{bytes} B")
    } else if b < KB * KB {
        format!("{:.1} KB", b / KB)
    } else if b < KB * KB * KB {
        format!("{:.1} MB", b / (KB * KB))
    } else {
        format!("{:.2} GB", b / (KB * KB * KB))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_order() {
        let mut v = vec!["ep10".to_string(), "ep2".to_string(), "ep1".to_string()];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(v, vec!["ep1", "ep2", "ep10"]);
    }

    #[test]
    fn fps_parse() {
        assert_eq!(parse_fps("30000/1001"), 30000.0 / 1001.0);
        assert_eq!(parse_fps("0/0"), 0.0);
        // 只有分子时按分母 1 处理
        assert_eq!(parse_fps("25"), 25.0);
    }

    #[test]
    fn time_and_size() {
        assert_eq!(format_time(65.9), "01:05");
        assert_eq!(format_time(3725.0), "01:02:05");
        assert_eq!(format_time(-1.0), "00:00");
        assert_eq!(format_size(1536), "1.5 KB");
    }

    #[test]
    fn kinds() {
        assert_eq!(kind_of("mkv"), Some("video"));
        assert_eq!(kind_of("flac"), Some("audio"));
        assert_eq!(kind_of("webp"), Some("image"));
        assert_eq!(kind_of("svg"), Some("image"));
        assert_eq!(kind_of("txt"), None);
    }

    /// 写一个真 · 两帧 GIF 出来，确认我们自己数的帧数和尺寸都对。
    #[test]
    fn gif_frames_are_counted() {
        use image::codecs::gif::GifEncoder;
        use image::{Frame as ImgFrame, Rgba, RgbaImage};

        let dir = std::env::temp_dir().join("iplayer-media-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("anim.gif");

        let mut a = RgbaImage::new(4, 3);
        for px in a.pixels_mut() {
            *px = Rgba([255, 0, 0, 255]);
        }
        let mut b = RgbaImage::new(4, 3);
        for px in b.pixels_mut() {
            *px = Rgba([0, 0, 255, 255]);
        }

        {
            let file = fs::File::create(&path).unwrap();
            let mut enc = GifEncoder::new(file);
            enc.encode_frame(ImgFrame::new(a)).unwrap();
            enc.encode_frame(ImgFrame::new(b)).unwrap();
        }

        let info = gif_info(&path).expect("应当解析成功");
        assert_eq!(info.frames, 2);
        assert_eq!((info.width, info.height), (4, 3));
        assert!(info.is_animated());

        // 单帧 GIF 不算动画
        let still_path = dir.join("still.gif");
        {
            let file = fs::File::create(&still_path).unwrap();
            let mut enc = GifEncoder::new(file);
            enc.encode_frame(ImgFrame::new(RgbaImage::new(4, 3))).unwrap();
        }
        let still = gif_info(&still_path).expect("应当解析成功");
        assert_eq!(still.frames, 1);
        assert!(!still.is_animated());

        // 非 GIF 直接放弃
        assert!(gif_info(Path::new("/definitely/not/here.png")).is_none());

        std::fs::remove_file(&path).ok();
        std::fs::remove_file(&still_path).ok();
    }

    /// 打进 Info.plist 的文档类型片段必须正确：
    /// ① 给没有系统 UTI 的冷门格式申了 `com.iplayer.<ext>` 并 conform 到父类；
    /// ② 音视频 LSHandlerRank=Default（默认用 iPlayer 打开），图片 Alternate（不抢 Preview）。
    #[test]
    fn doc_types_plist_declares_custom_utis_and_default_rank() {
        let plist = doc_types_plist();

        // 自定义 UTI 与 conform 关系
        assert!(plist.contains("UTImportedTypeDeclarations"), "缺少 UTImportedTypeDeclarations");
        assert!(plist.contains("com.iplayer.mkv"), "mkv 这种无 UTI 格式必须申自定义 UTI");
        assert!(plist.contains("com.iplayer.wma"), "wma 也必须申自定义 UTI");
        assert!(plist.contains("public.filename-extension"), "必须声明扩展名标签");
        assert!(plist.contains("UTTypeConformsTo"), "必须声明 conform 关系");

        // 音视频默认、图片不默认
        assert!(
            plist.contains("LSHandlerRank</key><string>Default</string>"),
            "音视频必须 LSHandlerRank=Default 才能成为默认播放器"
        );
        assert!(
            plist.contains("LSHandlerRank</key><string>Alternate</string>"),
            "图片必须 LSHandlerRank=Alternate，不抢 Preview"
        );

        // 整体得是合法 plist 片段
        assert!(plist.contains("CFBundleDocumentTypes"), "缺少 CFBundleDocumentTypes");
        // Default 应该恰好出现 2 次（视频 + 音频），图片是 Alternate ——
        // 数量兜底：一旦有人把图片也设成 Default，这里立刻红。
        assert_eq!(
            plist.matches("LSHandlerRank</key><string>Default</string>").count(),
            2,
            "Default 应该恰好出现 2 次（视频 + 音频），图片不应是 Default"
        );
    }

    /// `all_utis` 只收音视频（用于「设为默认」），不含图片，且覆盖具体子类 UTI。
    #[test]
    fn all_utis_covers_audio_video_not_images() {
        let utis = all_utis();
        assert!(utis.iter().any(|u| u == "public.movie"), "应含视频大类");
        assert!(utis.iter().any(|u| u == "public.audio"), "应含音频大类");
        assert!(
            utis.iter().any(|u| u == "com.iplayer.mkv"),
            "应含冷门格式的自定义 UTI"
        );
        assert!(
            utis.iter().any(|u| u == "public.mp3"),
            "应含具体子类 UTI（mp3）"
        );
        // 标准 UTI 也要进「设为默认」列表（修文件关联的关键）
        assert!(
            utis.iter().any(|u| u == "public.mpeg-4"),
            "mp4 的标准 UTI 应进列表"
        );
        assert!(
            utis.iter().any(|u| u == "com.blurayplayersoftware.matroska"),
            "mkv 的实际标准 UTI 应进列表"
        );
        assert!(
            utis.iter().any(|u| u == "com.microsoft.windows-media-wma"),
            "wma 的实际标准 UTI 应进列表"
        );
        assert!(
            !utis.iter().any(|u| u == "public.image"),
            "图片不该进「设为默认」列表，否则抢 Preview"
        );
        assert!(
            !utis.iter().any(|u| u.contains("iplayer.jpg")),
            "图片扩展名不该进默认列表"
        );
    }

    /// `std_uti` 的映射必须和**系统实际的** UTI 对齐，否则 `LSSetDefaultRoleHandler`
    /// 会吃 paramErr 跳过、那个格式永远争不到默认。这里钉住几个曾经错过的：
    /// mp4 不是 quicktime-movie、mkv 不是 org.matroska.mkv（本机没注册）、
    /// wma 不是 com.microsoft.wma、opus 和 ogg 共用 org.xiph.ogg-audio。
    #[test]
    fn std_uti_matches_system_utis() {
        assert_eq!(std_uti("mp4"), Some("public.mpeg-4"));
        assert_eq!(std_uti("mov"), Some("com.apple.quicktime-movie"));
        assert_eq!(std_uti("m4v"), Some("com.apple.m4v-video"));
        assert_eq!(std_uti("mkv"), Some("com.blurayplayersoftware.matroska"));
        assert_eq!(std_uti("wma"), Some("com.microsoft.windows-media-wma"));
        assert_eq!(std_uti("wmv"), Some("com.microsoft.windows-media-wmv"));
        assert_eq!(std_uti("avi"), Some("public.avi"));
        assert_eq!(std_uti("flv"), Some("com.adobe.flash.video"));
        assert_eq!(std_uti("ts"), Some("public.mpeg-2-transport-stream"));
        assert_eq!(std_uti("m2ts"), Some("public.avchd-mpeg-2-transport-stream"));
        assert_eq!(std_uti("rmvb"), Some("com.real.realmedia-vbr"));
        assert_eq!(std_uti("opus"), Some("org.xiph.ogg-audio"));
        // 旧的错误映射绝不能回来
        assert_ne!(std_uti("mp4"), Some("com.apple.quicktime-movie"));
        assert_ne!(std_uti("mkv"), Some("org.matroska.mkv"));
        assert_ne!(std_uti("wma"), Some("com.microsoft.wma"));
        assert_ne!(std_uti("opus"), Some("org.xiph.opus"));
        // 没标准 UTI 的冷门格式返回 None，靠自定义 com.iplayer.<ext> 去争
        assert_eq!(std_uti("rmvb"), Some("com.real.realmedia-vbr"));
    }
}
