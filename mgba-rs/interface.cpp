#include <cstdint>
#include <cstddef>
#include <cstdlib>
#include <cstdio>
#include <cstring>
#include <climits>
#include <exception>
#include <initializer_list>
#include <vector>

// libmgba's own configuration (see build/mgba/build.ninja): the link cable code below uses
// mGBA's internal headers, whose struct layouts follow these.
#define ENABLE_VFS
#define ENABLE_DIRECTORIES
#define ENABLE_VFS_FD
#define BUILD_STATIC
#define M_CORE_GB
#define M_CORE_GBA
// mGBA's CMake defines USE_PTHREADS on every UNIX build (Windows takes the _WIN32 branch of
// mgba-util/threading.h on both sides). Without it here, `Mutex` is a `void*` in this file but
// a `pthread_mutex_t` in libmgba, so `GBASIOLockstepCoordinator` is 56 bytes shorter here and
// the coordinator this file allocates is overrun by the library: heap corruption on link.
#ifndef _WIN32
#define USE_PTHREADS
#endif

#include <mgba/core/core.h>
#include <mgba/core/log.h>
#include <mgba/core/serialize.h>
#include <mgba-util/vfs.h>
#include <mgba-util/audio-buffer.h>
#include <mgba-util/audio-resampler.h>
#include <mgba/gba/core.h>

// mGBA's log goes nowhere, unless SUPERSHUCKIE_MGBA_LOG is set: 1 prints everything up to INFO,
// 2 also the serial (link cable) category's DEBUG lines, to stderr. A debugging aid for the link
// cable; never on in normal use.
static void nope_log(struct mLogger*, int category, enum mLogLevel level, const char* format, va_list args) {
    static int debug = -1;
    if(debug < 0) {
        const char *setting = std::getenv("SUPERSHUCKIE_MGBA_LOG");
        debug = setting != nullptr ? std::atoi(setting) : 0;
    }
    constexpr int GBA_SIO_CATEGORY = 6;
    if(debug > 0 && (level <= mLOG_INFO || (debug > 1 && category == GBA_SIO_CATEGORY))) {
        std::fprintf(stderr, "[mgba %d] ", category);
        std::vfprintf(stderr, format, args);
        std::fputc(10, stderr);
    }
}

static mLogger logger = {
    .log = nope_log
};


struct MGBALinkState;
struct MGBAReplayState;

struct MGBACoreRaw {
    mCore *core = nullptr;

    // The link cable (see the end of this file): the lockstep driver while a cable is in, and
    // the replay driver while a recording of a linked game plays. Both outlive their
    // attachments, so a core re-links without reallocating.
    MGBALinkState *link = nullptr;
    MGBAReplayState *replay = nullptr;

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

static void link_free(MGBACoreRaw *core);

extern "C" void mgba_rs_core_free(MGBACoreRaw *core) {
    if(core == nullptr) {
        return;
    }

    link_free(core);
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

// ---------------------------------------------------------------------------------------------
// Link cable
//
// Two cores on one thread joined by mGBA's lockstep coordinator, run cooperatively: instead of
// blocking a thread, the coordinator's sleep/wake calls set a flag, and the caller only steps
// cores that are awake (see `supershuckie_core::link`). While linked every effect the SIO
// driver has on the game is logged per frame (the `SerialIn` replay packet's payload), and a
// replay driver plays such a log back so a recording of a linked game runs alone.
//
// The lockstep coordinator and driver live in mGBA's internal headers, whose struct layouts
// depend on the build configuration: the defines at the top of this file are libmgba's own, so
// this file sees the same layouts.

#include <mgba/core/lockstep.h>
#include <mgba/core/timing.h>
#include <mgba/internal/gba/gba.h>
#include <mgba/internal/gba/sio.h>
#include <mgba/internal/gba/sio/lockstep.h>

// The per-frame serial log: a tag stream (little-endian).
enum MGBALinkLogTag : std::uint8_t {
    LINK_LOG_CONFIG = 0x10,    // devices u8, id u8: what connectedDevices()/deviceId() return from here on
    LINK_LOG_SET_MODE = 0x11,  // siocnt u16, rcnt u16: the registers right after the driver's setMode()
    LINK_LOG_START = 0x12,     // ok u8: what start() returned
    LINK_LOG_MULTI = 0x13,     // data u16[4]: what finishMultiplayer() delivered
    LINK_LOG_NORMAL8 = 0x14,   // data u8: what finishNormal8() returned
    LINK_LOG_NORMAL32 = 0x15,  // data u32: what finishNormal32() returned
    LINK_LOG_ASYNC = 0x16,     // at i32, siocnt u16, rcnt u16, finish i32: a change the driver made on its own at
                               // frame time `at` (cycles from the frame start); `finish` is where it (re)scheduled
                               // the transfer completion, also from the frame start, or LINK_LOG_NO_FINISH
    LINK_LOG_ATTACH = 0x1E,    // the cable went in this frame
    LINK_LOG_DETACH = 0x1F,    // the cable came out this frame
};

static constexpr std::int32_t LINK_LOG_NO_FINISH = INT32_MIN;

// Most log bytes kept per frame (a runaway driver cannot grow a replay without bound).
static constexpr std::size_t LINK_LOG_MAX_BYTES = 32768;

struct MGBALinkCoordinatorRaw {
    GBASIOLockstepCoordinator coordinator;
};

struct MGBALinkState;

struct MGBALinkUser {
    mLockstepUser d;
    MGBALinkState *link;
};

struct MGBALinkState {
    // The lockstep driver: first, so the driver pointer mGBA hands the vtable is this struct.
    GBASIOLockstepDriver driver;
    // The vtable as mGBA filled it, forwarded to by the logging wrappers.
    GBASIODriver original;
    void (*original_event)(mTiming *, void *, std::uint32_t) = nullptr;
    MGBALinkUser user;
    MGBACoreRaw *core = nullptr;
    MGBALinkCoordinatorRaw *coordinator = nullptr;
    bool attached = false;
    bool asleep = false;
    int requested_id = 0;
    int player_id = -1;
    int last_devices = -1;
    int last_id = -1;
    std::int32_t frame_start = 0;
    std::vector<std::uint8_t> log;
};

// What one logged sync entry the replay driver hands back looks like.
struct MGBAReplayEntry {
    std::uint8_t tag;
    std::uint16_t siocnt, rcnt;
    std::uint16_t multi[4];
    std::uint32_t data;
    std::uint8_t ok;
};

struct MGBAReplayAsync {
    std::int32_t at;
    std::uint16_t siocnt, rcnt;
    std::int32_t finish;
};

struct MGBAReplayState {
    GBASIODriver d;
    mTimingEvent event;
    MGBACoreRaw *core = nullptr;
    bool attached = false;
    int devices = 0;
    int id = 0;
    std::int32_t frame_start = 0;
    std::vector<MGBAReplayEntry> sync;
    std::size_t sync_head = 0;
    std::vector<MGBAReplayAsync> async_entries;
    std::size_t async_head = 0;
    std::uint64_t misses = 0;
};

static GBA *gba_of(MGBACoreRaw *core) {
    return static_cast<GBA *>(core->core->board);
}

static void log_push(MGBALinkState *link, std::initializer_list<std::uint8_t> bytes) {
    if(link->log.size() + bytes.size() > LINK_LOG_MAX_BYTES) {
        return;
    }
    link->log.insert(link->log.end(), bytes.begin(), bytes.end());
}

static void log_u16(std::vector<std::uint8_t> &log, std::uint16_t v) {
    log.push_back(static_cast<std::uint8_t>(v));
    log.push_back(static_cast<std::uint8_t>(v >> 8));
}

static void log_u32(std::vector<std::uint8_t> &log, std::uint32_t v) {
    log_u16(log, static_cast<std::uint16_t>(v));
    log_u16(log, static_cast<std::uint16_t>(v >> 16));
}

static void log_config_if_changed(MGBALinkState *link) {
    int devices = link->original.connectedDevices(&link->driver.d);
    int id = link->original.deviceId(&link->driver.d);
    if(devices != link->last_devices || id != link->last_id) {
        link->last_devices = devices;
        link->last_id = id;
        log_push(link, { LINK_LOG_CONFIG, static_cast<std::uint8_t>(devices), static_cast<std::uint8_t>(id) });
    }
}

// The logging wrappers around the lockstep driver's vtable.

static void link_set_mode(GBASIODriver *d, GBASIOMode mode) {
    auto *link = reinterpret_cast<MGBALinkState *>(d);
    link->original.setMode(d, mode);
    if(link->log.size() + 5 <= LINK_LOG_MAX_BYTES) {
        link->log.push_back(LINK_LOG_SET_MODE);
        log_u16(link->log, d->p->siocnt);
        log_u16(link->log, d->p->rcnt);
    }
}

static int link_connected_devices(GBASIODriver *d) {
    auto *link = reinterpret_cast<MGBALinkState *>(d);
    log_config_if_changed(link);
    return link->last_devices;
}

static int link_device_id(GBASIODriver *d) {
    auto *link = reinterpret_cast<MGBALinkState *>(d);
    log_config_if_changed(link);
    return link->last_id;
}

static bool link_start(GBASIODriver *d) {
    auto *link = reinterpret_cast<MGBALinkState *>(d);
    bool ok = link->original.start(d);
    log_push(link, { LINK_LOG_START, static_cast<std::uint8_t>(ok) });
    return ok;
}

static void link_finish_multiplayer(GBASIODriver *d, std::uint16_t data[4]) {
    auto *link = reinterpret_cast<MGBALinkState *>(d);
    link->original.finishMultiplayer(d, data);
    if(link->log.size() + 9 <= LINK_LOG_MAX_BYTES) {
        link->log.push_back(LINK_LOG_MULTI);
        for(int i = 0; i < 4; i++) {
            log_u16(link->log, data[i]);
        }
    }
}

static std::uint8_t link_finish_normal8(GBASIODriver *d) {
    auto *link = reinterpret_cast<MGBALinkState *>(d);
    std::uint8_t data = link->original.finishNormal8(d);
    log_push(link, { LINK_LOG_NORMAL8, data });
    return data;
}

static std::uint32_t link_finish_normal32(GBASIODriver *d) {
    auto *link = reinterpret_cast<MGBALinkState *>(d);
    std::uint32_t data = link->original.finishNormal32(d);
    if(link->log.size() + 5 <= LINK_LOG_MAX_BYTES) {
        link->log.push_back(LINK_LOG_NORMAL32);
        log_u32(link->log, data);
    }
    return data;
}

// The lockstep driver's own timing event: whatever it changes behind the game's back (the
// ready/SD bits, a secondary's transfer start) is logged with when it happened.
static void link_event(mTiming *timing, void *context, std::uint32_t cycles_late) {
    auto *link = static_cast<MGBALinkState *>(context);
    GBASIO *sio = link->driver.d.p;
    std::int32_t at = static_cast<std::int32_t>(link->driver.event.when - static_cast<std::uint32_t>(link->frame_start));
    std::uint16_t siocnt0 = sio->siocnt;
    std::uint16_t rcnt0 = sio->rcnt;
    bool scheduled0 = mTimingIsScheduled(timing, &sio->completeEvent);
    std::uint32_t when0 = sio->completeEvent.when;

    link->original_event(timing, context, cycles_late);

    bool scheduled1 = mTimingIsScheduled(timing, &sio->completeEvent);
    bool rescheduled = scheduled1 && (!scheduled0 || sio->completeEvent.when != when0);
    if(sio->siocnt != siocnt0 || sio->rcnt != rcnt0 || rescheduled) {
        if(link->log.size() + 13 <= LINK_LOG_MAX_BYTES) {
            link->log.push_back(LINK_LOG_ASYNC);
            log_u32(link->log, static_cast<std::uint32_t>(at));
            log_u16(link->log, sio->siocnt);
            log_u16(link->log, sio->rcnt);
            std::int32_t finish = rescheduled ? static_cast<std::int32_t>(sio->completeEvent.when - static_cast<std::uint32_t>(link->frame_start)) : LINK_LOG_NO_FINISH;
            log_u32(link->log, static_cast<std::uint32_t>(finish));
        }
    }
}

static void link_user_sleep(mLockstepUser *user) {
    reinterpret_cast<MGBALinkUser *>(user)->link->asleep = true;
}

static void link_user_wake(mLockstepUser *user) {
    reinterpret_cast<MGBALinkUser *>(user)->link->asleep = false;
}

static int link_user_requested_id(mLockstepUser *user) {
    return reinterpret_cast<MGBALinkUser *>(user)->link->requested_id;
}

static void link_user_player_id_changed(mLockstepUser *user, int id) {
    reinterpret_cast<MGBALinkUser *>(user)->link->player_id = id;
}

extern "C" MGBALinkCoordinatorRaw *mgba_rs_link_coordinator_new() {
    auto *coordinator = new MGBALinkCoordinatorRaw();
    GBASIOLockstepCoordinatorInit(&coordinator->coordinator);
    return coordinator;
}

extern "C" void mgba_rs_link_coordinator_free(MGBALinkCoordinatorRaw *coordinator) {
    if(coordinator == nullptr) {
        return;
    }
    GBASIOLockstepCoordinatorDeinit(&coordinator->coordinator);
    delete coordinator;
}

// Plug this core into `coordinator` as its first (0) or second (1) player. The lockstep driver
// replaces whatever SIO driver was installed.
extern "C" bool mgba_rs_core_link_attach(MGBACoreRaw *core, MGBALinkCoordinatorRaw *coordinator, bool first) {
    if(core->link != nullptr && core->link->attached) {
        return false;
    }
    if(core->replay != nullptr && core->replay->attached) {
        return false;
    }
    if(core->link == nullptr) {
        core->link = new MGBALinkState();
    }
    auto *link = core->link;
    link->core = core;
    link->coordinator = coordinator;
    link->requested_id = first ? 0 : 1;
    link->player_id = -1;
    link->last_devices = -1;
    link->last_id = -1;
    link->asleep = false;
    link->log.clear();
    link->user.d.sleep = link_user_sleep;
    link->user.d.wake = link_user_wake;
    link->user.d.requestedId = link_user_requested_id;
    link->user.d.playerIdChanged = link_user_player_id_changed;
    link->user.link = link;

    GBASIOLockstepDriverCreate(&link->driver, &link->user.d);
    link->original = link->driver.d;
    link->original_event = link->driver.event.callback;
    link->driver.d.setMode = link_set_mode;
    link->driver.d.connectedDevices = link_connected_devices;
    link->driver.d.deviceId = link_device_id;
    link->driver.d.start = link_start;
    link->driver.d.finishMultiplayer = link_finish_multiplayer;
    link->driver.d.finishNormal8 = link_finish_normal8;
    link->driver.d.finishNormal32 = link_finish_normal32;
    link->driver.event.callback = link_event;
    link->driver.event.context = link;

    link->frame_start = mTimingCurrentTime(core->core->timing);
    GBASIOLockstepCoordinatorAttach(&coordinator->coordinator, &link->driver);
    core->core->setPeripheral(core->core, mPERIPH_GBA_LINK_PORT, &link->driver.d);
    link->attached = true;
    log_push(link, { LINK_LOG_ATTACH });
    log_config_if_changed(link);
    return true;
}

// Pull the cable: the lockstep driver comes out and the plain (no cable) behaviour is back.
extern "C" void mgba_rs_core_link_detach(MGBACoreRaw *core) {
    auto *link = core->link;
    if(link == nullptr || !link->attached) {
        return;
    }
    // setPeripheral(NULL) deinits the driver, which removes the player from the coordinator.
    core->core->setPeripheral(core->core, mPERIPH_GBA_LINK_PORT, nullptr);
    if(link->driver.coordinator != nullptr) {
        GBASIOLockstepCoordinatorDetach(&link->coordinator->coordinator, &link->driver);
    }
    link->attached = false;
    link->asleep = false;
    link->coordinator = nullptr;
    log_push(link, { LINK_LOG_DETACH });
}

extern "C" bool mgba_rs_core_link_is_attached(const MGBACoreRaw *core) {
    return core->link != nullptr && core->link->attached;
}

extern "C" bool mgba_rs_core_link_is_asleep(const MGBACoreRaw *core) {
    return core->link != nullptr && core->link->attached && core->link->asleep;
}

extern "C" int mgba_rs_core_link_player_id(const MGBACoreRaw *core) {
    return core->link != nullptr ? core->link->player_id : -1;
}

// One slice of emulation (mGBA's runLoop: until the next timing event). Returns how many frames
// completed in it (0 or 1); the pixel buffer holds the new frame when 1.
extern "C" std::uint32_t mgba_rs_core_run_loop(MGBACoreRaw *core) {
    GBA *gba = gba_of(core);
    std::uint32_t before = gba->video.frameCounter;
    core->core->runLoop(core->core);
    return gba->video.frameCounter - before;
}

// The core's cycle clock (wraps every couple of minutes; take differences).
extern "C" std::int32_t mgba_rs_core_timing_now(const MGBACoreRaw *core) {
    return mTimingCurrentTime(core->core->timing);
}

// A new frame begins now: what the driver does from here on is logged against this instant.
extern "C" void mgba_rs_core_link_frame_started(MGBACoreRaw *core) {
    if(core->link != nullptr) {
        core->link->frame_start = mTimingCurrentTime(core->core->timing);
    }
}

// Copy (and clear) the serial log gathered since the last call. Returns the bytes needed;
// nothing is copied when `capacity` is short (call again with a bigger buffer).
extern "C" std::size_t mgba_rs_core_link_take_log(MGBACoreRaw *core, std::uint8_t *out, std::size_t capacity) {
    if(core->link == nullptr) {
        return 0;
    }
    auto &log = core->link->log;
    std::size_t size = log.size();
    if(size > capacity) {
        return size;
    }
    if(size > 0) {
        std::memcpy(out, log.data(), size);
        log.clear();
    }
    return size;
}

// ---------------------------------------------------------------------------------------------
// The replay driver: answers the game from a recorded log.

static bool replay_init(GBASIODriver *) { return true; }
static void replay_deinit(GBASIODriver *) {}
static void replay_reset(GBASIODriver *) {}
static std::uint32_t replay_driver_id(const GBASIODriver *) { return 0x79616C50; } // "Play"
static bool replay_handles_mode(GBASIODriver *, GBASIOMode) { return true; }
static std::uint16_t replay_write_siocnt(GBASIODriver *, std::uint16_t value) { return value; }
static std::uint16_t replay_write_rcnt(GBASIODriver *, std::uint16_t value) { return value; }

// The next sync entry if it is a `tag`, else a miss (nothing is consumed: the game may be
// asking in an order the log never saw, and the log's next entry stays for its own call).
static const MGBAReplayEntry *replay_next(MGBAReplayState *replay, std::uint8_t tag) {
    if(replay->sync_head < replay->sync.size() && replay->sync[replay->sync_head].tag == tag) {
        return &replay->sync[replay->sync_head++];
    }
    replay->misses++;
    return nullptr;
}

// Configuration entries sit in the same stream as the calls that first saw them: apply every
// one at the head before answering.
static void replay_apply_config(MGBAReplayState *replay) {
    while(replay->sync_head < replay->sync.size() && replay->sync[replay->sync_head].tag == LINK_LOG_CONFIG) {
        const auto &entry = replay->sync[replay->sync_head++];
        replay->devices = entry.data & 0xFF;
        replay->id = (entry.data >> 8) & 0xFF;
    }
}

static void replay_set_mode(GBASIODriver *d, GBASIOMode) {
    auto *replay = reinterpret_cast<MGBAReplayState *>(d);
    replay_apply_config(replay);
    if(const auto *entry = replay_next(replay, LINK_LOG_SET_MODE)) {
        d->p->siocnt = entry->siocnt;
        d->p->rcnt = entry->rcnt;
    }
}

static int replay_connected_devices(GBASIODriver *d) {
    auto *replay = reinterpret_cast<MGBAReplayState *>(d);
    replay_apply_config(replay);
    return replay->devices;
}

static int replay_device_id(GBASIODriver *d) {
    auto *replay = reinterpret_cast<MGBAReplayState *>(d);
    replay_apply_config(replay);
    return replay->id;
}

static bool replay_start(GBASIODriver *d) {
    auto *replay = reinterpret_cast<MGBAReplayState *>(d);
    replay_apply_config(replay);
    if(const auto *entry = replay_next(replay, LINK_LOG_START)) {
        return entry->ok != 0;
    }
    return false;
}

static void replay_finish_multiplayer(GBASIODriver *d, std::uint16_t data[4]) {
    auto *replay = reinterpret_cast<MGBAReplayState *>(d);
    replay_apply_config(replay);
    if(const auto *entry = replay_next(replay, LINK_LOG_MULTI)) {
        std::memcpy(data, entry->multi, sizeof(entry->multi));
    }
    else {
        std::memset(data, 0xFF, sizeof(std::uint16_t) * 4);
    }
}

static std::uint8_t replay_finish_normal8(GBASIODriver *d) {
    auto *replay = reinterpret_cast<MGBAReplayState *>(d);
    replay_apply_config(replay);
    if(const auto *entry = replay_next(replay, LINK_LOG_NORMAL8)) {
        return static_cast<std::uint8_t>(entry->data);
    }
    return 0xFF;
}

static std::uint32_t replay_finish_normal32(GBASIODriver *d) {
    auto *replay = reinterpret_cast<MGBAReplayState *>(d);
    replay_apply_config(replay);
    if(const auto *entry = replay_next(replay, LINK_LOG_NORMAL32)) {
        return entry->data;
    }
    return 0xFFFFFFFF;
}

static void replay_schedule_next_async(MGBAReplayState *replay);

// A logged change the lockstep driver made on its own, at its recorded time.
static void replay_async_event(mTiming *timing, void *context, std::uint32_t) {
    auto *replay = static_cast<MGBAReplayState *>(context);
    if(replay->async_head < replay->async_entries.size()) {
        const auto &entry = replay->async_entries[replay->async_head++];
        GBASIO *sio = replay->d.p;
        sio->siocnt = entry.siocnt;
        sio->rcnt = entry.rcnt;
        if(entry.finish != LINK_LOG_NO_FINISH) {
            mTimingDeschedule(timing, &sio->completeEvent);
            mTimingScheduleAbsolute(timing, &sio->completeEvent, replay->frame_start + entry.finish);
        }
    }
    replay_schedule_next_async(replay);
}

static void replay_schedule_next_async(MGBAReplayState *replay) {
    mTiming *timing = replay->core->core->timing;
    mTimingDeschedule(timing, &replay->event);
    if(replay->async_head < replay->async_entries.size()) {
        std::int32_t at = replay->frame_start + replay->async_entries[replay->async_head].at;
        std::int32_t until = at - mTimingCurrentTime(timing);
        if(until < 0) {
            until = 0;
        }
        mTimingSchedule(timing, &replay->event, until);
    }
}

static bool replay_attach(MGBACoreRaw *core) {
    if(core->link != nullptr && core->link->attached) {
        return false;
    }
    if(core->replay == nullptr) {
        core->replay = new MGBAReplayState();
    }
    auto *replay = core->replay;
    if(replay->attached) {
        return true;
    }
    replay->core = core;
    std::memset(&replay->d, 0, sizeof(replay->d));
    replay->d.init = replay_init;
    replay->d.deinit = replay_deinit;
    replay->d.reset = replay_reset;
    replay->d.driverId = replay_driver_id;
    replay->d.loadState = nullptr;
    replay->d.saveState = nullptr;
    replay->d.setMode = replay_set_mode;
    replay->d.handlesMode = replay_handles_mode;
    replay->d.connectedDevices = replay_connected_devices;
    replay->d.deviceId = replay_device_id;
    replay->d.writeSIOCNT = replay_write_siocnt;
    replay->d.writeRCNT = replay_write_rcnt;
    replay->d.start = replay_start;
    replay->d.finishMultiplayer = replay_finish_multiplayer;
    replay->d.finishNormal8 = replay_finish_normal8;
    replay->d.finishNormal32 = replay_finish_normal32;
    replay->event.context = replay;
    replay->event.callback = replay_async_event;
    replay->event.name = "SuperShuckie SIO replay";
    replay->event.priority = 0x80;
    replay->devices = 0;
    replay->id = 0;
    replay->sync.clear();
    replay->sync_head = 0;
    replay->async_entries.clear();
    replay->async_head = 0;
    replay->misses = 0;
    replay->frame_start = mTimingCurrentTime(core->core->timing);
    core->core->setPeripheral(core->core, mPERIPH_GBA_LINK_PORT, &replay->d);
    replay->attached = true;
    return true;
}

extern "C" void mgba_rs_core_replay_detach(MGBACoreRaw *core) {
    auto *replay = core->replay;
    if(replay == nullptr || !replay->attached) {
        return;
    }
    mTimingDeschedule(core->core->timing, &replay->event);
    core->core->setPeripheral(core->core, mPERIPH_GBA_LINK_PORT, nullptr);
    replay->attached = false;
    replay->sync.clear();
    replay->sync_head = 0;
    replay->async_entries.clear();
    replay->async_head = 0;
}

extern "C" bool mgba_rs_core_replay_is_attached(const MGBACoreRaw *core) {
    return core->replay != nullptr && core->replay->attached;
}

extern "C" std::uint64_t mgba_rs_core_replay_misses(const MGBACoreRaw *core) {
    return core->replay != nullptr ? core->replay->misses : 0;
}

// Reads a little-endian log.
struct LogReader {
    const std::uint8_t *p;
    std::size_t left;
    bool ok = true;

    std::uint8_t u8() {
        if(left < 1) { ok = false; return 0; }
        left--;
        return *p++;
    }
    std::uint16_t u16() {
        std::uint16_t lo = u8();
        return static_cast<std::uint16_t>(lo | (u8() << 8));
    }
    std::uint32_t u32() {
        std::uint32_t lo = u16();
        return lo | (static_cast<std::uint32_t>(u16()) << 16);
    }
};

// One frame's log, at the frame boundary before that frame runs: the replay driver goes in
// (if it is not in yet), the frame's sync entries replace whatever was left of the previous
// frame's, and its asynchronous changes are scheduled from now. Returns 0 when the log does
// not parse (nothing is applied), 1 when applied, 2 when applied and the cable came out during
// the recorded frame: the caller takes the driver out (mgba_rs_core_replay_detach) once the
// frame has run.
extern "C" std::uint32_t mgba_rs_core_replay_queue(MGBACoreRaw *core, const std::uint8_t *data, std::size_t size) {
    // Parse first, apply after: a bad log changes nothing.
    std::vector<MGBAReplayEntry> sync;
    std::vector<MGBAReplayAsync> async_entries;
    bool attach = false;
    bool detach = false;
    LogReader reader { data, size };
    while(reader.left > 0 && reader.ok) {
        std::uint8_t tag = reader.u8();
        MGBAReplayEntry entry {};
        entry.tag = tag;
        switch(tag) {
            case LINK_LOG_CONFIG: {
                std::uint8_t devices = reader.u8();
                std::uint8_t id = reader.u8();
                entry.data = devices | (static_cast<std::uint32_t>(id) << 8);
                sync.push_back(entry);
                break;
            }
            case LINK_LOG_SET_MODE:
                entry.siocnt = reader.u16();
                entry.rcnt = reader.u16();
                sync.push_back(entry);
                break;
            case LINK_LOG_START:
                entry.ok = reader.u8();
                sync.push_back(entry);
                break;
            case LINK_LOG_MULTI:
                for(int i = 0; i < 4; i++) {
                    entry.multi[i] = reader.u16();
                }
                sync.push_back(entry);
                break;
            case LINK_LOG_NORMAL8:
                entry.data = reader.u8();
                sync.push_back(entry);
                break;
            case LINK_LOG_NORMAL32:
                entry.data = reader.u32();
                sync.push_back(entry);
                break;
            case LINK_LOG_ASYNC: {
                MGBAReplayAsync a {};
                a.at = static_cast<std::int32_t>(reader.u32());
                a.siocnt = reader.u16();
                a.rcnt = reader.u16();
                a.finish = static_cast<std::int32_t>(reader.u32());
                async_entries.push_back(a);
                break;
            }
            case LINK_LOG_ATTACH:
                attach = true;
                break;
            case LINK_LOG_DETACH:
                detach = true;
                break;
            default:
                reader.ok = false;
                break;
        }
    }
    if(!reader.ok) {
        return 0;
    }

    if(core->link != nullptr && core->link->attached) {
        // A live cable is in: a recording cannot drive this game.
        return 0;
    }
    if(!replay_attach(core)) {
        return 0;
    }
    auto *replay = core->replay;
    (void) attach;
    replay->frame_start = mTimingCurrentTime(core->core->timing);
    // Sync entries not consumed by now were asked for in an order the game no longer follows;
    // they would only shadow this frame's. Configuration entries are state rather than
    // answers: whatever is left of them still applies.
    for(; replay->sync_head < replay->sync.size(); replay->sync_head++) {
        const auto &entry = replay->sync[replay->sync_head];
        if(entry.tag == LINK_LOG_CONFIG) {
            replay->devices = entry.data & 0xFF;
            replay->id = (entry.data >> 8) & 0xFF;
        }
        else {
            replay->misses++;
        }
    }
    replay->sync = std::move(sync);
    replay->sync_head = 0;
    replay->async_entries = std::move(async_entries);
    replay->async_head = 0;
    replay_schedule_next_async(replay);
    return detach ? 2 : 1;
}

// Detach whatever is in and free both drivers (at core teardown).
static void link_free(MGBACoreRaw *core) {
    mgba_rs_core_link_detach(core);
    mgba_rs_core_replay_detach(core);
    delete core->link;
    core->link = nullptr;
    delete core->replay;
    core->replay = nullptr;
}
