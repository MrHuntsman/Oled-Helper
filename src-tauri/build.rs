// Builds the injected explorer-hook DLL (a standalone cdylib) and stages it
// into OUT_DIR so the main crate can embed it with include_bytes!. This keeps
// the app a single portable exe: the DLL bytes ship inside the exe and are
// extracted to the config dir at runtime before injection.
//
// The hook is built into its own target dir via an explicit --target-dir, so
// the output path stays deterministic even when a user/global
// CARGO_TARGET_DIR or [build].target-dir redirect is in effect. It is built
// for the host target (the same x64 target the app targets), so no
// cross-compile handling is needed here.
fn build_explorer_hook() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is not set");
    let manifest_dir = std::path::Path::new(&manifest_dir);
    let hook_manifest = manifest_dir.join("explorer-hook/Cargo.toml");
    let hook_target = manifest_dir.join("explorer-hook/target");
    let dll = hook_target.join("release/explorer_hook.dll");

    println!("cargo:rerun-if-changed=explorer-hook/Cargo.toml");
    println!("cargo:rerun-if-changed=explorer-hook/src/lib.rs");
    println!("cargo:rerun-if-changed=text-raster/src/lib.rs");
    println!("cargo:rerun-if-changed=text-raster/Cargo.toml");
    println!("cargo:rerun-if-changed=explorer-hook/src/font.rs");
    println!("cargo:rerun-if-changed=explorer-hook/src/detour.rs");

    let status = std::process::Command::new("cargo")
        .args([
            "build",
            "--release",
            "--manifest-path",
            hook_manifest.to_str().expect("hook manifest path is not valid UTF-8"),
            "--target-dir",
            hook_target.to_str().expect("hook target dir path is not valid UTF-8"),
        ])
        .status()
        .expect("failed to run cargo to build explorer-hook");
    assert!(status.success(), "explorer-hook build failed");

    let out = std::env::var("OUT_DIR").expect("OUT_DIR is not set");
    std::fs::copy(&dll, std::path::Path::new(&out).join("explorer_hook.dll"))
        .unwrap_or_else(|e| panic!("failed to copy {} into OUT_DIR: {e}", dll.display()));
}

fn main() {
    tauri_build::build();
    build_explorer_hook();
}
