#include <cstdint>
#include <cstddef>
#include <exception>
#include <cstdio>
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
    mCore *core;

    VFile *rom_vf;
    std::vector<std::byte> rom;

    VFile *sram_vf;
    std::vector<std::byte> sram;

    VFile *bios_vf;
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

extern "C" MGBACoreRaw *mgba_rs_core_new(
    const std::byte *rom,
    std::size_t rom_size,
    const std::byte *sram,
    std::size_t sram_size,
    const std::byte *bios,
    std::size_t bios_size
) {
    mLogSetDefaultLogger(&logger);

    auto *core = new MGBACoreRaw();
    core->core = mCoreCreate(mPLATFORM_GBA);

    if(core->core == nullptr) {
        std::printf("Failed to create mGBA instance\n");
        std::terminate();
    }

	mCoreInitConfig(core->core, nullptr);

	core->core->opts.skipBios = true;

    if(!core->core->init(core->core)) {
        std::printf("Failed to init mGBA\n");
        std::terminate();
    }

    core->rom = std::vector(rom, rom + rom_size);
    core->rom_vf = VFileFromMemory(core->rom.data(), core->rom.size());
    core->core->loadROM(core->core, core->rom_vf);

    if(sram_size > 0) {
        core->sram = std::vector(sram, sram + sram_size);
        core->sram_vf = VFileFromMemory(core->sram.data(), core->sram.size());
        core->core->loadSave(core->core, core->sram_vf);
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

    if(core->ewram == ~0) {
        std::printf("Failed to find ewram\n");
        std::terminate();
    }

    if(core->iwram == ~0) {
        std::printf("Failed to find iwram\n");
        std::terminate();
    }

    core->pixels.resize(240 * 160);
    core->core->setVideoBuffer(core->core, core->pixels.data(), 240);

    mAudioBufferInit(&core->resampled, RESAMPLED_CAPACITY_FRAMES, 2);
    mAudioResamplerInit(&core->resampler, mINTERPOLATOR_SINC);
    mAudioResamplerSetDestination(&core->resampler, &core->resampled, OUTPUT_SAMPLE_RATE);

    if(bios_size > 0) {
        core->bios = std::vector(bios, bios + bios_size);
        core->bios_vf = VFileFromMemory(core->bios.data(), core->bios.size());
        if(!core->core->loadBIOS(core->core, core->bios_vf, 0)) {
            std::printf("Bad BIOS\n");
            std::terminate();
        }
    }

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

extern "C" const void *mgba_rs_core_get_sram(const MGBACoreRaw *core, std::size_t &size) {
    void *sram = nullptr;
    size = core->core->savedataClone(core->core, &sram);
    return sram;
}

#define SAVE_STATE_FLAGS (SAVESTATE_SAVEDATA | SAVESTATE_RTC)

extern "C" std::size_t mgba_rs_core_create_save_state(const MGBACoreRaw *core, std::byte *data, std::size_t data_size) {
	auto *vf = VFileMemChunk(NULL, 0);
    bool read_successfully = mCoreSaveStateNamed(core->core, vf, SAVE_STATE_FLAGS);
	size_t size = vf->size(vf);
	if(size <= data_size && read_successfully) {
	    vf->seek(vf, 0, SEEK_SET);
        vf->read(vf, data, size);
	}
	vf->close(vf);
    return size;
}

extern "C" bool mgba_rs_core_load_save_state(MGBACoreRaw *core, const std::byte *data, std::size_t data_size) {
    auto *vf = VFileFromMemory(const_cast<std::byte *>(data), data_size);
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
