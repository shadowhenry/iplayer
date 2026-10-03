fn main() {
    // Expose the compile target triple so the runtime can locate the correct
    // ffmpeg/ffprobe sidecar binary (they are shipped as `<name>-<triple>`).
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env=BUILD_TARGET_TRIPLE={target}");

    tauri_build::build()
}
