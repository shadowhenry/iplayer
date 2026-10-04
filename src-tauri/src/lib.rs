//! iPlayer — a desktop video player built with Rust + Tauri.

mod commands;
mod ffmpeg;
mod media;
mod stream;

/// WebKit raises `NSInternalInconsistencyException` ("This task has already been
/// stopped") when a custom-protocol response is delivered after the request was
/// cancelled — during heavy scrubbing the video element cancels range requests
/// constantly, so this is expected and harmless: the affected range request is
/// simply retried. `objc2`'s `catch-all` feature (see Cargo.toml) turns that
/// foreign exception into a normal Rust panic that the async runtime catches,
/// which keeps the process alive; this hook only silences the resulting noise
/// while leaving every other panic untouched.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let payload = info.payload();
        let msg = payload
            .downcast_ref::<&str>()
            .map(|s| *s)
            .or_else(|| payload.downcast_ref::<String>().map(|s| s.as_str()))
            .unwrap_or_default();
        if msg.contains("This task has already been stopped") {
            return;
        }
        previous(info);
    }));
}

pub fn run() {
    install_panic_hook();
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            commands::tool_status,
            commands::scan_folder,
            commands::probe_media,
            commands::prepare_playback,
            commands::cancel_playback,
            commands::stream_start,
            commands::stream_read,
            commands::stream_status,
            commands::stream_stop,
            commands::snapshot,
            commands::extract_audio,
            commands::make_gif,
            commands::reveal_in_finder,
            commands::open_path,
            commands::set_always_on_top,
            commands::set_resizable,
            commands::set_fullscreen,
            commands::set_theme,
        ])
        .setup(|_app| {
            // Resolve / verify the ffmpeg sidecars off the UI thread so the
            // first playback does not have to wait for the probe.
            std::thread::spawn(|| {
                let status = ffmpeg::status();
                if status.ffmpeg.is_none() {
                    eprintln!("[iplayer] 未找到 ffmpeg，转封装/截图/GIF 等功能将不可用");
                }
            });

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("启动 iPlayer 失败");

    app.run(|_app, event| {
        // Never leave an ffmpeg behind — neither a live transcode nor a
        // derivative that happens to be half-built.
        if let tauri::RunEvent::Exit = event {
            commands::cancel_build();
            stream::kill_all();
        }
    });
}
