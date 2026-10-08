mod app;
mod export;
mod ffmpeg;
mod icons;
mod media;
mod native;
mod player;
mod theme;
mod viz;

use std::path::Path;
use std::time::{Duration, Instant};

use gpui_kit::{AppContext as _, WindowBounds, WindowOptions, point, px, size};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.first().map(|s| s.as_str()) == Some("--selftest") {
        let path = args.get(1).expect("用法: iplayer --selftest <media>");
        selftest(path);
        return;
    }

    if args.first().map(|s| s.as_str()) == Some("--info") {
        let st = ffmpeg::status();
        println!("ffmpeg  = {:?}", st.ffmpeg);
        println!("ffprobe = {:?}", st.ffprobe);
        println!("version = {:?}", st.version);
        if let Some(p) = args.get(1) {
            let target = Path::new(p);
            if target.is_dir() {
                match media::scan_dir(target, false) {
                    Ok(list) => {
                        println!("{} 个条目", list.len());
                        for f in list.iter().take(10) {
                            println!("  [{}] {} ({})", f.kind, f.name, media::format_size(f.size));
                        }
                    }
                    Err(e) => eprintln!("扫描失败: {e}"),
                }
            } else {
                match media::probe(target) {
                    Ok(i) => println!(
                        "{}x{} @ {:.3}fps  {}/{} [{}]  {:.1}s  rotate={} sar={}",
                        i.width,
                        i.height,
                        i.fps,
                        i.vcodec,
                        i.acodec,
                        i.pix_fmt,
                        i.duration,
                        i.rotate,
                        i.sar
                    ),
                    Err(e) => eprintln!("探测失败: {e}"),
                }
                // 图片再走一遍真正的解码路径，这样 SVG / AVIF 这些也能验
                let ext = target
                    .extension()
                    .map(|e| e.to_string_lossy().to_lowercase())
                    .unwrap_or_default();
                if media::kind_of(&ext) == Some("image") {
                    match player::load_image(target) {
                        Ok(img) => {
                            let s = img.size(0);
                            println!("图片解码: {}x{} 纹理", s.width.0, s.height.0);
                        }
                        Err(e) => println!("图片解码失败: {e}"),
                    }
                }
            }
        }
        return;
    }

    // 打开一个媒体文件/文件夹：直接起窗口并播它
    let initial = args.first().map(|s| std::path::PathBuf::from(s));
    gui(initial);
}

fn gui(initial: Option<std::path::PathBuf>) {
    gpui_kit::application().run(|cx| {
        gpui_kit::init(cx);

        let options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(1180.), px(760.)), cx)),
            titlebar: Some(gpui_kit::TitlebarOptions {
                title: Some("iPlayer".into()),
                // 自绘标题栏：系统标题栏透明，红绿灯由我们自己让位
                appears_transparent: true,
                traffic_light_position: Some(point(px(14.), px(12.))),
            }),
            app_owns_titlebar_drag: true,
            ..Default::default()
        };

        gpui_kit::open_window(options, cx, move |window, cx| {
            let app = cx.new(|cx| app::App::new(window, cx));
            if let Some(path) = initial.clone() {
                app.update(cx, |app, cx| app.load_initial(path, cx));
            }
            app
        })
        .expect("打开窗口失败");

        cx.activate(true);
    });
}

/// 无窗口自测：验证解码 -> 呈现、seek、时钟是否都工作。
fn selftest(path: &str) {
    let info = match media::probe(Path::new(path)) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("探测失败: {e}");
            return;
        }
    };
    println!(
        "源: {}x{} @ {:.2}fps  {}/{}  {:.1}s  video={} audio={}",
        info.width,
        info.height,
        info.fps,
        info.vcodec,
        info.acodec,
        info.duration,
        info.has_video,
        info.has_audio
    );

    let duration = info.duration;
    let has_video = info.has_video;
    let has_audio = info.has_audio;

    let mut p = match player::Player::open(info) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("打开失败: {e}");
            return;
        }
    };
    if has_video {
        println!("输出画面尺寸: {}x{}", p.size.0, p.size.1);
    }
    println!("音频: {:?}", p.audio_note.as_deref().unwrap_or("已就绪"));

    p.play();
    let (n, pos, playing) = run_for(&mut p, 3.0);
    println!("▶ 前 3 秒：呈现 {n} 帧, 位置 {pos:.2}s, playing={playing}");
    if let Some(a) = &p.audio_note {
        println!("  (音频不可用: {a})");
    }

    // 短素材（动图、铃声）也得能测，所以落点按总时长取，不写死
    let probe_at = (duration * 0.5).clamp(0.2, 10.0);
    p.seek(probe_at);
    let (n2, pos2, _) = run_for(&mut p, 1.5);
    println!("⏩ seek 到 {probe_at:.2}s 后 1.5 秒：呈现 {n2} 帧, 位置 {pos2:.2}s");

    p.set_speed(2.0);
    let t0 = Instant::now();
    let (n3, pos3, _) = run_for(&mut p, 1.5);
    let wall = t0.elapsed().as_secs_f64();
    println!(
        "⏩ 2.0x 播放 {wall:.2} 秒：呈现 {n3} 帧, 位置 {pos3:.2}s (媒体时间推进 {:.2}s)",
        pos3 - pos2
    );

    p.pause();
    let a = p.position();
    std::thread::sleep(Duration::from_millis(300));
    println!("⏸ 暂停后位置稳定: {:.3}s -> {:.3}s", a, p.position());

    // 暂停期间画面必须一动不动
    if has_video {
        let baseline = p.frame().map(|f| f.id.0);
        let mut changes = 0u64;
        for _ in 0..200 {
            if let Some(f) = p.frame() {
                if Some(f.id.0) != baseline {
                    changes += 1;
                }
            }
        }
        println!("⏸ 暂停期间画面变化 = {changes}（应为 0）");

        // 暂停状态下 seek，也必须能拿到新画面（否则界面会一直空着）
        let back = (duration * 0.1).clamp(0.1, 2.0);
        p.seek(back);
        let mut got = false;
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(3) {
            if p.frame().is_some() {
                got = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(4));
        }
        println!(
            "⏸ seek 到 {back:.2}s 后仍能出画面: {}（位置 {:.2}s）",
            if got { "是" } else { "否 ✗" },
            p.position()
        );
    }

    drop(p);

    export_selftest(Path::new(path), has_video, has_audio, duration);
}

/// 导出工具箱自测：真跑一遍三个任务，检查产出的文件是不是像那么回事。
fn export_selftest(src: &Path, has_video: bool, has_audio: bool, duration: f64) {
    let out = std::env::temp_dir().join("iplayer-selftest-export");
    let _ = std::fs::create_dir_all(&out);
    println!("📦 导出工具箱（输出到 {}）", out.display());

    for kind in export::Kind::ALL {
        if kind == export::Kind::Audio && !has_audio {
            println!("  {} 跳过：源文件没有音轨", kind.label());
            continue;
        }
        if kind != export::Kind::Audio && !has_video {
            println!("  {} 跳过：源文件没有画面", kind.label());
            continue;
        }

        let dest = out.join(format!("probe.{}", kind.ext()));
        let _ = std::fs::remove_file(&dest);
        let job = match kind {
            export::Kind::Snapshot => export::Export::snapshot(src, 1.0, dest.clone()),
            export::Kind::Audio => export::Export::audio(src, dest.clone(), duration),
            export::Kind::Gif => export::Export::gif(src, 1.0, 2.0, dest.clone()),
        };
        let mut job = match job {
            Ok(j) => j,
            Err(e) => {
                println!("  {} 起不来: {e}", kind.label());
                continue;
            }
        };
        let t0 = Instant::now();
        let result = job.wait();
        let took = t0.elapsed().as_secs_f64();
        match result {
            Ok(()) => match check_export(&dest, kind) {
                Ok(note) => println!("  {} ✓ {:.2}s · {note}", kind.label(), took),
                Err(e) => println!("  {} ✗ 产出不对: {e}", kind.label()),
            },
            Err(e) => println!("  {} ✗ {e}", kind.label()),
        }
    }

    let _ = std::fs::remove_dir_all(&out);
}

/// 校验导出结果：文件存在、非空、magic 对得上。
fn check_export(path: &Path, kind: export::Kind) -> Result<String, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("产物不存在: {e}"))?;
    if meta.len() == 0 {
        return Err("产物是空文件".to_string());
    }
    let head = std::fs::read(path).map_err(|e| format!("读不回来: {e}"))?;
    let magic: &[u8] = match kind {
        export::Kind::Snapshot => &[0x89, b'P', b'N', b'G'],
        export::Kind::Gif => b"GIF8",
        export::Kind::Audio => b"ftyp", // mp4 系列：大小(4) + 'ftyp'
    };
    let ok = if kind == export::Kind::Audio {
        head.len() > 8 && &head[4..8] == magic
    } else {
        head.starts_with(magic)
    };
    if !ok {
        return Err("文件头不对，可能是个损坏的文件".to_string());
    }
    Ok(format!(
        "{} · {}",
        media::format_size(meta.len()),
        path.file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default()
    ))
}

/// 跑 `secs` 秒，返回（呈现的不同帧数, 位置, 是否在播）。
fn run_for(p: &mut player::Player, secs: f64) -> (u64, f64, bool) {
    let t0 = Instant::now();
    let mut uniq = 0u64;
    let mut last_id = None;
    while t0.elapsed().as_secs_f64() < secs {
        if let Some(f) = p.frame() {
            if Some(f.id.0) != last_id {
                last_id = Some(f.id.0);
                uniq += 1;
            }
        }
        std::thread::sleep(Duration::from_millis(4));
    }
    (uniq, p.position(), p.is_playing())
}
