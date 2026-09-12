#!/bin/sh
# Build the melonDS static library for Super Shuckie with LTO and, optionally, profile-guided
# optimisation. See scripts/build-melonds.ps1 for the Windows version and the full rationale.
#
#   scripts/build-melonds.sh [build-dir]                       LTO build
#   PGO_ROM=game.nds PGO_REPLAY="a.replay b.replay" \
#   scripts/build-melonds.sh [build-dir]                       PGO + LTO build (trains on the replays)
#
# Optional: PGO_FRAMES (frames per replay, default 18000), CMAKE_GENERATOR.
set -e

BUILD_DIR="${1:-build/melonDS}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

# Super Shuckie's local changes to melonDS (see melonds-rs/patches/README.md) are kept as patch
# files because melonDS is a submodule; apply whichever are not applied yet.
for patch in melonds-rs/patches/*.patch; do
    if git -C melonds-rs/melonDS apply --check --reverse --ignore-whitespace "../patches/$(basename "$patch")" 2>/dev/null; then
        continue
    fi
    git -C melonds-rs/melonDS apply --ignore-whitespace "../patches/$(basename "$patch")"
    echo "applied $(basename "$patch")"
done

LTO="-flto=auto -ffat-lto-objects"

configure() {
    cmake ./melonds-rs/melonDS -B "$BUILD_DIR" \
        -DENABLE_JIT=ON -DENABLE_OGLRENDERER=OFF -DENABLE_GDBSTUB=OFF -DBUILD_QT_SDL=OFF \
        -DCMAKE_BUILD_TYPE=Release \
        -DCMAKE_CXX_FLAGS="$1" -DCMAKE_C_FLAGS="$1"
    cmake --build "$BUILD_DIR" -j"$(nproc 2>/dev/null || sysctl -n hw.ncpu)"
}

if [ -z "$PGO_ROM" ]; then
    echo "== melonDS: LTO build (set PGO_ROM and PGO_REPLAY for PGO) =="
    configure "$LTO"
    exit 0
fi

echo "== 1/3 melonDS: instrumented build =="
find "$BUILD_DIR" -name '*.gcda' -delete 2>/dev/null || true
configure "-fprofile-generate -fprofile-update=atomic"

echo "== 2/3 training =="
LINK=""
for a in -Wl,--start-group "$BUILD_DIR/src/libcore.a" "$BUILD_DIR/src/teakra/src/libteakra.a" build/mgba/libmgba.a \
         -lstdc++ -lm -lpthread -Wl,--end-group -fprofile-generate -lgcov; do
    LINK="$LINK -C link-arg=$a"
done
# shellcheck disable=SC2086
cargo rustc --release -p supershuckie-core --example nds_bench -- $LINK
BENCH=target/release/examples/nds_bench

for r in $PGO_REPLAY; do
    echo "== training on $r =="
    "$BENCH" "$PGO_ROM" --replay "$r" --frames "${PGO_FRAMES:-18000}" --warmup 0 --keyframes 120 --present-every 4
done
"$BENCH" "$PGO_ROM" --frames 2000 --warmup 0

echo "== 3/3 melonDS: optimised rebuild with the profile + LTO =="
configure "-fprofile-use -fprofile-correction -fprofile-partial-training -Wno-missing-profile $LTO"
echo "done: $BUILD_DIR/src/libcore.a"
