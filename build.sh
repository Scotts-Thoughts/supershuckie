#!/bin/sh

git pull --recurse-submodules=yes

BUILD_MODE=Release

cmake ./mgba-rs/mgba -B build/mgba \
	-DLIBMGBA_ONLY=ON \
	-DDISABLE_FRONTENDS=ON \
	-DCMAKE_BUILD_TYPE=$BUILD_MODE
make -C build/mgba -j$(nproc)

# melonDS is built with LTO, and with profile-guided optimisation when PGO_ROM/PGO_REPLAY are
# set (see scripts/build-melonds.sh): the DS interpreter is 12-18% faster for it. It comes after
# mGBA because the PGO training binary links both libraries.
scripts/build-melonds.sh build/melonDS

cmake ./supershuckie-qt  -B build -DCMAKE_BUILD_TYPE=$BUILD_MODE -DSCRIPT_BUILD=ON
make -C build -j$(nproc)
