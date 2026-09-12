cmake_minimum_required(VERSION 4.0)

project("SuperShuckie"
    VERSION 1.0
    LANGUAGES CXX
)

include(corrosion.cmake)

corrosion_import_crate(MANIFEST_PATH "../supershuckie-frontend-c/Cargo.toml")

set(CMAKE_AUTOMOC ON)

if(${WIN32})
    set(EXE_TYPE "WIN32")
endif()

add_library(supershuckie-cpp-static STATIC
    src/main.cpp
    src/main_window.cpp
    src/render_widget.cpp
    src/file_rw.cpp
    src/game_speed_dialog.cpp
    src/ask_for_text_dialog.cpp
    src/select_item_dialog.cpp
    src/controller_settings_window.cpp
    src/sdl_event_wrapper.cpp
    src/theme.cpp
    src/replay_playback_controls.cpp
    src/nds_date_dialog.cpp
    src/video_export_dialog.cpp
    src/audio_output.cpp
)

set_property(TARGET supershuckie-cpp-static PROPERTY CXX_STANDARD 20)

target_include_directories(supershuckie-cpp-static
    PRIVATE "${CMAKE_SOURCE_DIR}/../supershuckie-frontend-c/include"
)

# Fold Qt, SDL, and the C/C++ runtime into a single standalone executable.
# Requires a static Qt6 build (e.g. mingw-w64-ucrt-x86_64-qt6-static) and a static SDL3;
# point CMAKE_PREFIX_PATH/Qt6_DIR at the static Qt when enabling this.
option(SUPERSHUCKIE_STATIC "Statically link Qt, SDL, and the runtime into a single exe" OFF)

find_package(Qt6 REQUIRED COMPONENTS Core Gui Widgets)
if(SUPERSHUCKIE_STATIC)
    find_package(SDL3 REQUIRED CONFIG COMPONENTS SDL3-static)
    set(SUPERSHUCKIE_SDL_TARGET SDL3::SDL3-static)
else()
    find_package(SDL3 REQUIRED CONFIG COMPONENTS SDL3)
    set(SUPERSHUCKIE_SDL_TARGET SDL3::SDL3)
endif()

if(SUPERSHUCKIE_STATIC)
    # MSYS2's static Qt still references its third-party deps via full-path .dll.a import
    # libraries, which -static cannot override. Repoint each imported target at the matching
    # static archive so nothing is pulled from a DLL. Their own transitive deps are supplied
    # by the --start-group block on the final executable below.
    set(_SS_LIB "C:/msys64/ucrt64/lib")
    macro(_ss_repoint tgt archive)
        if(TARGET ${tgt})
            set_target_properties(${tgt} PROPERTIES
                IMPORTED_LOCATION "${_SS_LIB}/${archive}"
                IMPORTED_LOCATION_RELEASE "${_SS_LIB}/${archive}")
        endif()
    endmacro()
    _ss_repoint(ZLIB::ZLIB libz.a)
    _ss_repoint(PNG::PNG libpng16.a)
    _ss_repoint(Freetype::Freetype libfreetype.a)
    _ss_repoint(harfbuzz::harfbuzz libharfbuzz.a)
    _ss_repoint(JPEG::JPEG libjpeg.a)
    _ss_repoint(TIFF::TIFF libtiff.a)
    _ss_repoint(Libb2::Libb2 libb2.a)
    _ss_repoint(PCRE2::16BIT libpcre2-16.a)
    if(TARGET WrapWebP::WrapWebP)
        set_target_properties(WrapWebP::WrapWebP PROPERTIES INTERFACE_LINK_LIBRARIES
            "${_SS_LIB}/libwebp.a;${_SS_LIB}/libwebpdemux.a;${_SS_LIB}/libwebpmux.a;${_SS_LIB}/libsharpyuv.a")
    endif()
    # libb2 (BLAKE2) and PCRE2 reach Qt6::Core through the Libb2::Libb2 / WrapPCRE2::WrapPCRE2
    # imported targets, which resolve (via pkg-config) to .dll.a import libraries. Point their
    # link interfaces straight at the static archives.
    if(TARGET Libb2::Libb2)
        set_target_properties(Libb2::Libb2 PROPERTIES INTERFACE_LINK_LIBRARIES "${_SS_LIB}/libb2.a")
    endif()
    if(TARGET WrapPCRE2::WrapPCRE2)
        set_target_properties(WrapPCRE2::WrapPCRE2 PROPERTIES INTERFACE_LINK_LIBRARIES "${_SS_LIB}/libpcre2-16.a")
    endif()
endif()

target_link_libraries(supershuckie-cpp-static
    PUBLIC Qt6::Widgets
    PUBLIC Qt6::Core
    PUBLIC ${SUPERSHUCKIE_SDL_TARGET}
    PUBLIC supershuckie_frontend_c
)

if(SCRIPT_BUILD)
    find_library(MGBA_CORE NAMES mgba PATHS ${CMAKE_BINARY_DIR}/mgba)
    find_library(MELONDS_CORE NAMES core PATHS ${CMAKE_BINARY_DIR}/melonDS/src/)
    find_library(MELONDS_TEAKRA NAMES teakra PATHS ${CMAKE_BINARY_DIR}/melonDS/src/teakra/src)
    target_link_libraries(supershuckie-cpp-static
        PUBLIC ${MELONDS_CORE}
        PUBLIC ${MELONDS_TEAKRA}
        PUBLIC ${MGBA_CORE}
    )
    # scripts/build-melonds.* produce libcore.a with LTO (and PGO) objects; make sure the final
    # link actually runs the link-time optimiser over them rather than taking the fallback code.
    if(CMAKE_CXX_COMPILER_ID STREQUAL "GNU")
        target_link_options(supershuckie-cpp-static PUBLIC -flto=auto)
    endif()
endif()


if(${WIN32})
    # dwmapi: window frame theming. shlwapi: mGBA's vfs/config use PathIsRelativeW/PathRemoveFileSpecW.
    target_link_libraries(supershuckie-cpp-static PUBLIC dwmapi shlwapi)
endif()

if(SUPERSHUCKIE_STATIC)
    # The repointed static archives (freetype, harfbuzz, glib, tiff, …) carry no link metadata,
    # so their inter-dependencies (tiff->lzma/jbig/deflate/Lerc/zstd, harfbuzz<->freetype,
    # harfbuzz->glib/graphite2, freetype->bz2/brotli, glib->pcre2-8/intl/iconv) must be supplied
    # explicitly. Attaching this as the LAST PUBLIC entry of supershuckie-cpp-static places it
    # after Qt's static archives in the final link line, so the group resolves what Qt pulls in.
    # LINK_GROUP/RESCAN emits a --start-group/--end-group (surviving CMake de-duplication) so the
    # linker resolves the libs in any order. With -static, each plain name resolves to libNAME.a.
    target_link_libraries(supershuckie-cpp-static PUBLIC
        "$<LINK_GROUP:RESCAN,freetype,harfbuzz,png16,z,jpeg,tiff,webp,webpdemux,webpmux,sharpyuv,b2,pcre2-16,pcre2-8,glib-2.0,graphite2,brotlidec,brotlicommon,bz2,intl,iconv,lzma,deflate,jbig,Lerc,zstd,rpcrt4,usp10>"
    )
endif()

add_executable(supershuckie-debug)
target_link_libraries(supershuckie-debug supershuckie-cpp-static)

# Ship the license texts and copyright notices of everything linked into the executable
# (the emulator cores, every Rust crate, Qt and its statically linked dependencies, the
# MinGW runtime, the embedded boot ROMs) in a libraries-and-attributions/ folder next to it.
# scripts/make-attributions.py gathers them from the repo, the cargo registry and the MSYS2
# prefix, and fails the build if a dependency has no license text to ship.
# Default on for the Windows standalone build, where every library is redistributed inside
# the exe; on other platforms Qt/SDL/the runtime are the system's and the toolchain layout
# the script expects (an MSYS2 or rustup sysroot) may not exist.
if(${WIN32})
    set(_ss_attr_default ON)
else()
    set(_ss_attr_default OFF)
endif()
option(SUPERSHUCKIE_ATTRIBUTIONS "Generate libraries-and-attributions/ next to the executable" ${_ss_attr_default})
if(SUPERSHUCKIE_ATTRIBUTIONS)
    find_package(Python3 COMPONENTS Interpreter REQUIRED)
    set(SUPERSHUCKIE_ATTRIBUTIONS_DIR "${CMAKE_BINARY_DIR}/libraries-and-attributions")
    set(_ss_attr_script "${CMAKE_SOURCE_DIR}/../scripts/make-attributions.py")
    set(_ss_attr_args --out "${SUPERSHUCKIE_ATTRIBUTIONS_DIR}"
        --cargo "${Rust_CARGO_CACHED}" --target "${Rust_CARGO_TARGET_CACHED}")
    if(${WIN32})
        # The Qt/SDL/runtime libraries come from the MSYS2 environment whose cargo builds the
        # crates (C:/msys64/ucrt64/bin/cargo.exe -> C:/msys64/ucrt64).
        get_filename_component(_ss_msys_prefix "${Rust_CARGO_CACHED}" DIRECTORY)
        get_filename_component(_ss_msys_prefix "${_ss_msys_prefix}" DIRECTORY)
        list(APPEND _ss_attr_args --msys-prefix "${_ss_msys_prefix}")
    else()
        list(APPEND _ss_attr_args --msys-prefix "")
    endif()
    # Re-run when the inputs it reads from the repo change: the script, the lock file (crate
    # set), the vendored notices, and the license files it copies verbatim.
    file(GLOB_RECURSE _ss_attr_inputs CONFIGURE_DEPENDS
        "${CMAKE_SOURCE_DIR}/../licenses/*"
        "${CMAKE_SOURCE_DIR}/../bootrom/*/LICENSE"
        "${CMAKE_SOURCE_DIR}/../bootrom/*/*/LICENSE")
    add_custom_command(
        OUTPUT "${SUPERSHUCKIE_ATTRIBUTIONS_DIR}/README.txt"
        COMMAND "${Python3_EXECUTABLE}" "${_ss_attr_script}" ${_ss_attr_args}
        DEPENDS "${_ss_attr_script}" "${CMAKE_SOURCE_DIR}/../Cargo.lock"
                "${CMAKE_SOURCE_DIR}/../COPYING" "${CMAKE_SOURCE_DIR}/icon-credits.txt"
                ${_ss_attr_inputs}
        WORKING_DIRECTORY "${CMAKE_SOURCE_DIR}/.."
        COMMENT "Collecting third-party licenses into libraries-and-attributions/"
        VERBATIM)
    add_custom_target(supershuckie-attributions DEPENDS "${SUPERSHUCKIE_ATTRIBUTIONS_DIR}/README.txt")
endif()

if(${APPLE})
    set(supershuckie_ICON "${CMAKE_SOURCE_DIR}/supershuckie.icns")
    set_source_files_properties(${supershuckie_ICON} PROPERTIES MACOSX_PACKAGE_LOCATION "Resources")
    add_executable(supershuckie MACOSX_BUNDLE ${supershuckie_ICON})
    set_target_properties(supershuckie PROPERTIES
        MACOSX_BUNDLE_BUNDLE_NAME "SuperShuckie"
        MACOSX_BUNDLE_INFO_PLIST "${CMAKE_SOURCE_DIR}/Info.plist.in"
        MACOSX_BUNDLE_ICON_FILE "supershuckie.icns"
        MACOSX_BUNDLE_COPYRIGHT "Snowy"
        MACOSX_BUNDLE_EXECUTABLE_NAME "supershuckie"
        MACOSX_BUNDLE_GUI_IDENTIFIER "com.snowymouse.supershuckie"
        MACOSX_BUNDLE_LONG_VERSION_STRING "2.0"
        MACOSX_BUNDLE_BUNDLE_VERSION "2.0"
        MACOSX_BUNDLE_SHORT_VERSION_STRING "2"
        OUTPUT_NAME "SuperShuckie"
    )
elseif(${WIN32})
    add_executable(supershuckie WIN32
        src/windows.rc
    )
else()
    add_executable(supershuckie)
    set_target_properties(supershuckie-debug PROPERTIES
        OUTPUT_NAME "supershuckie"
    )
endif()

target_link_libraries(supershuckie supershuckie-cpp-static)
if(SUPERSHUCKIE_ATTRIBUTIONS)
    add_dependencies(supershuckie supershuckie-attributions)
endif()

if(SUPERSHUCKIE_STATIC)
    # A static Qt has no plugin DLLs to load at runtime, so the platform integration
    # plugin must be linked in and registered via Q_IMPORT_PLUGIN (qt6_import_plugins).
    if(${WIN32})
        qt6_import_plugins(supershuckie INCLUDE Qt6::QWindowsIntegrationPlugin)
    endif()
    # Statically link libgcc/libstdc++/winpthread so no MinGW runtime DLLs are needed.
    # --allow-multiple-definition: mgba bundles its own crc32, which also lives in zlib
    # (pulled in statically via Qt's PNG support); both are the standard CRC-32, so taking
    # the first definition is fine.
    target_link_options(supershuckie PRIVATE -static -static-libgcc -static-libstdc++ -Wl,--allow-multiple-definition)
endif()
