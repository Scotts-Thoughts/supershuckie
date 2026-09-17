#include <cstdint>
#include <cstddef>
#include <cstdlib>
#include <cstdio>
#include <exception>
#include <vector>

#define ENABLE_VFS
#define ENABLE_DIRECTORIES

#include <mgba/core/core.h>
#include <mgba/core/log.h>
#include <mgba/core/serialize.h>
#include <mgba-util/vfs.h>
#include <mgba-util/audio-buffer.h>
#include <mgba-util/audio-resampler.h>
#include <mgba/gba/core.h>

static void nope_log(struct mLogger*, int category, enum mLogLevel level, const char* format, va_list args) {
}

static mLogger logger = {
    .log = nope_log
};


struct MGBACoreRaw {
    mCore *core = nullptr;

    // The save VFile is not kept here: once handed to loadSave() it is owned by mGBA (as
    // savedata.realVf) and closed by GBAUnloadROM when the core is unloaded/deinited. Its backing
    // memory is a private copy VFileMemChunk makes internally, so we don't need to keep a vector
    // of our own alive for it either.
    VFile *rom_vf = nullptr;
    std::vector<std::byte> rom;

    VFile *bios_vf = nullptr;
    std::vector<std::byte> bios;

    std::vector<std::uint32_t> pixels;

    std::size_t iwram = ~0;
    std::size_t ewram = ~0;

    // The core mixes at 32.768 kHz (or more, depending on SOUNDBIAS) into its own buffer; this
    // resamples that to the 48 kHz every core hands the frontend. Only run while enabled.
    bool audio_enabled = false;
    mAudioResampler resampler;
    mAudioBuffer resampled;
};

// ~170 ms at 48 kHz: plenty for the frames run between two drains.
static constexpr std::size_t RESAMPLED_CAPACITY_FRAMES = 8192;
static constexpr double OUTPUT_SAMPLE_RATE = 48000.0;

// Error codes handed back through `error_out` by mgba_rs_core_new on failure (see below).
enum MGBACoreNewError : std::uint32_t {
    MGBA_CORE_NEW_ERROR_CREATE_FAILED = 1,
    MGBA_CORE_NEW_ERROR_INIT_FAILED = 2,
    MGBA_CORE_NEW_ERROR_BAD_ROM = 3,
    MGBA_CORE_NEW_ERROR_MISSING_MEMORY_BLOCKS = 4,
    MGBA_CORE_NEW_ERROR_BAD_BIOS = 5,
};

// Tear down whatever mgba_rs_core_new got through before failing and report `code` through
// `error_out`. `core->core->init` is only ever called once, right after creation and before
// anything else touches the core, so by the time this can be reached with core->core non-null,
// either init succeeded (deinit is safe) or the caller already reset core->core to nullptr after
// freeing a half-initialised one by hand (see the init failure path below, which must NOT call
// deinit(): a failed init() never populates core->cpu/board, and mCore's own deinit
// unconditionally dereferences them).
static MGBACoreRaw *fail(MGBACoreRaw *core, std::uint32_t *error_out, std::uint32_t code) {
    if(error_out != nullptr) {
        *error_out = code;
    }
    if(core != nullptr) {
        if(core->core != nullptr) {
            core->core->deinit(core->core);
        }
        delete core;
    }
    return nullptr;
}

extern "C" MGBACoreRaw *mgba_rs_core_new(
    const std::byte *rom,
    std::size_t rom_size,
    const std::byte *sram,
    std::size_t sram_size,
    const std::byte *bios,
    std::size_t bios_size,
    std::uint32_t *error_out
) {
    mLogSetDefaultLogger(&logger);

    auto *core = new MGBACoreRaw();
    core->core = mCoreCreate(mPLATFORM_GBA);

    if(core->core == nullptr) {
        return fail(core, error_out, MGBA_CORE_NEW_ERROR_CREATE_FAILED);
    }

	mCoreInitConfig(core->core, nullptr);

	core->core->opts.skipBios = true;

    if(!core->core->init(core->core)) {
        // core->cpu/board were never populated (GBACoreCreate leaves them null and init() only
        // sets them on success), so core->deinit() would crash; free the mCore shell directly.
        // struct GBACore has struct mCore as its first member, so freeing through the base
        // pointer mCoreCreate handed back is the same as freeing the allocation mgba made.
        std::free(core->core);
        core->core = nullptr;
        return fail(core, error_out, MGBA_CORE_NEW_ERROR_INIT_FAILED);
    }

    core->rom = std::vector(rom, rom + rom_size);
    core->rom_vf = VFileFromConstMemory(core->rom.data(), core->rom.size());
    if(core->rom_vf == nullptr || !core->core->loadROM(core->core, core->rom_vf)) {
        // On every current loadROM failure path mGBA has already given up ownership of the vf
        // (or never took it), so closing it ourselves here cannot double-free.
        if(core->rom_vf != nullptr) {
            core->rom_vf->close(core->rom_vf);
            core->rom_vf = nullptr;
        }
        return fail(core, error_out, MGBA_CORE_NEW_ERROR_BAD_ROM);
    }

    if(sram_size > 0) {
        // VFileMemChunk copies `sram` into its own growable buffer and its truncate() can expand
        // (unlike VFileFromMemory's, which cannot grow past `sram_size`). That matters because an
        // undersized .sav (e.g. a 64 KiB FLASH512 save for a FLASH1M game) needs to grow when
        // GBASavedataInitFlash upgrades it; VFileFromMemory would hand back a NULL map() and the
        // subsequent memset would segfault. mGBA takes ownership of this vf via loadSave() and
        // closes it itself in GBAUnloadROM, so we don't keep a copy of the pointer or the bytes.
        VFile *sram_vf = VFileMemChunk(sram, sram_size);
        core->core->loadSave(core->core, sram_vf);
    }

    if(bios_size > 0) {
        core->bios = std::vector(bios, bios + bios_size);
        core->bios_vf = VFileFromConstMemory(core->bios.data(), core->bios.size());
        if(core->bios_vf == nullptr || !core->core->loadBIOS(core->core, core->bios_vf, 0)) {
            // loadBIOS only takes ownership (sets gba->biosVf) once GBAIsBIOS() has accepted the
            // file; on rejection the vf is still ours to close.
            if(core->bios_vf != nullptr) {
                core->bios_vf->close(core->bios_vf);
                core->bios_vf = nullptr;
            }
            return fail(core, error_out, MGBA_CORE_NEW_ERROR_BAD_BIOS);
        }
    }

    const mCoreMemoryBlock *blocks;
    std::size_t block_count = core->core->listMemoryBlocks(core->core, &blocks);

    for(std::size_t i = 0; i < block_count; i++) {
        if(blocks[i].start == 0x2000000) {
            core->ewram = blocks[i].id;
        }
        if(blocks[i].start == 0x3000000) {
            core->iwram = blocks[i].id;
        }
    }

    if(core->ewram == ~0 || core->iwram == ~0) {
        return fail(core, error_out, MGBA_CORE_NEW_ERROR_MISSING_MEMORY_BLOCKS);
    }

    core->pixels.resize(240 * 160);
    core->core->setVideoBuffer(core->core, core->pixels.data(), 240);

    // Initialised last (after every fallible step above) so no failure path ever needs to tear
    // these back down.
    mAudioBufferInit(&core->resampled, RESAMPLED_CAPACITY_FRAMES, 2);
    mAudioResamplerInit(&core->resampler, mINTERPOLATOR_SINC);
    mAudioResamplerSetDestination(&core->resampler, &core->resampled, OUTPUT_SAMPLE_RATE);

	core->core->rtc.override = RTC_FAKE_EPOCH;
	core->core->rtc.value = 0;

    core->core->reset(core->core);

    return core;
}

extern "C" void mgba_rs_core_free(MGBACoreRaw *core) {
    if(core == nullptr) {
        return;
    }

    core->core->deinit(core->core);
    mAudioResamplerDeinit(&core->resampler);
    mAudioBufferDeinit(&core->resampled);

    delete core;
}

// Whether to resample the core's mix for the frontend. The core mixes either way (it is part of
// emulation; a full buffer just drops samples), so nothing here affects emulation or save states.
// Both buffers are cleared on a change so a re-enable does not play what was mixed meanwhile.
extern "C" void mgba_rs_core_set_audio_enabled(MGBACoreRaw *core, bool enabled) {
    if(core->audio_enabled == enabled) {
        return;
    }
    core->audio_enabled = enabled;
    mAudioBufferClear(core->core->getAudioBuffer(core->core));
    mAudioBufferClear(&core->resampled);
}

// Pop up to `max_frames` stereo frames at 48 kHz mixed since the last read.
extern "C" std::size_t mgba_rs_core_read_audio(MGBACoreRaw *core, std::int16_t *out, std::size_t max_frames) {
    if(!core->audio_enabled) {
        return 0;
    }
    // The source rate follows the SOUNDBIAS resolution bits, so re-read it every time.
    mAudioResamplerSetSource(&core->resampler, core->core->getAudioBuffer(core->core), core->core->audioSampleRate(core->core), true);
    mAudioResamplerProcess(&core->resampler);
    return mAudioBufferRead(&core->resampled, out, max_frames);
}

extern "C" void mgba_rs_core_run_frame(MGBACoreRaw *core) {
    core->core->runFrame(core->core);
}

extern "C" void mgba_rs_core_reset(MGBACoreRaw *core) {
    core->core->reset(core->core);
}

extern "C" const std::uint32_t *mgba_rs_core_get_pixels(const MGBACoreRaw *core) {
    return core->pixels.data();
}

extern "C" void mgba_rs_core_set_input(MGBACoreRaw *core, std::uint16_t input) {
    core->core->setKeys(core->core, input);
}

// mGBA malloc()s a fresh copy of the save data on every call (see _GBACoreSavedataClone); the
// caller must free it back through mgba_rs_core_free_sram_clone once it has copied what it needs.
// NULL/0 whenever the save size is not yet known (e.g. the game was closed before mGBA detected
// its save type), not an error.
extern "C" const void *mgba_rs_core_get_sram(const MGBACoreRaw *core, std::size_t &size) {
    void *sram = nullptr;
    size = core->core->savedataClone(core->core, &sram);
    return sram;
}

// Frees a clone handed back by mgba_rs_core_get_sram. Must go through this shim (not the caller's
// own allocator) since mGBA's _GBACoreSavedataClone allocates it with this binary's malloc().
extern "C" void mgba_rs_core_free_sram_clone(void *p) {
    std::free(p);
}

#define SAVE_STATE_FLAGS (SAVESTATE_SAVEDATA | SAVESTATE_RTC)

extern "C" std::size_t mgba_rs_core_create_save_state(const MGBACoreRaw *core, std::byte *data, std::size_t data_size) {
	auto *vf = VFileMemChunk(NULL, 0);
    bool ok = mCoreSaveStateNamed(core->core, vf, SAVE_STATE_FLAGS);
	std::size_t size = static_cast<std::size_t>(vf->size(vf));
	ok = ok && size <= data_size;
	if(ok) {
	    vf->seek(vf, 0, SEEK_SET);
	    ok = vf->read(vf, data, size) == static_cast<ssize_t>(size);
	}
	vf->close(vf);
    return ok ? size : 0;
}

extern "C" bool mgba_rs_core_load_save_state(MGBACoreRaw *core, const std::byte *data, std::size_t data_size) {
    if(data_size == 0) {
        return false;
    }
    auto *vf = VFileFromConstMemory(data, data_size);
    if(vf == nullptr) {
        return false;
    }
    auto success = mCoreLoadStateNamed(core->core, vf, SAVE_STATE_FLAGS);
    vf->close(vf);
    return success;
}

extern "C" void *mgba_rs_core_get_ewram(MGBACoreRaw *core) {
    std::size_t q;
    auto *ptr = core->core->getMemoryBlock(core->core, core->ewram, &q);
    if(ptr != nullptr) {
        return ptr;
    }
    std::printf("Failed to get ewram\n");
    std::terminate();
}

extern "C" void *mgba_rs_core_get_iwram(MGBACoreRaw *core) {
    std::size_t q;
    auto *ptr = core->core->getMemoryBlock(core->core, core->iwram, &q);
    if(ptr != nullptr) {
        return ptr;
    }
    std::printf("Failed to get iwram\n");
    std::terminate();
}

// mGBA's region ids (enum GBAMemoryRegion); spelled out so the internal headers, whose struct
// layouts depend on mGBA's build configuration, stay out of this file.
static constexpr std::size_t GBA_REGION_ID_PALETTE_RAM = 0x5;
static constexpr std::size_t GBA_REGION_ID_VRAM = 0x6;
static constexpr std::size_t GBA_REGION_ID_OAM = 0x7;
static constexpr std::size_t GBA_REGION_ID_SRAM_MIRROR = 0xF;

// Direct access to a memory region by mGBA region id. The save data is asked for through the
// SRAM mirror id, which always yields the whole buffer (the SRAM id hands out only the current
// bank for 1 MiB flash, sized as if it were the whole chip). Null with size 0 when absent.
extern "C" std::uint8_t *mgba_rs_core_get_region(MGBACoreRaw *core, std::uint32_t region, std::size_t &size) {
    size = 0;
    std::size_t id;
    switch(region) {
        case 0: id = GBA_REGION_ID_PALETTE_RAM; break;
        case 1: id = GBA_REGION_ID_VRAM; break;
        case 2: id = GBA_REGION_ID_OAM; break;
        case 3: id = GBA_REGION_ID_SRAM_MIRROR; break;
        default: return nullptr;
    }
    std::size_t block_size = 0;
    auto *ptr = static_cast<std::uint8_t *>(core->core->getMemoryBlock(core->core, id, &block_size));
    if(ptr == nullptr) {
        return nullptr;
    }
    size = block_size;
    return ptr;
}

// Write through mGBA's patch path so the renderer's palette/VRAM/OAM caches see the change.
extern "C" void mgba_rs_core_patch_write(MGBACoreRaw *core, std::uint32_t address, const std::uint8_t *data, std::size_t length) {
    for(std::size_t i = 0; i < length; i++) {
        core->core->rawWrite8(core->core, address + static_cast<std::uint32_t>(i), -1, data[i]);
    }
}
