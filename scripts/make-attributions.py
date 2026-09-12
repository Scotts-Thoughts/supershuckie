#!/usr/bin/env python3
"""Assemble the libraries-and-attributions/ folder that ships next to the SuperShuckie executable.

Everything statically linked into (or embedded in) the executable has a license that
requires its text and copyright notice to travel with binary distributions: GPL/LGPL/MPL
for SuperShuckie itself and the emulator cores, MIT/BSD/zlib/Apache/Unicode for the
Rust crates and the C libraries that Qt pulls in. This script gathers those texts from
their authoritative locations at build time:

  * this repository            (COPYING, bootrom licenses, vendored notices in licenses/)
  * the emulator submodules    (melonDS, mGBA and their bundled third-party code)
  * the cargo registry         (every crate in the dependency graph, via `cargo metadata`)
  * the MSYS2 prefix           (Qt, SDL3, Qt's external libraries, the MinGW runtime,
                                the Rust standard library)

and writes them into --out with a README.txt index. It exits non-zero if anything it
expects is missing, so a new dependency without a license file breaks the build rather
than shipping unattributed.

Run by CMake (see supershuckie-qt/CMakeLists.txt); can also be run by hand:
    python scripts/make-attributions.py --out build-static/libraries-and-attributions
"""

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
from pathlib import Path

LICENSE_FILE_PREFIXES = ("LICENSE", "LICENCE", "COPYING", "NOTICE", "UNLICENSE")

# MSYS2 packages whose code is linked into the Windows executable, in the order they are
# listed in the README. (short name, share/licenses subdirectory, license id, description)
MSYS2_PACKAGES = [
    ("sdl3", "SDL3", "Zlib", "Simple DirectMedia Layer 3 (controller input)"),
    ("freetype", "freetype", "FTL OR GPL-2.0-or-later", "FreeType font engine (used by Qt)"),
    ("harfbuzz", "harfbuzz", "MIT-Modern-Variant (Old MIT)", "HarfBuzz text shaping (used by Qt)"),
    ("graphite2", "graphite2", "LGPL-2.1-or-later OR MPL-2.0 OR GPL-2.0-or-later", "Graphite2 smart-font rendering (used by HarfBuzz)"),
    ("libpng", "libpng", "libpng-2.0", "libpng (used by Qt and mGBA)"),
    ("zlib", "zlib", "Zlib", "zlib (used by Qt, libpng and mGBA)"),
    ("libjpeg-turbo", "libjpeg-turbo", "IJG AND BSD-3-Clause AND Zlib", "libjpeg-turbo (Qt JPEG image plugin)"),
    ("libtiff", "libtiff", "libtiff", "libtiff (Qt TIFF image plugin)"),
    ("libwebp", "libwebp", "BSD-3-Clause", "libwebp (Qt WebP image plugin)"),
    ("libdeflate", "libdeflate", "MIT", "libdeflate (used by libtiff)"),
    ("jbigkit", "jbigkit", "GPL-2.0-or-later", "JBIG-KIT (used by libtiff)"),
    ("lerc", "lerc", "Apache-2.0", "LERC raster compression (used by libtiff)"),
    ("xz", "xz", "0BSD (liblzma)", "XZ Utils / liblzma (used by libtiff)"),
    ("zstd", "zstd", "BSD-3-Clause OR GPL-2.0-only", "Zstandard (used by libtiff)"),
    ("libb2", "libb2", "CC0-1.0", "libb2 BLAKE2 (used by Qt Core)"),
    ("pcre2", "pcre2", "BSD-3-Clause WITH PCRE2-exception", "PCRE2 regular expressions (used by Qt Core and GLib)"),
    ("glib2", "glib2", "LGPL-2.1-or-later", "GLib (used by HarfBuzz)"),
    ("gettext-runtime", "gettext-runtime", "LGPL-2.1-or-later (libintl)", "gettext runtime / libintl (used by GLib)"),
    ("libiconv", "libiconv", "LGPL-2.1-or-later", "GNU libiconv (used by GLib)"),
    ("brotli", "brotli", "MIT", "Brotli (used by FreeType)"),
    ("bzip2", "bzip2", "bzip2-1.0.6", "bzip2 (used by FreeType)"),
    ("gcc-libs", "gcc-libs", "GPL-3.0-or-later WITH GCC-exception-3.1", "GCC runtime libraries: libgcc, libstdc++ (statically linked)"),
    ("crt", "crt", "ZPL-2.1 AND public domain AND BSD AND LGPL (see COPYING.MinGW-w64-runtime.txt)", "mingw-w64 C runtime"),
    ("libwinpthread", "libwinpthread", "MIT AND BSD-3-Clause", "winpthreads (statically linked)"),
]

# Bundled third-party files inside the melonDS submodule that must ship, as
# (path relative to the submodule, name in the output directory).
MELONDS_LICENSE_FILES = [
    ("LICENSE", "LICENSE"),
    ("src/teakra/LICENSE", "teakra-LICENSE"),
    ("src/blip-buf/license.txt", "blip-buf-license.txt"),
    ("src/dolphin/license_dolphin.txt", "dolphin-license.txt"),
    ("src/fatfs/LICENSE.txt", "fatfs-LICENSE.txt"),
    ("src/tiny-AES-c/unlicense.txt", "tiny-AES-c-unlicense.txt"),
]

MGBA_LICENSE_FILES = [
    ("LICENSE", "LICENSE"),
    ("res/licenses/inih.txt", "inih-LICENSE.txt"),
]


class AttributionError(Exception):
    pass


def run(cmd, cwd=None):
    result = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True, encoding="utf-8")
    if result.returncode != 0:
        raise AttributionError(f"{' '.join(str(c) for c in cmd)} failed:\n{result.stderr}")
    return result.stdout


def git(repo, *args):
    try:
        return run(["git", "-C", str(repo), *args]).strip()
    except (AttributionError, FileNotFoundError):
        return None


def copy_required(src: Path, dst: Path):
    if not src.is_file():
        raise AttributionError(f"required license file is missing: {src}")
    dst.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(src, dst)


def copy_tree_required(src: Path, dst: Path):
    if not src.is_dir():
        raise AttributionError(f"required license directory is missing: {src}")
    shutil.copytree(src, dst, dirs_exist_ok=True)


def write_text(path: Path, text: str):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8", newline="\n")


def license_files_in(directory: Path):
    return sorted(
        p for p in directory.iterdir()
        if p.is_file() and p.name.upper().startswith(LICENSE_FILE_PREFIXES)
    )


def workspace_version(repo: Path) -> str:
    text = (repo / "Cargo.toml").read_text(encoding="utf-8")
    m = re.search(r'^\[workspace\.package\][^\[]*?^version\s*=\s*"([^"]+)"', text, re.M | re.S)
    return m.group(1) if m else "unknown"


# --------------------------------------------------------------------------------------
# SuperShuckie itself
# --------------------------------------------------------------------------------------

def section_supershuckie(repo: Path, out: Path, index: list):
    dst = out / "supershuckie"
    copy_required(repo / "COPYING", dst / "COPYING")
    copy_required(repo / "supershuckie-qt" / "icon-credits.txt", dst / "icon-credits.txt")

    version = workspace_version(repo)
    commit = git(repo, "rev-parse", "HEAD") or "unknown"
    describe = git(repo, "describe", "--always", "--dirty") or commit
    write_text(dst / "NOTICE.txt", f"""SuperShuckie {version}
Copyright (C) Snowy (SnowyMouse) and contributors

This program is free software: you can redistribute it and/or modify it under the
terms of the GNU General Public License as published by the Free Software
Foundation, version 3 (see COPYING). It is distributed WITHOUT ANY WARRANTY.

Complete corresponding source code for this build:
    https://github.com/SnowyMouse/supershuckie   (upstream)
    https://github.com/Scotts-Thoughts/supershuckie   (this build)
    commit {commit} ({describe})

The source tree also contains the modifications applied to the emulator cores
(melonds-rs/patches/) and everything needed to relink the program against
modified versions of the LGPL libraries listed in ../qt and ../libraries.

The application icon is by Scott (Scott's Thoughts); see icon-credits.txt.
""")
    index.append(("SuperShuckie", version, "GPL-3.0-only", "supershuckie/"))


# --------------------------------------------------------------------------------------
# Emulator cores
# --------------------------------------------------------------------------------------

def section_emulator_cores(repo: Path, out: Path, index: list, sameboy_dir: Path):
    # melonDS (git submodule, GPL-3.0-or-later, with bundled third-party code)
    melonds = repo / "melonds-rs" / "melonDS"
    dst = out / "emulator-cores" / "melonDS"
    for rel, name in MELONDS_LICENSE_FILES:
        copy_required(melonds / rel, dst / name)
    copy_required(repo / "licenses" / "notices" / "melonDS-third-party.txt", dst / "THIRD-PARTY-NOTICES.txt")
    rev = git(melonds, "describe", "--tags", "--always") or "unknown"
    patches = sorted(p.name for p in (repo / "melonds-rs" / "patches").glob("*.patch"))
    write_text(dst / "NOTICE.txt", f"""melonDS  (https://melonds.kuribo64.net/, https://github.com/melonDS-emu/melonDS)
Copyright 2016-2025 melonDS team
GNU General Public License v3 or later (LICENSE)

Revision built into SuperShuckie: {rev}
SuperShuckie applies the following patches, which are part of its source tree
under melonds-rs/patches/:
{chr(10).join('    ' + p for p in patches) or '    (none)'}

Bundled third-party code: see THIRD-PARTY-NOTICES.txt.
""")
    index.append(("melonDS", rev, "GPL-3.0-or-later (+ bundled MIT / LGPL-2.1 / GPL-2.0 / FatFs / BSD-2 / public domain)", "emulator-cores/melonDS/"))

    # mGBA (git submodule, MPL-2.0)
    mgba = repo / "mgba-rs" / "mgba"
    dst = out / "emulator-cores" / "mGBA"
    for rel, name in MGBA_LICENSE_FILES:
        copy_required(mgba / rel, dst / name)
    copy_required(repo / "licenses" / "notices" / "mGBA-third-party.txt", dst / "THIRD-PARTY-NOTICES.txt")
    rev = git(mgba, "describe", "--tags", "--always") or "unknown"
    commit = git(mgba, "rev-parse", "HEAD") or "unknown"
    write_text(dst / "NOTICE.txt", f"""mGBA  (https://mgba.io/, https://github.com/mgba-emu/mgba)
Copyright (c) 2013-2025 Jeffrey Pfau
Mozilla Public License 2.0 (LICENSE)

Revision built into SuperShuckie: {rev} (commit {commit})

Per MPL 2.0 section 3.2, the source code of the mGBA files compiled into this
program is available, unmodified, at the URL above at that commit; SuperShuckie
does not modify mGBA's source. The glue code around it (mgba-rs/) is part of
SuperShuckie and is GPL-3.0.

Bundled third-party code: see THIRD-PARTY-NOTICES.txt.
""")
    index.append(("mGBA", rev, "MPL-2.0 (+ bundled BSD-3 inih)", "emulator-cores/mGBA/"))

    # SameBoy (bundled inside the sameboy-sys crate, MIT)
    dst = out / "emulator-cores" / "SameBoy"
    copy_required(sameboy_dir / "SameBoy" / "LICENSE", dst / "LICENSE")
    version_mk = sameboy_dir / "SameBoy" / "version.mk"
    sb_version = "unknown"
    if version_mk.is_file():
        m = re.search(r"VERSION\s*:?=\s*(\S+)", version_mk.read_text(encoding="utf-8"))
        if m:
            sb_version = m.group(1)
    write_text(dst / "NOTICE.txt", f"""SameBoy  (https://sameboy.github.io/, https://github.com/LIJI32/SameBoy)
Copyright (c) 2015-2023 Lior Halphon
MIT (Expat) License (LICENSE)

SameBoy {sb_version} is compiled into SuperShuckie through the sameboy-sys /
safeboy crates ({sameboy_dir.name}), which are GPL-3.0-only and written by
SuperShuckie's author.
""")
    index.append(("SameBoy", sb_version, "MIT", "emulator-cores/SameBoy/"))


# --------------------------------------------------------------------------------------
# Boot ROMs / BIOS images embedded in the executable
# --------------------------------------------------------------------------------------

def section_boot_roms(repo: Path, out: Path, index: list):
    dst = out / "boot-roms"
    copy_required(repo / "bootrom" / "cgb" / "cgb_boot" / "LICENSE", dst / "cgb-boot" / "LICENSE")
    copy_required(repo / "bootrom" / "dmg" / "hardware.inc" / "LICENSE", dst / "hardware.inc" / "LICENSE")
    copy_required(repo / "licenses" / "notices" / "gba-bios.txt", dst / "gba-bios" / "NOTICE.txt")
    copy_required(repo / "licenses" / "texts" / "GPL-2.0.txt", dst / "gba-bios" / "GPL-2.0.txt")
    write_text(dst / "README.txt", """Boot ROM and BIOS images embedded in the SuperShuckie executable
================================================================

None of these are Nintendo's; they are open-source replacements.

cgb-boot/     Game Boy Color boot ROM: SameBoy's cgb_boot_fast, Copyright (c)
              2015-2023 Lior Halphon, MIT License (cgb-boot/LICENSE). Built from
              bootrom/cgb/cgb_boot/ in the SuperShuckie source tree.

dmg           Game Boy boot ROM: a 256-byte hardware-initialisation stub written
              for SuperShuckie (bootrom/dmg/dmg.asm, GPL-3.0 like the rest of the
              program). It only sets up the stack, VRAM, audio registers and
              palette, then hands over to the cartridge; it does not contain the
              logo data or logo check of the original.

hardware.inc/ Register definitions used to assemble the boot ROMs. CC0 1.0
              (hardware.inc/LICENSE), https://github.com/gbdev/hardware.inc

gba-bios/     Game Boy Advance BIOS: the open-source VBA-M / Normmatt replacement
              BIOS, GPL-2.0-or-later. See gba-bios/NOTICE.txt.

Nintendo DS   No BIOS or firmware image is shipped; melonDS's built-in FreeBIOS
              (BSD-2-Clause) and generated firmware are used. See
              ../emulator-cores/melonDS/THIRD-PARTY-NOTICES.txt.
""")
    index.append(("SameBoy CGB boot ROM", "", "MIT", "boot-roms/cgb-boot/"))
    index.append(("hardware.inc", "", "CC0-1.0", "boot-roms/hardware.inc/"))
    index.append(("VBA-M / Normmatt GBA BIOS", "", "GPL-2.0-or-later", "boot-roms/gba-bios/"))


# --------------------------------------------------------------------------------------
# Rust crates
# --------------------------------------------------------------------------------------

def cargo_metadata(repo: Path, cargo: str, target: str):
    cmd = [cargo, "metadata", "--format-version", "1", "--manifest-path",
           str(repo / "supershuckie-frontend-c" / "Cargo.toml")]
    if target:
        cmd += ["--filter-platform", target]
    return json.loads(run(cmd, cwd=repo))


def linked_crates(meta):
    """Packages reachable from supershuckie-frontend-c through normal (non-build,
    non-dev) dependencies, excluding proc-macro crates: the set whose code can end
    up in the executable."""
    packages = {p["id"]: p for p in meta["packages"]}
    nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
    root = next(p["id"] for p in meta["packages"] if p["name"] == "supershuckie-frontend-c")

    def is_proc_macro(pkg):
        return any(t["kind"] == ["proc-macro"] for t in pkg["targets"])

    seen = set()
    stack = [root]
    while stack:
        pkg_id = stack.pop()
        if pkg_id in seen:
            continue
        seen.add(pkg_id)
        for dep in nodes[pkg_id]["deps"]:
            kinds = {k["kind"] for k in dep["dep_kinds"]}
            if not (None in kinds or "normal" in kinds):
                continue
            if is_proc_macro(packages[dep["pkg"]]):
                continue
            stack.append(dep["pkg"])
    return sorted((packages[i] for i in seen), key=lambda p: (p["name"], p["version"]))


def section_crates(repo: Path, out: Path, index: list, cargo: str, target: str):
    meta = cargo_metadata(repo, cargo, target)
    crates = linked_crates(meta)
    dst = out / "crates"
    overrides = repo / "licenses" / "crates"
    lines = []
    sameboy_dir = None
    problems = []

    for pkg in crates:
        name, version = pkg["name"], pkg["version"]
        manifest_dir = Path(pkg["manifest_path"]).parent
        if pkg["source"] is None:
            # Workspace member: part of SuperShuckie, covered by supershuckie/COPYING.
            continue
        if name == "sameboy-sys":
            sameboy_dir = manifest_dir

        crate_dst = dst / f"{name}-{version}"
        files = license_files_in(manifest_dir)
        override_dir = overrides / name
        if override_dir.is_dir():
            files += license_files_in(override_dir)
        license_expr = pkg.get("license") or "(no license field)"

        if files:
            for f in files:
                copy_required(f, crate_dst / f.name)
        elif license_expr in ("GPL-3.0-only", "GPL-3.0-or-later"):
            # Same license and author as SuperShuckie; the GPL text is shipped once.
            write_text(crate_dst / "LICENSE-NOTE.txt",
                       f"{name} {version} is licensed under {license_expr}.\n"
                       f"The license text is in ../../supershuckie/COPYING.\n")
        else:
            problems.append(f"{name} {version} ({license_expr}) ships no license file and "
                            f"licenses/crates/{name}/ has no override")
            continue

        repo_url = pkg.get("repository") or ""
        lines.append(f"{name} {version}\n    license: {license_expr}\n"
                     + (f"    {repo_url}\n" if repo_url else "")
                     + f"    files: {', '.join(p.name for p in sorted(crate_dst.iterdir()))}\n")
        index.append((name, version, license_expr, f"crates/{name}-{version}/"))

    if problems:
        raise AttributionError("crates without license texts:\n  " + "\n  ".join(problems))
    if sameboy_dir is None:
        raise AttributionError("sameboy-sys is not in the dependency graph")

    write_text(dst / "README.txt",
               "Rust crates compiled into SuperShuckie\n"
               "======================================\n\n"
               "Resolved from Cargo.lock for the supershuckie-frontend-c crate"
               + (f" (target {target})" if target else "") + ".\n"
               "Crates only used at build time (build scripts, procedural macros) are not\n"
               "listed because none of their code is in the executable. Crates that are part\n"
               "of SuperShuckie itself are covered by ../supershuckie/COPYING.\n"
               "Each crate's license text(s) are in the directory named after it.\n\n"
               + "\n".join(lines))
    return sameboy_dir


# --------------------------------------------------------------------------------------
# Rust standard library
# --------------------------------------------------------------------------------------

def section_rust_std(out: Path, index: list, cargo: str):
    rustc = Path(cargo).with_name("rustc" + Path(cargo).suffix) if os.path.isabs(cargo) else "rustc"
    version = run([str(rustc), "--version"]).strip()
    short_version = (re.match(r"rustc (\S+)", version) or [None, version])[1]
    sysroot = Path(run([str(rustc), "--print", "sysroot"]).strip())
    dst = out / "rust-std"
    msys_doc = sysroot / "share" / "doc" / "rustc"
    rustup_doc = sysroot / "share" / "doc" / "rust"
    if (msys_doc / "COPYRIGHT-library.html").is_file():
        copy_required(msys_doc / "COPYRIGHT-library.html", dst / "COPYRIGHT-library.html")
        copy_tree_required(msys_doc / "licenses", dst / "licenses")
    elif (rustup_doc / "COPYRIGHT").is_file():
        for name in ("COPYRIGHT", "LICENSE-APACHE", "LICENSE-MIT"):
            copy_required(rustup_doc / name, dst / name)
    else:
        raise AttributionError(f"cannot find the Rust standard library license files under {sysroot}")
    write_text(dst / "NOTICE.txt", f"""Rust standard library
=====================

The executable contains the Rust standard library of:
    {version}

The Rust standard library is dual-licensed under the Apache License 2.0 and
the MIT license, and bundles third-party code under other permissive licenses.
The full copyright and license information as shipped with the toolchain is
in this directory (COPYRIGHT-library.html / COPYRIGHT and the license texts).
""")
    index.append(("Rust standard library", short_version, "MIT OR Apache-2.0 (+ bundled)", "rust-std/"))


# --------------------------------------------------------------------------------------
# MSYS2: Qt, SDL3, Qt's external libraries, MinGW runtime
# --------------------------------------------------------------------------------------

def msys2_package_name(prefix: Path, short_name: str) -> str:
    env = prefix.name  # e.g. ucrt64
    arch_prefix = {"ucrt64": "mingw-w64-ucrt-x86_64-", "mingw64": "mingw-w64-x86_64-",
                   "clang64": "mingw-w64-clang-x86_64-", "clangarm64": "mingw-w64-clang-aarch64-"}.get(env, "")
    return arch_prefix + short_name


def msys2_package_version(prefix: Path, short_name: str) -> str:
    """Read the installed version from pacman's local database without running pacman."""
    local_db = prefix.parent / "var" / "lib" / "pacman" / "local"
    full = msys2_package_name(prefix, short_name)
    for entry in local_db.glob(full + "-*"):
        rest = entry.name[len(full) + 1:]
        # A package name never contains "-<digit>" before the version, so the first
        # "-<digit>" starts the version; reject entries that are longer package names.
        if rest[:1].isdigit():
            return rest
    return "unknown"


def section_msys2(repo: Path, out: Path, index: list, prefix: Path):
    if not prefix.is_dir():
        raise AttributionError(f"MSYS2 prefix does not exist: {prefix}")
    share_licenses = prefix / "share" / "licenses"

    # Qt: the static Qt package keeps its license texts under qt6-static/share/licenses/qt6.
    qt_static = prefix / "qt6-static" / "share" / "licenses" / "qt6"
    qt_dynamic = share_licenses / "qt6-base"
    qt_src = qt_static if qt_static.is_dir() else qt_dynamic
    qt_pkg = "qt6-static" if qt_static.is_dir() else "qt6-base"
    dst = out / "qt"
    copy_tree_required(qt_src, dst)
    qt_version = msys2_package_version(prefix, qt_pkg)
    copy_required(repo / "licenses" / "qt" / "THIRD-PARTY-NOTICES.txt", dst / "THIRD-PARTY-NOTICES.txt")
    copy_tree_required(repo / "licenses" / "qt" / "third-party", dst / "third-party")
    write_text(dst / "NOTICE.txt", f"""Qt 6 (Qt Core, Qt Gui, Qt Widgets, Qt Svg, Qt OpenGL, the Windows platform
plugin and the image-format / style plugins)
MSYS2 package {msys2_package_name(prefix, qt_pkg)} {qt_version}
Copyright (C) The Qt Company Ltd. and other contributors
https://www.qt.io/

Qt is used by SuperShuckie under the GNU Lesser General Public License
version 3 (LGPL-3.0-only.txt). The Qt libraries are statically linked into
the executable. As required by LGPL v3 section 4, the complete source code
of SuperShuckie is available under the GPL v3 (see ../supershuckie/NOTICE.txt),
which allows the program to be recompiled and relinked against a modified
version of Qt. Qt's own source is available from https://download.qt.io/ and
the MSYS2 build recipe from https://github.com/msys2/MINGW-packages.

The other files in this directory are the license texts Qt is offered under
(LGPL-3.0-only.txt, GPL-2.0-only.txt, GPL-3.0-only.txt, the Qt GPL exception,
and BSD-3-Clause.txt for Qt's examples and build files).

Third-party code that is part of the Qt source tree and therefore compiled
into these libraries is listed in THIRD-PARTY-NOTICES.txt, with license texts
in third-party/.
""")
    index.append(("Qt 6", qt_version, "LGPL-3.0-only (+ bundled third-party)", "qt/"))

    # Everything else from pacman.
    lines = []
    for short, subdir, license_id, description in MSYS2_PACKAGES:
        src = share_licenses / subdir
        pkg_dst = out / "libraries" / subdir
        copy_tree_required(src, pkg_dst)
        version = msys2_package_version(prefix, short)
        lines.append(f"{subdir}/   {description}\n    license: {license_id}\n"
                     f"    MSYS2 package mingw-w64-{prefix.name}-{short} {version}\n")
        index.append((subdir, version, license_id, f"libraries/{subdir}/"))

    write_text(out / "libraries" / "README.txt", f"""C libraries statically linked into the SuperShuckie executable
=============================================================

These come from MSYS2 ({prefix}) and are the libraries the static Qt build
depends on, plus SDL3 and the MinGW-w64 / GCC runtime. Each directory holds
the license files exactly as installed by the MSYS2 package named below;
versions are the installed package versions at the time this build was made.

Notes required by some of these licenses:
  * Portions of this software are copyright (c) The FreeType Project
    (www.freetype.org). All rights reserved. (freetype/, FreeType License)
  * This software is based in part on the work of the Independent JPEG Group.
    (libjpeg-turbo/, IJG license)
  * libgcc and libstdc++ are GPL-3.0 with the GCC Runtime Library Exception
    (gcc-libs/COPYING.RUNTIME). GLib, libintl, libiconv, graphite2 are LGPL;
    as with Qt, relinking is possible because SuperShuckie's full source is
    available (see ../supershuckie/NOTICE.txt).

{chr(10).join(lines)}""")


# --------------------------------------------------------------------------------------
# Top-level index
# --------------------------------------------------------------------------------------

def write_index(out: Path, index: list, version: str, msys_used: bool):
    width = max(len(f"{n} {v}".strip()) for n, v, _, _ in index) + 2
    rows = "\n".join(f"{(n + ' ' + v).strip():{width}} {lic:60} {where}" for n, v, lic, where in index)
    scope = ("Windows standalone build: every library below is statically linked into\n"
             "supershuckie.exe, which has no non-system DLL dependencies."
             if msys_used else
             "The Qt / SDL / C-runtime libraries are supplied by the system on this platform\n"
             "and are not listed here.")
    write_text(out / "README.txt", f"""SuperShuckie {version} - libraries and attributions
=================================================

SuperShuckie is free software under the GNU General Public License v3
(supershuckie/COPYING). This folder contains the license texts and copyright
notices of SuperShuckie and of every third-party component built into the
executable, as those licenses require for binary distributions. Keep this
folder together with the executable when redistributing it.

{scope}

Layout
------
supershuckie/     SuperShuckie's own license, source-code offer and icon credit
emulator-cores/   melonDS (GPL-3), mGBA (MPL-2.0), SameBoy (MIT) and the third-party
                  code bundled inside them
boot-roms/        the open-source boot ROM / BIOS images embedded in the executable
crates/           every Rust crate compiled into the executable (one directory each)
rust-std/         the Rust standard library
qt/               Qt 6 (LGPL-3) and the third-party code bundled inside Qt
libraries/        SDL3, the C libraries Qt links, and the MinGW-w64 / GCC runtime

Components
----------
{rows}
""")


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--out", required=True, type=Path, help="output directory (recreated)")
    parser.add_argument("--repo-root", type=Path, default=Path(__file__).resolve().parent.parent)
    parser.add_argument("--cargo", default="cargo", help="cargo executable used for the build")
    parser.add_argument("--target", default="x86_64-pc-windows-gnu" if os.name == "nt" else "",
                        help="cargo target triple to resolve the crate graph for ('' = host)")
    parser.add_argument("--msys-prefix", type=Path, default=None,
                        help="MSYS2 environment prefix (e.g. C:/msys64/ucrt64) whose Qt/SDL/runtime "
                             "are linked in; defaults to the prefix containing --cargo on Windows. "
                             "Pass an empty string to skip the MSYS2 section.")
    args = parser.parse_args()

    repo = args.repo_root.resolve()
    out = args.out.resolve()
    msys_prefix = args.msys_prefix
    if msys_prefix is None and os.name == "nt":
        cargo_path = shutil.which(args.cargo)
        if cargo_path and Path(cargo_path).parent.name == "bin":
            candidate = Path(cargo_path).parent.parent
            if (candidate / "share" / "licenses").is_dir():
                msys_prefix = candidate
    if msys_prefix is not None and str(msys_prefix) in ("", "."):
        msys_prefix = None

    try:
        if out.exists():
            shutil.rmtree(out)
        out.mkdir(parents=True)
        index, crate_index = [], []
        section_supershuckie(repo, out, index)
        sameboy_dir = section_crates(repo, out, crate_index, args.cargo, args.target)
        section_emulator_cores(repo, out, index, sameboy_dir)
        section_boot_roms(repo, out, index)
        section_rust_std(out, index, args.cargo)
        if msys_prefix is not None:
            section_msys2(repo, out, index, msys_prefix)
        index += crate_index
        write_index(out, index, workspace_version(repo), msys_prefix is not None)
    except AttributionError as e:
        print(f"make-attributions: error: {e}", file=sys.stderr)
        return 1
    print(f"make-attributions: wrote {len(index)} components to {out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
