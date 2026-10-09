//! 字幕：SRT / WebVTT / ASS(SSA) 三种文本格式 → 一条按时间排好序的 cue 表。
//!
//! 只做「文本 + 起止时间」这一件事 —— 播放器里字幕就是叠在画面上的一行字，
//! 位置 / 字号由 UI 决定，不解析 ASS 的样式表。三种格式的年月日格式差异
//! （逗号 / 点 / 省略小时）都在 [`parse_time`] 里收口，解析规则各有各的
//! 小节，改动时对照各自的单测。

use std::path::Path;

/// 认得的字幕扩展名（也给打开对话框当过滤器用）。
pub const EXTENSIONS: &[&str] = &["srt", "vtt", "ass", "ssa"];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    Srt,
    Vtt,
    Ass,
}

impl Format {
    /// 按扩展名认格式；不认得返回 `None`（调用方据此判定"这不是字幕文件"）。
    pub fn from_ext(ext: &str) -> Option<Format> {
        match ext.to_ascii_lowercase().as_str() {
            "srt" => Some(Format::Srt),
            "vtt" => Some(Format::Vtt),
            "ass" | "ssa" => Some(Format::Ass),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Format::Srt => "SRT",
            Format::Vtt => "WebVTT",
            Format::Ass => "ASS",
        }
    }
}

/// 一条字幕：`start <= t < end` 期间显示。
#[derive(Clone, Debug, PartialEq)]
pub struct Cue {
    pub start: f64,
    pub end: f64,
    pub text: String,
}

/// 一份载入好的字幕（cue 已按开始时间排好序）。
#[derive(Clone, Debug)]
pub struct Subtitles {
    cues: Vec<Cue>,
    pub format: Format,
    /// 文件名，面板上显示"当前字幕"用
    pub name: String,
}

impl Subtitles {
    pub fn len(&self) -> usize {
        self.cues.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cues.is_empty()
    }

    /// 解析一段字幕文本。`cues` 会按开始时间排序 —— 有些工具导出的 SRT
    /// 里两条字幕的顺序是反的，二分查找容不下没排序的表。
    pub fn parse(text: &str, format: Format, name: impl Into<String>) -> Subtitles {
        let mut cues = match format {
            Format::Srt | Format::Vtt => parse_blocks(text, format),
            Format::Ass => parse_ass(text),
        };
        cues.retain(|c| c.end > c.start && !c.text.is_empty());
        cues.sort_by(|a, b| a.start.partial_cmp(&b.start).unwrap_or(std::cmp::Ordering::Equal));
        Subtitles {
            cues,
            format,
            name: name.into(),
        }
    }

    /// `t` 时刻该显示的字幕（`start <= t < end`）。没有就返回 `None`。
    ///
    /// 允许少量重叠：先二分到"开始时间不晚于 `t` 的最后一串"，
    /// 再往前找几条，取最近的一条覆盖 `t` 的 —— 卡拉 OK 式的叠字就是这样。
    pub fn index_at(&self, t: f64) -> Option<usize> {
        let upto = self.cues.partition_point(|c| c.start <= t);
        (0..upto)
            .rev()
            .take(4)
            .find(|&i| t < self.cues[i].end)
    }

    pub fn at(&self, t: f64) -> Option<&str> {
        self.index_at(t).map(|i| self.cues[i].text.as_str())
    }
}

/// 从磁盘读一份字幕。名字取文件名（面板上显示），读不出来就是错误提示。
pub fn load(path: &Path) -> Result<Subtitles, String> {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let Some(format) = Format::from_ext(&ext) else {
        return Err(format!("不支持的字幕格式：.{ext}"));
    };
    let raw = std::fs::read(path).map_err(|e| format!("读不了字幕：{e}"))?;
    // 字幕文件里 BOM 很常见（Windows 记事本另存），不剥掉第一条的时间戳就认不出来
    let text = String::from_utf8_lossy(&raw);
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| path.display().to_string());
    let subs = Subtitles::parse(text, format, name);
    if subs.is_empty() {
        return Err("这个字幕文件里没有可用的时间轴".to_string());
    }
    Ok(subs)
}

/// 在媒体文件旁边找同名 sidecar 字幕（`movie.mp4` → `movie.srt`），
/// 按 [`EXTENSIONS`] 的顺序取第一个存在的。找不到返回 `None`。
pub fn sidecar(media: &Path) -> Option<std::path::PathBuf> {
    let stem = media.file_stem()?;
    let dir = media.parent().unwrap_or(Path::new("."));
    EXTENSIONS.iter().find_map(|ext| {
        let cand = dir.join(format!("{}.{ext}", stem.to_string_lossy()));
        cand.is_file().then_some(cand)
    })
}

/// SRT / WebVTT：按空行切块，块里含 `-->` 的那一行是时间轴，其后是文本。
fn parse_blocks(text: &str, format: Format) -> Vec<Cue> {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut out = Vec::new();
    for block in text.split("\n\n") {
        let mut lines = block.lines().filter(|l| !l.trim().is_empty());
        let Some(first) = lines.next() else { continue };
        let head = first.trim();
        // VTT 的表头与 NOTE / STYLE / REGION 块整块跳过（它们也可能带 `-->`）
        if head.starts_with("WEBVTT")
            || head.starts_with("NOTE")
            || head.starts_with("STYLE")
            || head.starts_with("REGION")
        {
            continue;
        }
        // 时间轴行：`00:00:01,000 --> 00:00:04,000`（VTT 后面还可能跟 cue 设置）
        let (timeline, body_from) = if head.contains("-->") {
            (head, lines)
        } else {
            match lines.next() {
                Some(l) if l.contains("-->") => (l.trim(), lines),
                _ => continue,
            }
        };
        let (Some(start), Some(end)) = split_times(timeline) else {
            continue;
        };
        let body: Vec<&str> = body_from.map(|l| l.trim_end()).collect();
        let text = clean_text(&body.join("\n"), format);
        out.push(Cue { start, end, text });
    }
    out
}

/// `a --> b` 里抠出两个时刻。
fn split_times(line: &str) -> (Option<f64>, Option<f64>) {
    let Some((a, b)) = line.split_once("-->") else {
        return (None, None);
    };
    // 右边可能还跟着 VTT 的 cue 设置（align:start position:10% …），取第一个词
    let b = b.trim().split_whitespace().next().unwrap_or("");
    (parse_time(a.trim()), parse_time(b))
}

/// 时刻文本 → 秒。接受 `hh:mm:ss,mmm` / `hh:mm:ss.mmm` / `mm:ss.mmm` / `ss.mmm`
/// 与 ASS 的 `h:mm:ss.cc`。
fn parse_time(s: &str) -> Option<f64> {
    let s = s.trim().replace(',', ".");
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() > 3 || parts.is_empty() {
        return None;
    }
    let mut sec = 0.0;
    for (i, p) in parts.iter().rev().enumerate() {
        let v: f64 = p.trim().parse().ok()?;
        sec += v * 60f64.powi(i as i32);
    }
    sec.is_finite().then_some(sec)
}

/// ASS / SSA：`[Events]` 段里每行 `Dialogue:`，字段顺序由上面的 `Format:` 行给出。
fn parse_ass(text: &str) -> Vec<Cue> {
    let mut out = Vec::new();
    let mut in_events = false;
    // 默认就是 ASS 标准的字段顺序，遇到 Format: 再按它重排
    let mut idx = (1usize, 2usize, 9usize);
    for raw in text.lines() {
        let line = raw.trim();
        if line.starts_with('[') {
            in_events = line.eq_ignore_ascii_case("[events]");
            continue;
        }
        if !in_events {
            continue;
        }
        if let Some(rest) = strip_prefix_ci(line, "format:") {
            let names: Vec<String> = rest
                .split(',')
                .map(|s| s.trim().to_ascii_lowercase())
                .collect();
            let find = |k: &str| names.iter().position(|n| n == k);
            if let (Some(s), Some(e), Some(t)) = (find("start"), find("end"), find("text")) {
                idx = (s, e, t);
            }
            continue;
        }
        let Some(rest) = strip_prefix_ci(line, "dialogue:") else {
            continue;
        };
        // Text 是最后一个字段，本身可能含逗号 → 只切前 N 个
        let fields: Vec<&str> = rest.splitn(idx.2 + 1, ',').collect();
        let pick = |i: usize| fields.get(i).map(|s| s.trim()).unwrap_or("");
        let (Some(start), Some(end)) = (parse_time(pick(idx.0)), parse_time(pick(idx.1))) else {
            continue;
        };
        let text = clean_text(pick(idx.2), Format::Ass);
        out.push(Cue { start, end, text });
    }
    out
}

fn strip_prefix_ci<'a>(line: &'a str, prefix: &str) -> Option<&'a str> {
    let head = line.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix).then(|| &line[prefix.len()..])
}

/// 去掉标记、还原换行与实体，得到能直接画到屏幕上的字。
///
/// - ASS：`{\an8}`、`{\pos(...)}` 这类覆盖块整块删；`\N` / `\n` 是硬/软换行；`\h` 是不换行空格
/// - SRT / VTT：`<i>`、`<v 张三>`、`<00:00:01.000>` 这类标签删掉；实体还原
///
/// 两种标记**各管各的**：ASS 的正文里允许出现 `<`（不是标签），
/// SRT 的正文里也可能有花括号 —— 按格式只认自己那一套，别把用户的话吃掉。
fn clean_text(raw: &str, format: Format) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if format == Format::Ass => {
                // ASS 覆盖块：吞到配对的 '}'（没配对就吞到行尾）
                for c in chars.by_ref() {
                    if c == '}' {
                        break;
                    }
                }
            }
            '<' if format != Format::Ass => {
                // 标签：整段吞到 '>'。SRT 里理论上不该有，但真文件里 `<i>` 满天飞
                for c in chars.by_ref() {
                    if c == '>' {
                        break;
                    }
                }
            }
            '\\' => match chars.peek() {
                Some('N') | Some('n') => {
                    chars.next();
                    out.push('\n');
                }
                Some('h') => {
                    chars.next();
                    out.push(' ');
                }
                _ => out.push('\\'),
            },
            _ => out.push(c),
        }
    }
    let out = decode_entities(&out);
    // 逐个 cue 收尾：行尾空白清掉、多于两行的空行压掉
    out.lines()
        .map(|l| l.trim())
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

/// 只还原最常见的几个实体 —— 字幕里够用，不必上完整的 HTML 解析器。
fn decode_entities(s: &str) -> String {
    s.replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn srt_basic() {
        let src = "1\n00:00:01,000 --> 00:00:04,000\n你好，世界\n\n2\n00:00:05,500 --> 00:00:07,250\n第二句\n";
        let s = Subtitles::parse(src, Format::Srt, "a.srt");
        assert_eq!(s.len(), 2);
        assert_eq!(s.cues[0].start, 1.0);
        assert_eq!(s.cues[0].end, 4.0);
        assert_eq!(s.cues[0].text, "你好，世界");
        assert_eq!(s.cues[1].start, 5.5);
        assert_eq!(s.at(0.5), None, "还没到第一条");
        assert_eq!(s.at(1.0), Some("你好，世界"), "开始时刻就该显示");
        assert_eq!(s.at(3.999), Some("你好，世界"));
        assert_eq!(s.at(4.0), None, "结束时刻不再显示");
        assert_eq!(s.at(5.6), Some("第二句"));
        assert_eq!(s.at(99.0), None);
    }

    /// Windows 记事本另存的 BOM + CRLF + 多行文本 + 缺序号行，都得认。
    #[test]
    fn srt_messy_real_world_file() {
        let src = "\u{feff}1\r\n00:00:00,000 --> 00:00:02,000\r\n第一行\r\n第二行\r\n\r\n\r\n00:00:03,000 --> 00:00:04,000\r\n没有序号也能认\r\n";
        let s = Subtitles::parse(src, Format::Srt, "b.srt");
        assert_eq!(s.len(), 2, "多行文本要合成一条，不该被空行切开");
        assert_eq!(s.cues[0].text, "第一行\n第二行");
        assert_eq!(s.cues[1].text, "没有序号也能认");
    }

    #[test]
    fn vtt_header_note_and_tags() {
        let src = "WEBVTT\n\nNOTE 这是注释\n00:00:00,000 --> 00:00:01,000\n不该被当成字幕\n\n\
01:00.000 --> 01:02.000 align:start position:10%\n<i>斜体</i> &amp; 实体\n\n\
00:00:05.000 --> 00:00:06.000\n<v 张三>说话的人\n";
        let s = Subtitles::parse(src, Format::Vtt, "c.vtt");
        assert_eq!(s.len(), 2, "NOTE 块要跳过");
        // 表里按开始时间排序：晚一点的 5s 那条排前，省略小时的 60s 那条排后
        assert_eq!(s.cues[0].text, "说话的人");
        assert_eq!(s.cues[1].start, 60.0, "VTT 允许省略小时（mm:ss.mmm）");
        assert_eq!(s.cues[1].text, "斜体 & 实体");
        assert_eq!(s.at(60.5), Some("斜体 & 实体"));
    }

    #[test]
    fn ass_dialogue_lines() {
        let src = "[Script Info]\nTitle: 测试\n\n[V4+ Styles]\nFormat: Name, Fontname\nStyle: Default,Arial\n\n\
[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n\
Dialogue: 0,0:00:01.00,0:00:03.50,Default,,0,0,0,,{\\an8}上面的一句话\n\
Dialogue: 0,0:00:04.00,0:00:06.00,Default,,0,0,0,,第一行\\N第二行\n\
Comment: 0,0:00:07.00,0:00:08.00,Default,,0,0,0,,注释不该显示\n";
        let s = Subtitles::parse(src, Format::Ass, "d.ass");
        assert_eq!(s.len(), 2, "Comment 行不是字幕");
        assert_eq!(s.cues[0].text, "上面的一句话", "ASS 覆盖块要删掉");
        assert_eq!(s.cues[1].text, "第一行\n第二行", "\\N 是换行");
        assert_eq!(s.at(2.0), Some("上面的一句话"));
        assert_eq!(s.at(5.0), Some("第一行\n第二行"));
    }

    /// cue 顺序是乱的（有的工具会这样）也得能查 —— 解析时要重排。
    #[test]
    fn unsorted_cues_are_sorted_on_parse() {
        let src = "1\n00:00:10,000 --> 00:00:12,000\n后面那条\n\n2\n00:00:01,000 --> 00:00:02,000\n前面那条\n";
        let s = Subtitles::parse(src, Format::Srt, "e.srt");
        assert_eq!(s.cues[0].text, "前面那条");
        assert_eq!(s.at(1.5), Some("前面那条"));
        assert_eq!(s.at(11.0), Some("后面那条"));
    }

    /// 叠字（后一条在时间上和前一条重叠）取最近开始的够用就好，但绝不能返回 None。
    #[test]
    fn overlapping_cues_still_resolve() {
        let src = "1\n00:00:00,000 --> 00:00:10,000\n底层\n\n2\n00:00:02,000 --> 00:00:12,000\n上层\n";
        let s = Subtitles::parse(src, Format::Srt, "f.srt");
        assert!(s.index_at(5.0).is_some(), "重叠区间必须有字幕可选");
    }

    /// 空内容 / 起止写反的条目不进表。
    #[test]
    fn junk_entries_are_dropped() {
        let src = "1\n00:00:05,000 --> 00:00:03,000\n时间反了\n\n2\n00:00:06,000 --> 00:00:07,000\n\n\n3\n没有时间轴\n";
        let s = Subtitles::parse(src, Format::Srt, "g.srt");
        assert!(s.is_empty(), "反的 / 空的 / 没时间轴的全都不该进来，实际 {:?}", s.cues);
    }

    #[test]
    fn time_formats() {
        assert_eq!(parse_time("00:00:01,500"), Some(1.5));
        assert_eq!(parse_time("01:02:03.250"), Some(3723.25));
        assert_eq!(parse_time("02:03.500"), Some(123.5));
        assert_eq!(parse_time("0:00:03.50"), Some(3.5));
        assert_eq!(parse_time("7.5"), Some(7.5));
        assert_eq!(parse_time("abc"), None);
        assert_eq!(parse_time("1:2:3:4"), None);
    }

    /// 标记只按自家格式处理：SRT 里的花括号是正文，ASS 里的 `<` 也是正文。
    #[test]
    fn markers_are_format_specific() {
        let srt = Subtitles::parse(
            "1\n00:00:01,000 --> 00:00:02,000\n{这不是覆盖块} 而 <i>这是标签</i>\n",
            Format::Srt,
            "h.srt",
        );
        assert_eq!(srt.cues[0].text, "{这不是覆盖块} 而 这是标签");

        let ass = Subtitles::parse(
            "[Events]\nDialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,{\\an8}3 < 5 且 a>b\n",
            Format::Ass,
            "i.ass",
        );
        assert_eq!(ass.cues[0].text, "3 < 5 且 a>b");
    }

    #[test]
    fn format_from_extension() {
        assert_eq!(Format::from_ext("SRT"), Some(Format::Srt));
        assert_eq!(Format::from_ext("vtt"), Some(Format::Vtt));
        assert_eq!(Format::from_ext("ssa"), Some(Format::Ass));
        assert_eq!(Format::from_ext("txt"), None);
        assert!(EXTENSIONS.contains(&"ass"));
    }

    /// 真的从磁盘读一份（含 BOM），名字进 `name`、格式按扩展名判定。
    #[test]
    fn load_from_disk_with_bom() {
        let p = std::env::temp_dir().join("iplayer-sub-load-test.srt");
        std::fs::write(&p, "\u{feff}1\n00:00:01,000 --> 00:00:02,000\n磁盘上的字幕\n").unwrap();
        let s = load(&p).expect("应当读得出来");
        assert_eq!(s.name, "iplayer-sub-load-test.srt");
        assert_eq!(s.format, Format::Srt);
        assert_eq!(s.at(1.5), Some("磁盘上的字幕"));
        let _ = std::fs::remove_file(&p);
    }

    /// 认不出的扩展名 / 空时间轴要给一句人话，而不是静默空表。
    #[test]
    fn load_rejects_junk() {
        let p = std::env::temp_dir().join("iplayer-sub-junk.txt");
        std::fs::write(&p, "这就不是字幕").unwrap();
        assert!(load(&p).unwrap_err().contains("格式"));

        let p2 = std::env::temp_dir().join("iplayer-sub-empty.srt");
        std::fs::write(&p2, "\n\n\n").unwrap();
        assert!(load(&p2).unwrap_err().contains("时间轴"));
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(&p2);
    }

    /// 同名 sidecar：`movie.mp4` → `movie.srt`。
    #[test]
    fn sidecar_lookup() {
        let dir = std::env::temp_dir().join("iplayer-sidecar-test");
        std::fs::create_dir_all(&dir).unwrap();
        let movie = dir.join("movie.mp4");
        std::fs::write(&movie, b"x").unwrap();
        std::fs::write(dir.join("movie.ass"), "[Events]\n").unwrap();
        assert_eq!(sidecar(&movie), Some(dir.join("movie.ass")));
        assert_eq!(sidecar(&dir.join("other.mp4")), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
