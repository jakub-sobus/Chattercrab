//! Generates the crab `.ico` and embeds it into Windows executables.

use std::path::PathBuf;

#[allow(dead_code)]
#[path = "src/icon.rs"]
mod icon;

fn main() {
    println!("cargo:rerun-if-changed=src/icon.rs");
    println!("cargo:rerun-if-changed=build.rs");

    let Some(out_dir) = std::env::var_os("OUT_DIR").map(PathBuf::from) else {
        println!("cargo:warning=OUT_DIR not set; skipping icon generation");
        return;
    };
    let ico_path = out_dir.join("chattercrab.ico");
    if let Err(e) = std::fs::write(&ico_path, icon::crab_ico(&[16, 24, 32, 48, 64, 256])) {
        println!("cargo:warning=could not write icon: {e}");
        return;
    }

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    // Resource scripts need forward slashes or escaped backslashes.
    let ico = ico_path.display().to_string().replace('\\', "/");
    let rc_path = out_dir.join("chattercrab.rc");
    let rc = format!("1 ICON \"{ico}\"\n");
    if let Err(e) = std::fs::write(&rc_path, rc) {
        println!("cargo:warning=could not write resource script: {e}");
        return;
    }
    match embed_resource::compile(&rc_path, embed_resource::NONE) {
        embed_resource::CompilationResult::Ok | embed_resource::CompilationResult::NotWindows => {}
        other => {
            // Missing rc.exe/windres: the executable just has no embedded icon. The
            // window icon is still set at runtime from the same generator.
            println!("cargo:warning=executable icon not embedded: {other}");
        }
    }
}
