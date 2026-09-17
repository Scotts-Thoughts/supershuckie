use cc::Build;
use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let mut interface_builder = Build::new();
    interface_builder.include("mgba/include");
    interface_builder.cpp(true);
    interface_builder.std("c++20");
    interface_builder.file("interface.cpp");
    interface_builder.warnings(false);
    // See melonds-rs/build.rs: skip cc-rs's automatic `-lstdc++` so it doesn't collide with the
    // static libstdc++.a the frame server links explicitly.
    interface_builder.cpp_set_stdlib(None);
    interface_builder.compile("mgba-rs-interface");

    println!("cargo::rerun-if-changed=interface.cpp");

    // Everything below is only for `cargo test`/`cargo build` of a *dependent* crate (there is
    // no CMake/corrosion or frame-server build.rs around to supply mGBA's static library or
    // the C++ runtime in that case). It is gated on the `link-cores` feature, which is only
    // turned on via [dev-dependencies] in the crates whose test binaries need it, so it stays
    // off for the normal corrosion build of supershuckie-frontend-c.
    if env::var_os("CARGO_FEATURE_LINK_CORES").is_none() {
        return;
    }

    println!("cargo:rerun-if-env-changed=SUPERSHUCKIE_BUILD_DIR");

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let build_dir = env::var_os("SUPERSHUCKIE_BUILD_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest_dir.join("..").join("build"));

    let mgba_dir = build_dir.join("mgba");
    if !mgba_dir.join("libmgba.a").exists() {
        println!(
            "cargo:warning=libmgba.a not found in {}; build the cores first (build.sh) or set SUPERSHUCKIE_BUILD_DIR",
            mgba_dir.display()
        );
        return;
    }

    println!("cargo:rustc-link-search=native={}", mgba_dir.display());
    // The `-bundle` modifier keeps this big, LTO'd archive out of the rlib; it is resolved
    // only at the final binary link, same as CMake's Qt link.
    println!("cargo:rustc-link-lib=static:-bundle=mgba");

    link_runtime();
}

/// The C++ runtime and platform libraries mGBA's static archive calls into. Emitted from both
/// melonds-rs/build.rs and mgba-rs/build.rs when `link-cores` is enabled; duplicate
/// `rustc-link-lib` lines for the same name are harmless to the linker.
fn link_runtime() {
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    match target_os.as_str() {
        "windows" => {
            // Unlike the frame server's build.rs (which hands the linker driver a raw
            // `-l:libstdc++.a`/`-l:libpthread.a` and lets it search its own default lib
            // dirs), `cargo:rustc-link-lib` makes rustc itself resolve the static archive
            // against its `-L` search list first — which does not include MSYS2's `lib`
            // directory by default, so without this it fails with "could not find native
            // static library `stdc++`". Ask g++ where it would find each one instead of
            // hardcoding an MSYS2 install path.
            for lib in ["libstdc++.a", "libpthread.a"] {
                match find_mingw_lib_dir(lib) {
                    Some(dir) => println!("cargo:rustc-link-search=native={}", dir.display()),
                    None => println!(
                        "cargo:warning={lib} not found via `g++ -print-file-name`; the link may fail"
                    ),
                }
            }
            // rustc wraps a `static=` lib in `-Bstatic`, so this resolves to libstdc++.a
            // rather than the mingw `libstdc++.dll.a` import stub a bare `-lstdc++` prefers.
            // The `-bundle` modifier matters here too, and not just for size: with no
            // modifier, `static=` defaults to *bundling* the archive's objects straight into
            // mgba-rs's own rlib at mgba-rs's compile time, which places them on the final
            // link line ahead of `-lmgba`. Since nothing has referenced libstdc++'s symbols
            // yet at that point, ld's single left-to-right pass pulls in none of them and
            // never revisits — producing undefined references to std::filesystem/_Rb_tree/
            // etc. `-bundle` keeps it an external `-lstdc++` reference instead, ordered
            // (like mgba) after the archive that needs it.
            println!("cargo:rustc-link-lib=static:-bundle=stdc++");
            println!("cargo:rustc-link-lib=static:-bundle=pthread");
            println!("cargo:rustc-link-lib=dylib=shlwapi");
            println!("cargo:rustc-link-lib=dylib=ws2_32");
            println!("cargo:rustc-link-lib=dylib=ole32");
            println!("cargo:rustc-link-lib=dylib=shell32");
            println!("cargo:rustc-link-lib=dylib=uuid");
        }
        "linux" => {
            println!("cargo:rustc-link-lib=stdc++");
            println!("cargo:rustc-link-lib=pthread");
        }
        "macos" => {
            println!("cargo:rustc-link-lib=c++");
            println!("cargo:rustc-link-lib=framework=CoreFoundation");
            // mGBA/melonDS's availability checks call ___isPlatformVersionAtLeast, which lives
            // in the compiler runtime archive clang links implicitly but rustc does not.
            // Unlike the frame server's build.rs (which links a binary directly and can use
            // `rustc-link-arg` with the full archive path), this is a library crate's build
            // script: `rustc-link-arg` here would NOT propagate to a dependent's final link,
            // so this uses the same transitive `rustc-link-search`/`rustc-link-lib` pair as
            // the mgba archive above instead.
            match find_clang_rt_osx() {
                Some(archive) => {
                    if let Some(parent) = archive.parent() {
                        println!("cargo:rustc-link-search=native={}", parent.display());
                    }
                    if let Some(libname) = archive
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .and_then(|s| s.strip_prefix("lib"))
                    {
                        // See the windows branch above: `-bundle` avoids the same
                        // bundle-into-rlib ordering trap for this archive too.
                        println!("cargo:rustc-link-lib=static:-bundle={libname}");
                    }
                }
                None => println!(
                    "cargo:warning=libclang_rt.osx.a not found; the link may fail on ___isPlatformVersionAtLeast"
                ),
            }
        }
        _ => {}
    }
}

/// The directory a mingw g++/gcc would find `lib_filename` (e.g. `libstdc++.a`) in, via its
/// own `-print-file-name` query, so this doesn't have to hardcode an MSYS2 install path.
fn find_mingw_lib_dir(lib_filename: &str) -> Option<PathBuf> {
    let compiler = env::var("CXX").unwrap_or_else(|_| "g++".to_string());
    let output = Command::new(&compiler)
        .arg(format!("-print-file-name={lib_filename}"))
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    let dir = path.parent()?;
    if dir.as_os_str().is_empty() {
        // g++ doesn't know the file, so `-print-file-name` just echoed the bare name back.
        return None;
    }
    Some(dir.to_path_buf())
}

/// `<toolchain>/usr/lib/clang/<version>/lib/darwin/libclang_rt.osx.a`, located from the clang
/// `xcrun` selects. Ported from supershuckie-frame-server/build.rs.
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
        .filter(|p| p.exists())
        .collect();
    candidates.sort();
    candidates.pop()
}
