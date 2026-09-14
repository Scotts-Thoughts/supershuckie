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

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();

    let core_libs: Vec<(PathBuf, &str)> = [
        ("melonDS/src", "core"),
        ("melonDS/src/teakra/src", "teakra"),
        ("mgba", "mgba"),
    ]
    .into_iter()
    .map(|(dir, lib)| (build_dir.join(dir), lib))
    .collect();

    for (dir, lib) in &core_libs {
        if !dir.join(format!("lib{lib}.a")).exists() {
            println!(
                "cargo:warning=lib{lib}.a not found in {}; build the cores first (build.sh) or set SUPERSHUCKIE_BUILD_DIR",
                dir.display()
            );
        }
    }

    if target_os == "windows" {
        // mingw's ld resolves archives in a single left-to-right pass, so the circular
        // references between melonDS/mGBA's own archive members (and the interface glue
        // objects that call into them) don't resolve with plain `-lcore -lteakra -lmgba`.
        // CMake's build works around the equivalent Qt problem with LINK_GROUP:RESCAN; do
        // the same here with an explicit --start-group/--end-group. The system/runtime libs
        // the cores call into (shlwapi's PathRemoveFileSpecW, ole32/shell32's
        // SHGetKnownFolderPath, mingwex/msvcrt's strdup & co.) go in the same group: since
        // rustc appends this link-arg after its own default libs, putting them outside the
        // group would place them before mgba/melonDS in the final command, too early for
        // ld's single pass to still be looking for those symbols.
        let mut group = String::from("-Wl,--start-group");
        for (dir, lib) in &core_libs {
            group.push(',');
            group.push_str(&dir.join(format!("lib{lib}.a")).display().to_string());
        }
        for lib in [
            "shlwapi", "ws2_32", "ole32", "shell32", "mingwex", "msvcrt", "kernel32", "advapi32",
            "uuid", "gcc", "gcc_eh",
        ] {
            group.push(',');
            group.push_str("-l");
            group.push_str(lib);
        }
        // The C++ runtime, as static archives named outright. `-lstdc++` takes mingw's
        // `libstdc++.dll.a` when both flavours are installed, and the server then needs
        // `libstdc++-6.dll`, `libgcc_s_seh-1.dll` and `libwinpthread-1.dll` beside it or on
        // PATH — three files nobody copying one executable knows about, and an exit with
        // 0xC0000135 (STATUS_DLL_NOT_FOUND) before the first byte of protocol when they are
        // missing. `-l:` names the archive file itself, so the runtime goes into the
        // executable and pointing Cutter at the .exe is the whole setup. libstdc++'s thread
        // support comes from libpthread.a, the static winpthread rustc itself links by that
        // name, and it sits in the group for the same ordering reason as everything else.
        for archive in ["libstdc++.a", "libpthread.a"] {
            group.push_str(",-l:");
            group.push_str(archive);
        }
        group.push_str(",--end-group");
        println!("cargo:rustc-link-arg={group}");
        // And tell the driver the same, for any runtime library it adds on its own.
        println!("cargo:rustc-link-arg=-static-libgcc");
        println!("cargo:rustc-link-arg=-static-libstdc++");
    } else {
        for (dir, lib) in &core_libs {
            println!("cargo:rustc-link-search=native={}", dir.display());
            println!("cargo:rustc-link-lib=static={lib}");
        }
    }

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
        "windows" => {}
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
