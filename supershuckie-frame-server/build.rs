//! Links the C/C++ emulator cores.
//!
//! The Qt app gets `libcore.a` (melonDS), `libteakra.a` (its DSP) and `libmgba.a` from CMake;
//! a plain `cargo build` of this binary has to name them itself. They are looked for under
//! `SUPERSHUCKIE_BUILD_DIR` (default: `<repo>/build`, where `build.sh` puts them).

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=SUPERSHUCKIE_BUILD_DIR");

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let build_dir = env::var_os("SUPERSHUCKIE_BUILD_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest_dir.join("..").join("build"));

    for (dir, lib) in [
        ("melonDS/src", "core"),
        ("melonDS/src/teakra/src", "teakra"),
        ("mgba", "mgba"),
    ] {
        let dir = build_dir.join(dir);
        if !dir.join(format!("lib{lib}.a")).exists() {
            println!(
                "cargo:warning=lib{lib}.a not found in {}; build the cores first (build.sh) or set SUPERSHUCKIE_BUILD_DIR",
                dir.display()
            );
        }
        println!("cargo:rustc-link-search=native={}", dir.display());
        println!("cargo:rustc-link-lib=static={lib}");
    }

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    match target_os.as_str() {
        "macos" => {
            println!("cargo:rustc-link-lib=c++");
            println!("cargo:rustc-link-arg=-framework");
            println!("cargo:rustc-link-arg=CoreFoundation");
            // melonDS's availability checks call ___isPlatformVersionAtLeast, which lives in the
            // compiler runtime archive clang links implicitly but rustc does not.
            match find_clang_rt_osx() {
                Some(archive) => println!("cargo:rustc-link-arg={}", archive.display()),
                None => println!("cargo:warning=libclang_rt.osx.a not found; the link may fail on ___isPlatformVersionAtLeast"),
            }
        }
        "linux" => {
            println!("cargo:rustc-link-lib=stdc++");
            println!("cargo:rustc-link-lib=pthread");
        }
        "windows" => {
            // shlwapi: mGBA's vfs/config use PathIsRelativeW/PathRemoveFileSpecW. ws2_32: melonDS.
            println!("cargo:rustc-link-lib=shlwapi");
            println!("cargo:rustc-link-lib=ws2_32");
        }
        _ => {}
    }
}

/// `<toolchain>/usr/lib/clang/<version>/lib/darwin/libclang_rt.osx.a`, located from the clang
/// `xcrun` selects.
fn find_clang_rt_osx() -> Option<PathBuf> {
    let output = Command::new("xcrun").args(["--find", "clang"]).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let clang = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    // .../usr/bin/clang -> .../usr/lib/clang
    let lib_clang = clang.parent()?.parent()?.join("lib").join("clang");
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(&lib_clang)
        .ok()?
        .flatten()
        .map(|e| e.path().join("lib").join("darwin").join("libclang_rt.osx.a"))
        .filter(|p| Path::new(p).exists())
        .collect();
    candidates.sort();
    candidates.pop()
}
