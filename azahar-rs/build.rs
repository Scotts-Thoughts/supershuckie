//! Compiles the C++ glue (`interface.cpp`) against Azahar's headers.
//!
//! Azahar itself is built by `scripts/build-azahar-spike.ps1` (its core libraries plus the
//! `azahar_bundle` target, which merges every archive the core needs into one
//! `build/azahar/libazahar.a`). The include directories and definitions below are the ones
//! Azahar's own CMake gives its targets on this toolchain (read off the spike's compile line);
//! they must be kept in step when Azahar or its options change.

use cc::Build;
use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let root = manifest_dir.join("..");
    let azahar = root.join("third-party").join("azahar");
    println!("cargo:rerun-if-env-changed=SUPERSHUCKIE_BUILD_DIR");
    let build_dir = env::var_os("SUPERSHUCKIE_BUILD_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("build"));
    let azahar_build = build_dir.join("azahar");

    let mut b = Build::new();
    b.cpp(true);
    b.std("c++20");
    b.file("interface.cpp");
    b.warnings(false);
    // See melonds-rs/build.rs: consumers supply the C++ runtime themselves.
    b.cpp_set_stdlib(None);
    for dir in [
        azahar.join("src"),
        azahar_build.join("src"),
        azahar.join("externals/fmt/include"),
        azahar.join("externals/library-headers/ffmpeg/include"),
        azahar.join("externals/microprofile"),
        azahar.join("externals/boost"),
        azahar.join("externals/xxHash"),
        azahar.join("externals/dds-ktx"),
        azahar.join("externals/xbyak"),
        azahar.join("externals/zstd/lib"),
        azahar.join("externals/glad/include"),
    ] {
        b.include(dir);
    }
    for (k, v) in [
        ("BOOST_ALL_NO_LIB", None),
        ("BOOST_ASIO_DISABLE_CONCEPTS", None),
        ("BOOST_DATE_TIME_NO_LIB", None),
        ("BOOST_ERROR_CODE_HEADER_ONLY", None),
        ("BOOST_REGEX_NO_LIB", None),
        ("BOOST_SYSTEM_NO_LIB", None),
        ("CITRA_HAS_SSE42", None),
        ("ENABLE_OPENGL", None),
        ("ENABLE_SOFTWARE_RENDERER", None),
        ("HAVE_FASTINTERP", None),
        ("MICROPROFILE_ENABLED", Some("0")),
    ] {
        b.define(k, v);
    }
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "windows" {
        for k in ["NOMINMAX", "WIN32_LEAN_AND_MEAN", "UNICODE", "_UNICODE", "MINGW_HAS_SECURE_API"] {
            b.define(k, None);
        }
        b.flag_if_supported("-Wno-attributes");
        b.flag_if_supported("-Wno-interference-size");
        b.flag_if_supported("-Wno-psabi");
    }
    b.flag_if_supported("-msse4.1");
    b.flag_if_supported("-msse4.2");
    b.compile("azahar-rs-interface");
    println!("cargo::rerun-if-changed=interface.cpp");

    if env::var_os("CARGO_FEATURE_LINK_CORES").is_none() {
        return;
    }

    let bundle = azahar_build.join("libazahar.a");
    // A rebuilt core must reach the binaries that link it.
    println!("cargo::rerun-if-changed={}", bundle.display());
    if !bundle.exists() {
        println!(
            "cargo:warning={} not found; build Azahar first (scripts/build-azahar-spike.ps1 -Bundle) or set SUPERSHUCKIE_BUILD_DIR",
            bundle.display()
        );
        return;
    }
    println!("cargo:rustc-link-search=native={}", azahar_build.display());
    println!("cargo:rustc-link-lib=static:-bundle=azahar");
    link_runtime(&target_os);
}

/// The C++ runtime and platform libraries Azahar's archives call into (the list is the spike
/// executable's link line, minus Azahar's own archives which are in the bundle).
fn link_runtime(target_os: &str) {
    match target_os {
        "windows" => {
            for lib in ["libstdc++.a", "libpthread.a"] {
                if let Some(dir) = find_mingw_lib_dir(lib) {
                    println!("cargo:rustc-link-search=native={}", dir.display());
                }
            }
            println!("cargo:rustc-link-lib=static:-bundle=stdc++");
            println!("cargo:rustc-link-lib=static:-bundle=pthread");
            for lib in [
                "crypt32", "ws2_32", "iphlpapi", "gdi32", "opengl32", "winmm", "psapi", "imm32",
                "version", "bcrypt", "user32", "shell32", "ole32", "oleaut32", "uuid", "advapi32",
                "winspool", "comdlg32", "kernel32",
            ] {
                println!("cargo:rustc-link-lib=dylib={lib}");
            }
        }
        "linux" => {
            println!("cargo:rustc-link-lib=stdc++");
            println!("cargo:rustc-link-lib=pthread");
            println!("cargo:rustc-link-lib=GL");
        }
        _ => {}
    }
}

fn find_mingw_lib_dir(lib_filename: &str) -> Option<PathBuf> {
    let compiler = env::var("CXX").unwrap_or_else(|_| "g++".to_string());
    let output = Command::new(&compiler).arg(format!("-print-file-name={lib_filename}")).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let path = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    let dir = path.parent()?;
    if dir.as_os_str().is_empty() {
        return None;
    }
    Some(dir.to_path_buf())
}
