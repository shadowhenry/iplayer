fn main() {
    // Sidecars ship as `binaries/<tool>-<triple>`; the resolution code needs to
    // know which triple this build targets without guessing at runtime.
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env=BUILD_TARGET_TRIPLE={target}");
    println!("cargo:rerun-if-changed=build.rs");
}
