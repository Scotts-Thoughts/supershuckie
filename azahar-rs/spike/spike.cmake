# Headless Azahar (3DS) feasibility spike for Super Shuckie. See spike.cpp.
# Included into Azahar's top-level directory scope by hook.cmake, so the directory-scoped
# include path and definitions that src/CMakeLists.txt gives Azahar's own targets are repeated
# here for this one.
add_executable(azahar_spike "${CMAKE_CURRENT_LIST_DIR}/spike.cpp")
target_include_directories(azahar_spike PRIVATE "${CMAKE_SOURCE_DIR}/src")
target_compile_definitions(azahar_spike PRIVATE ENABLE_SOFTWARE_RENDERER)
if(WIN32)
    target_compile_definitions(azahar_spike PRIVATE NOMINMAX WIN32_LEAN_AND_MEAN UNICODE _UNICODE)
endif()
if(MINGW)
    target_compile_definitions(azahar_spike PRIVATE MINGW_HAS_SECURE_API)
    target_compile_options(azahar_spike PRIVATE -Wno-attributes -Wno-interference-size -Wno-psabi)
endif()
target_link_libraries(azahar_spike PRIVATE citra_core video_core audio_core citra_common xxHash::xxhash libzstd_static)
if(ENABLE_OPENGL)
    target_link_libraries(azahar_spike PRIVATE glad)
    target_compile_definitions(azahar_spike PRIVATE SPIKE_OPENGL ENABLE_OPENGL)
endif()
if(MINGW)
    target_link_libraries(azahar_spike PRIVATE crypt32 ws2_32 iphlpapi gdi32 opengl32)
    # A self-contained exe: no libstdc++/libwinpthread DLL search when run from anywhere.
    target_link_options(azahar_spike PRIVATE -static)
endif()
target_link_libraries(azahar_spike PRIVATE ${PLATFORM_LIBRARIES} Threads::Threads)

# One archive with everything the core needs, for the app's CMake and azahar-rs's `link-cores`
# feature: a single archive lets ld resolve the circular references between Azahar's libraries
# without link groups. The list is the spike's own link line.
set(AZAHAR_BUNDLE_INPUTS
    src/core/libcitra_core.a src/video_core/libvideo_core.a src/audio_core/libaudio_core.a
    src/common/libcitra_common.a src/network/libnetwork.a
    externals/xxHash/cmake_unofficial/libxxhash.a externals/zstd/build/cmake/lib/libzstd.a
    externals/libzstd_seekable.a externals/glad/libglad.a externals/enet/libenet.a
    externals/libressl/ssl/libssl-54.a externals/libressl/crypto/libcrypto-51.a
    externals/lodepng/liblodepng.a externals/dynarmic/src/dynarmic/libdynarmic.a
    externals/dynarmic/externals/zydis/libZydis.a externals/dynarmic/externals/zydis/zycore/libZycore.a
    externals/dynarmic/externals/mcl/src/libmcl.a externals/libboost_serialization.a
    externals/libboost_iostreams.a externals/cryptopp/libcryptopp.a externals/fmt/libfmt.a
    externals/faad2/libfaad2.a externals/soundtouch/libSoundTouch.a externals/teakra/src/libteakra.a)
set(_mri "create libazahar.a\n")
foreach(lib IN LISTS AZAHAR_BUNDLE_INPUTS)
    string(APPEND _mri "addlib ${lib}\n")
endforeach()
string(APPEND _mri "save\nend\n")
file(WRITE "${CMAKE_BINARY_DIR}/azahar-bundle.mri" "${_mri}")
add_custom_command(OUTPUT "${CMAKE_BINARY_DIR}/libazahar.a"
    COMMAND ${CMAKE_COMMAND} -E remove -f libazahar.a
    COMMAND ${CMAKE_AR} -M < azahar-bundle.mri
    # Both cores ship Teakra; keep Azahar's copy apart (see rename-teakra.cmake).
    COMMAND ${CMAKE_COMMAND} -DBUNDLE=${CMAKE_BINARY_DIR}/libazahar.a -DTEAKRA=${CMAKE_BINARY_DIR}/externals/teakra/src/libteakra.a
        -DNM=${CMAKE_NM} -DOBJCOPY=${CMAKE_OBJCOPY} -P "${CMAKE_CURRENT_LIST_DIR}/rename-teakra.cmake"
    DEPENDS azahar_spike ${AZAHAR_BUNDLE_INPUTS} "${CMAKE_BINARY_DIR}/azahar-bundle.mri" "${CMAKE_CURRENT_LIST_DIR}/rename-teakra.cmake"
    WORKING_DIRECTORY "${CMAKE_BINARY_DIR}"
    COMMENT "Bundling Azahar's archives into libazahar.a")
add_custom_target(azahar_bundle DEPENDS "${CMAKE_BINARY_DIR}/libazahar.a")
