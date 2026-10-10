mod app;
mod export;
mod ffmpeg;
mod icons;
mod media;
mod native;
mod player;
mod subtitle;
mod theme;
mod viz;

use std::path::Path;
use std::time::{Duration, Instant};

use gpui_kit::{AppContext as _, WindowBounds, WindowOptions, point, px, size};

fn main() {
    install_panic_logger();
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.first().map(|s| s.as_str()) == Some("--selftest") {
        let path = args.get(1).expect("用法: iplayer --selftest <media>");
        selftest(path);
        return;
    }

    // 打包脚本用它生成 Info.plist 里的 CFBundleDocumentTypes —— 扩展名清单
    // 只存在 media.rs 一份，shell 那边不用再抄一遍。
    if args.first().map(|s| s.as_str()) == Some("--doc-types") {
        println!("{}", media::doc_types_plist());
        return;
    }

    // 把 iPlayer 设成媒体文件的默认打开方式（等价于界面里那颗链条图标 / ⌘D）。
    // 必须以 .app 包里的可执行文件身份运行 —— 认的是 bundle identifier。
    if args.first().map(|s| s.as_str()) == Some("--set-default") {
        match native::set_default_role_handler(&media::all_utis()) {
            Ok(n) => println!("已把 iPlayer 设为默认打开方式（{n} 类内容类型）"),
            Err(e) => eprintln!("设置失败：{e}"),
        }
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

/// 把 panic 落到文件里。GUI 程序从 Finder 启动时 stderr 是看不见的，
/// 之前"点截图就崩"查了半天只有一份没有符号的 .ips —— 有了这个，
/// 任何 panic 都会在 `~/Library/Logs/iPlayer/` 留下完整现场（消息 + 位置 + 栈）。
///
/// **钩子本身绝不能再 panic**：`eprintln!` 在 stderr 断管（`... | head`、
/// 被关掉的日志管道）时会自己 panic，此时进程已经在 panic 中，标准库直接
/// abort —— 原始消息和栈全丢。所以这里：先写文件，再尽力写 stderr，
/// 全程只用不会 panic 的 `let _ = ...`。
fn install_panic_logger() {
    std::panic::set_hook(Box::new(|info| {
        use std::io::Write;
        use std::sync::atomic::{AtomicU32, Ordering};

        // 文件名带上毫秒 + 自增序号：一次 abort 里钩子可能被叫两遍
        // （先是原始 panic，然后是 "panic in a function that cannot unwind"），
        // 同名会互相覆盖，把最要紧的第一条消息冲掉。
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let who = std::thread::current()
            .name()
            .map(|s| s.to_string())
            .unwrap_or_else(|| "<未命名线程>".into());
        let body = format!(
            "线程 {who} panic: {}\n\n位置: {}\n\n栈:\n{}",
            info,
            info.location()
                .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
                .unwrap_or_else(|| "<未知>".into()),
            std::backtrace::Backtrace::force_capture()
        );

        // 先落盘：这是唯一可靠的出口（GUI 下 stderr 可能根本没接管道）
        if let Some(home) = std::env::var_os("HOME") {
            let dir = std::path::Path::new(&home).join("Library/Logs/iPlayer");
            if std::fs::create_dir_all(&dir).is_ok() {
                let path = dir.join(format!("crash-{ts}-{seq}.log"));
                let _ = std::fs::write(&path, &body);
            }
        }
        // 再补一份到 stderr（终端里跑的时候方便看），写失败就算了
        let _ = writeln!(std::io::stderr(), "{body}");
    }));
}

fn gui(initial: Option<std::path::PathBuf>) {
    let application = gpui_kit::application();

    // 访达里双击 / "打开方式 → iPlayer" / 拖到 Dock 图标时，macOS 发的是一条
    // Apple Event（aevt/odoc），**文件不在 argv 里**。AppKit 在 finishLaunching
    // 里会给它装自己的处理器，然后把它交给应用委托的 `application:openURLs:` ——
    // gpui-pre 把这个口子透出来了，接上它就行（自己往 NSAppleEventManager 上挂
    // 处理器是白搭，见 native.rs 里那段说明）。
    application.on_open_urls(native::open_urls);

    // 事件到位那一刻没有 GPUI 上下文，只能排进队列 + 拍一下这个通道；
    // 真正取件的是下面挂给窗口的那条常驻任务（帧循环闲着时也得有人看队列）。
    let (open_doc_tx, open_doc_rx) = smol::channel::unbounded();
    native::set_open_doc_wake(open_doc_tx);

    // 事件有时比窗口先到（窗口建得慢），这里先捞一次，剩下的交给 tick / 常驻任务
    let initial = initial.or_else(native::take_open_doc);

    // Dock 图标被点（应用没有可见窗口时）：把藏起来的窗口亮回来。
    // 注意 on_reopen 挂在 Application 上（run 之前注册），回调里才拿到 &mut App。
    application.on_reopen(|cx| show_main_window(cx));
    application.run(move |cx| {
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

        let (any_handle, _) =
            gpui_kit::open_window(options, cx, move |window, cx| {
                let app = cx.new(|cx| app::App::new(window, cx));
                if let Some(path) = initial {
                    app.update(cx, |app, cx| app.load_initial(path, cx));
                }
                // 常驻等"系统让我们打开某个文件"的信号（帧循环闲着的时候，
                // 没有别的路能发现队列里有东西）。
                app::watch_open_docs(&app, open_doc_rx, cx);
                app
            })
            .expect("打开窗口失败");
        MAIN_WINDOW
            .set(any_handle)
            .expect("主窗口只会创建一次");

        // 红绿灯关闭现在是"藏窗口"（见 App::new），所以不存在"窗口全关进程
        // 还赖在 Dock"的问题了；这里不再 quit-on-close。

        // Dock 右键菜单里的「显示主界面」。窗口活跃时动作派发给窗口内的
        // 监听（App 根节点 on_action），否则走这条全局兜底 —— 两条路最终
        // 都落到 native::show_window。
        cx.on_action::<app::ShowMainWindow>(|_, cx| show_main_window(cx));
        cx.set_dock_menu(vec![gpui_kit::MenuItem::action(
            "显示主界面",
            app::ShowMainWindow,
        )]);

        cx.activate(true);
    });
}

/// 主窗口句柄：reopen / Dock 菜单动作都靠它把窗口亮回来。
static MAIN_WINDOW: std::sync::OnceLock<gpui_kit::AnyWindowHandle> = std::sync::OnceLock::new();

fn show_main_window(cx: &mut gpui_kit::App) {
    if let Some(handle) = MAIN_WINDOW.get() {
        let _ = handle.update(cx, |_, window, _| {
            crate::native::show_window(window);
        });
    }
    cx.activate(true);
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

    // 播放中换画面角度：解码进程会被换掉重开，画面和时钟都不能断。
    //
    // 只对"还够长"的片源做这项检查：换角度是带 `-ss 当前位置` 重开解码，
    // 片源已经播完时（短 GIF、单帧图片）seek 到末尾本来就解不出东西，
    // 那是素材没内容，不是换角度坏了。
    if has_video && duration >= 6.0 {
        let before = p.size;
        p.set_orientation(player::Orientation::IDENTITY.rotated_cw());
        let (n5, pos5, playing5) = run_for(&mut p, 1.0);
        let after = p.size;
        let ok = n5 > 0 && playing5 && after == (before.1, before.0);
        println!(
            "🔄 播放中右转 90°：{}x{} -> {}x{}, 1 秒出帧 {}, 位置 {pos5:.2}s, playing={playing5}{}",
            before.0,
            before.1,
            after.0,
            after.1,
            n5,
            if ok { " ✓" } else { " ✗" }
        );
        p.set_orientation(player::Orientation::IDENTITY);
    } else if has_video {
        println!("🔄 播放中右转 90°：片源只有 {duration:.1}s，跳过（seek 无内容）");
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

        // 拖进度条不能黑屏：seek 之后、新画面还没解出来之前，**旧画面必须留在屏上**。
        // 这里不只是"拖一下"，而是像真人那样连续拖动 2 秒（每 100ms 一次预览 seek，
        // 位置在 10%~90% 之间来回扫），这期间像帧循环一样不停取帧、数有多少次取到空 ——
        // 取到空就是用户看到的黑屏闪。
        if duration >= 2.0 && p.frame().is_some() {
            let before = p.frame().map(|f| f.id.0);
            let target = (duration * 0.75).clamp(0.2, duration - 0.2);
            p.scrub(target);
            let kept = before.is_some() && p.frame().map(|f| f.id.0) == before;

            p.play();
            let mut black = 0u64;
            let mut seen = std::collections::HashSet::new();
            let mut lats: Vec<Duration> = Vec::new();
            let t0 = Instant::now();
            let mut i = 0u32;
            while t0.elapsed() < Duration::from_millis(2000) {
                let frac = 0.1 + 0.8 * ((i % 10) as f64 / 9.0);
                let id_before = p.frame().map(|f| f.id.0);
                let t_seek = Instant::now();
                p.scrub(frac * duration);
                let mut lat: Option<Duration> = None;
                while t_seek.elapsed() < Duration::from_millis(100) {
                    match p.frame() {
                        Some(f) => {
                            seen.insert(f.id.0);
                            if lat.is_none() && Some(f.id.0) != id_before {
                                lat = Some(t_seek.elapsed());
                            }
                        }
                        None => black += 1,
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                lats.push(lat.unwrap_or(Duration::from_millis(100)));
                i += 1;
            }
            lats.sort();
            let median = lats.get(lats.len() / 2).copied().unwrap_or_default();
            let slowest = lats.last().copied().unwrap_or_default();
            println!(
                "🎞 模拟拖动 2 秒（{} 次 seek）：seek 瞬间画面不丢 {}、黑屏采样 {black}（应为 0）、\
                 呈现过 {} 帧",
                i,
                if kept { "✓" } else { "✗" },
                seen.len()
            );
            println!(
                "   预览 seek 出帧延迟：中位 {}ms / 最慢 {}ms（拖动节流 {}ms，超过就会一直掐掉还没出帧的解码）{}",
                median.as_millis(),
                slowest.as_millis(),
                crate::app::SCRUB_INTERVAL.as_millis(),
                if kept && black == 0 && seen.len() > 1 {
                    " ✓"
                } else {
                    "  ← 黑屏/卡帧回归"
                }
            );
        }
    }

    // 换画面角度 = 把新滤镜塞进命令行、重开一次解码进程：
    // 必须还能出画面，且呈现尺寸要跟着宽高互换。
    // （单帧图片 duration=0，暂停态 seek 到 0.1s 本来就无帧可解，跳过。）
    if has_video && duration > 0.0 {
        let before = p.size;
        p.set_orientation(player::Orientation::IDENTITY.rotated_cw());
        let (n4, _, _) = run_for(&mut p, 1.0);
        let after = p.size;
        let ok = n4 > 0 && after == (before.1, before.0);
        println!(
            "🔄 右转 90° 之后：{}x{} -> {}x{}, 出帧 {}{}",
            before.0,
            before.1,
            after.0,
            after.1,
            n4,
            if ok { " ✓" } else { " ✗" }
        );
        p.set_orientation(player::Orientation::IDENTITY);
    }

    drop(p);

    orientation_selftest();

    export_selftest(Path::new(path), has_video, has_audio, duration);
}

/// 画面角度自测：现造一张 4x2 的"坐标图"（R/G/B 三个通道各存一位坐标，
/// 全用 0 / 255 的极值，缩放取整不会把两种颜色搅在一起），然后**真跑一遍
/// ffmpeg 滤镜链**，逐个朝向核对每个像素落在哪儿。
///
/// 为什么要真跑：视频那条路是 ffmpeg 的 `transpose/hflip/vflip` 在转，
/// 静态图片那条路是我们在 CPU 上按 `Orientation::map_back` 自己搬像素。
/// 这套自测把两边的语义钉在一起 —— 只要它过了，"右转 90°"在图片和视频上
/// 就是同一个方向。
fn orientation_selftest() {
    use player::Orientation;

    println!("🔄 画面角度（真跑 ffmpeg 核对每个像素的落点）");

    let dir = std::env::temp_dir().join("iplayer-selftest-orient");
    let _ = std::fs::create_dir_all(&dir);
    let png = dir.join("grid.png");

    const W: u32 = 4;
    const H: u32 = 2;
    // 源代码：R = x 的最低位、G = y、B = x 的高位，全部 0/255
    let code = |x: u32, y: u32| -> [u8; 4] {
        [
            if x & 1 == 1 { 255 } else { 0 },
            if y == 1 { 255 } else { 0 },
            if x >> 1 == 1 { 255 } else { 0 },
            255,
        ]
    };

    let mut img = image::RgbaImage::new(W, H);
    for (x, y, p) in img.enumerate_pixels_mut() {
        *p = image::Rgba(code(x, y));
    }
    if let Err(e) = img.save(&png) {
        println!("  ✗ 造不出测试图: {e}");
        return;
    }

    let id = Orientation::IDENTITY;
    let cases: [(&str, Orientation); 6] = [
        ("原始", id),
        ("右转 90°", id.rotated_cw()),
        ("左转 90°", id.rotated_ccw()),
        ("左右翻转", id.flipped_h()),
        ("上下翻转", id.flipped_v()),
        ("右转 90°+左右翻转", id.rotated_cw().flipped_h()),
    ];

    for (name, o) in cases {
        match decode_one_frame(&png, o) {
            Ok((w, h, bytes)) => match check_orientation(&bytes, w, h, o) {
                Ok(()) => println!("  {name} ✓ {w}x{h}"),
                Err(e) => println!("  {name} ✗ {e}"),
            },
            Err(e) => println!("  {name} ✗ {e}"),
        }
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// 用真解码轨解出一帧原始像素（走的就是播放时那条 ffmpeg 命令行）。
fn decode_one_frame(
    path: &Path,
    orient: player::Orientation,
) -> Result<(u32, u32, Vec<u8>), String> {
    let mut track = player::VideoTrack::start(path, 4, 2, 25.0, 0.0)?;
    track.set_orientation(orient, 0.0)?;
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(5) {
        track.advance(0.0);
        if let Some(img) = &track.current {
            let w = img.size(0).width.0 as u32;
            let h = img.size(0).height.0 as u32;
            let bytes = img.as_bytes(0).ok_or("纹理里没有字节")?.to_vec();
            return Ok((w, h, bytes));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Err("5 秒内没解出画面".to_string())
}

/// 逐个输出像素核对：它应当来自 `map_back` 指到的那个源像素。
fn check_orientation(bytes: &[u8], w: u32, h: u32, o: player::Orientation) -> Result<(), String> {
    let (ew, eh) = o.dims(4, 2);
    if (w, h) != (ew, eh) {
        return Err(format!("尺寸 {w}x{h}，期望 {ew}x{eh}"));
    }
    let code = |x: u32, y: u32| -> [u8; 4] {
        [
            if x & 1 == 1 { 255 } else { 0 },
            if y == 1 { 255 } else { 0 },
            if x >> 1 == 1 { 255 } else { 0 },
            255,
        ]
    };
    // GPUI 的缓冲是 BGRA，我们造的源是 RGBA —— 这里把期望值也换成 BGRA
    let bgra = |v: [u8; 4]| [v[2], v[1], v[0], v[3]];

    let mut seen = [false; 8];
    for oy in 0..h {
        for ox in 0..w {
            let (x, y) = o.map_back(ox, oy, 4, 2);
            let i = ((oy * w + ox) as usize) * 4;
            let got = [bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]];
            let want = bgra(code(x, y));
            // 缩放/色彩空间换算可能让 0 变成 3、255 变成 252，给一点容差
            let near = got
                .iter()
                .zip(want.iter())
                .all(|(a, b)| (*a as i32 - *b as i32).abs() <= 6);
            if !near {
                return Err(format!(
                    "输出({ox},{oy}) 得到 {got:?}，按 map_back 应当来自源({x},{y})={want:?}"
                ));
            }
            seen[(y * 4 + x) as usize] = true;
        }
    }
    if seen.iter().any(|s| !s) {
        return Err("有源像素没被搬到输出里".to_string());
    }
    Ok(())
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
