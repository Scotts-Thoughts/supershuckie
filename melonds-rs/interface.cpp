#define JIT_ENABLED 1

#include "melonDS/src/NDS.h"
#include "melonDS/src/Platform.h"
#include <cstdint>
#include <cstdlib>
#include <memory>
#include <semaphore>
#include <thread>
#ifdef _WIN32
#ifndef _WIN32_WINNT
#define _WIN32_WINNT 0x0A00
#endif
#ifndef WIN32_LEAN_AND_MEAN
#define WIN32_LEAN_AND_MEAN
#endif
#ifndef NOMINMAX
#define NOMINMAX
#endif
#include <windows.h>
#endif

using namespace melonDS;

static u64 ms;

// A replay keyframe may restore the 3D engine's polygon and vertex RAM from an earlier keyframe
// (the recorder's transient-buffer masks, see supershuckie-replay-recorder/src/keyframe_masks.rs)
// while the render list and the polygons already submitted for the next frame are this state's
// own. Loading one with melonds_rs_core_load_save_state_discarding_geometry draws nothing from
// the restored polygons (GPU3D::DiscardGeometryOnLoad) and follows, frame by frame, whether the
// picture still lacks geometry because of it: until the game has flushed a frame's worth of
// polygons all submitted after the load, and the picture rendered from them is on screen.
struct DiscardedGeometry {
    bool render_list = false; // the render list holds discarded polygons
    bool pending = false;     // the polygons submitted so far for the next flush include discarded ones
    bool rendered = false;    // the 3D picture last rendered came from a list holding discarded polygons
    bool shown = false;       // the frame last run showed such a picture
    u32 bank = 0;             // GPU3D::CurRAMBank, which flips at every flush

    bool active() const { return render_list || pending || rendered; }
};

struct MelonDSCoreHolder {
    std::unique_ptr<NDS> nds;
    DiscardedGeometry discarded;
};

// Advance `discarded` past the frame RunFrame just emulated. The frame showed the 3D picture
// rendered during the frame before it (at VCount 215; right after the load for the first frame);
// this frame's flush, if any, happened at VBlank, before this frame's own render.
static void track_discarded_geometry(MelonDSCoreHolder *core) {
    DiscardedGeometry &d = core->discarded;
    if (!d.active()) {
        d.shown = false;
        return;
    }

    const GPU3D &gpu3d = core->nds->GPU.GPU3D;
    const u32 dispcnt = core->nds->GPU.GPU2D_A.DispCnt;
    const bool shows_3d = (dispcnt & (1 << 3)) && (dispcnt & (1 << 8)); // BG0 enabled, as 3D
    d.shown = d.rendered && shows_3d;

    if (gpu3d.CurRAMBank != d.bank) {
        d.bank = gpu3d.CurRAMBank;
        // With rendering off the flush swaps banks without building a new render list.
        if (gpu3d.RenderingEnabled) {
            d.render_list = d.pending;
        }
        d.pending = false;
    }
    d.rendered = d.render_list;
}

// Error codes handed back through `error_out` by melonds_rs_core_new on failure.
enum MelonDSCoreNewError : std::uint32_t {
    MELONDS_CORE_NEW_ERROR_BAD_ROM = 1,
    MELONDS_CORE_NEW_ERROR_EXCEPTION = 2,
};

extern "C" MelonDSCoreHolder *melonds_rs_core_new(
    const u8 *rom,
    std::size_t rom_size,
    const u8 *sram,
    std::size_t sram_size,
    bool jit,
    std::uint32_t *error_out
) {
    // NDS construction, ROM parsing and save loading can all throw (e.g. std::bad_alloc, or
    // melonDS's own parsing exceptions), which is UB unwinding across this extern "C" boundary if
    // left uncaught.
    MelonDSCoreHolder *holder = nullptr;
    try {
        NDSCart::NDSCartArgs cartargs;

        auto file_data = std::make_unique<u8[]>(rom_size);
        std::memcpy(file_data.get(), rom, rom_size);

        holder = new MelonDSCoreHolder();

        NDSArgs nds_args;
        if(!jit) {
            nds_args.JIT = std::nullopt;
        }
        // Every core hands the frontend 48 kHz stereo (supershuckie_core::emulator::AUDIO_SAMPLE_RATE);
        // melonDS resamples the SPU's 32.7 kHz mix to this with blip_buf.
        nds_args.OutputSampleRate = 48000.0;

        holder->nds = std::make_unique<NDS>(std::move(nds_args));

        static_cast<SoftRenderer &>(holder->nds->GetRenderer3D()).SetThreaded(true, holder->nds->GPU);
        auto cart = NDSCart::ParseROM(std::move(file_data), rom_size, holder, std::move(cartargs));
        if(!cart) {
            delete holder;
            if(error_out != nullptr) {
                *error_out = MELONDS_CORE_NEW_ERROR_BAD_ROM;
            }
            return nullptr;
        }

        holder->nds->SetNDSCart(std::move(cart));

        if(sram_size > 0) {
            holder->nds->SetNDSSave(sram, sram_size);
        }

        holder->nds->SetARM7BIOS(bios_arm7_bin);
        holder->nds->SetARM9BIOS(bios_arm9_bin);
        holder->nds->LoadBIOS();
        holder->nds->SetupDirectBoot("nds.rom");
        holder->nds->Start();

        return holder;
    } catch(...) {
        delete holder;
        if(error_out != nullptr) {
            *error_out = MELONDS_CORE_NEW_ERROR_EXCEPTION;
        }
        return nullptr;
    }
}

extern "C" void melonds_rs_core_free(MelonDSCoreHolder *core) {
    delete core;
}

extern "C" void melonds_rs_core_run_frame(MelonDSCoreHolder *core) {
    core->nds->RunFrame();
    track_discarded_geometry(core);
}

// Whether the frame last run showed a 3D picture missing geometry that a
// melonds_rs_core_load_save_state_discarding_geometry load discarded.
extern "C" bool melonds_rs_core_shows_discarded_geometry(const MelonDSCoreHolder *core) {
    return core->discarded.shown;
}

// Presentation hint: when set, the 2D renderer does not composite frames (see GPU::SkipDrawing).
// Emulation, timing and save states are unaffected; the framebuffer simply is not updated.
extern "C" void melonds_rs_core_set_skip_drawing(MelonDSCoreHolder *core, bool skip) {
    core->nds->GPU.SkipDrawing = skip;
}

// Pop up to `max_frames` stereo frames the SPU has mixed since the last read. The output ring is
// not part of the save state and overwrites itself when nobody reads it, so this never affects
// emulation.
extern "C" std::size_t melonds_rs_core_read_audio(MelonDSCoreHolder *core, std::int16_t *out, std::size_t max_frames) {
    int read = core->nds->SPU.ReadOutput(reinterpret_cast<s16 *>(out), static_cast<int>(max_frames));
    return read < 0 ? 0 : static_cast<std::size_t>(read);
}

// Forget whatever the SPU has mixed so far.
extern "C" void melonds_rs_core_drain_audio(MelonDSCoreHolder *core) {
    core->nds->SPU.DrainOutput();
}

extern "C" u8 *melonds_rs_core_get_sram(const MelonDSCoreHolder *core, size_t &size) {
    size = core->nds->GetNDSSaveLength();
    return core->nds->GetNDSSave();
}

extern "C" std::size_t melonds_rs_core_create_save_state(MelonDSCoreHolder *core, void *data, std::size_t data_size) {
    Savestate state(data, data_size, true);
    bool success = core->nds->DoSavestate(&state);
    state.Finish();

    if(!success) {
        return 0;
    }

    return state.Length();
}

extern "C" bool melonds_rs_core_load_save_state(MelonDSCoreHolder *core, void *data, std::size_t data_size) {
    Savestate state(data, data_size, false);
    core->discarded = {};
    return core->nds->DoSavestate(&state);
}

// Load a state whose polygon and vertex RAM may be a stale copy (see DiscardedGeometry): nothing
// is drawn from the polygons it restores. The flag is consulted inside DoSavestate, before the
// render thread is restarted on the loaded render list.
extern "C" bool melonds_rs_core_load_save_state_discarding_geometry(MelonDSCoreHolder *core, void *data, std::size_t data_size) {
    Savestate state(data, data_size, false);
    GPU3D &gpu3d = core->nds->GPU.GPU3D;
    gpu3d.DiscardGeometryOnLoad = true;
    bool loaded = core->nds->DoSavestate(&state);
    gpu3d.DiscardGeometryOnLoad = false;

    DiscardedGeometry &d = core->discarded;
    d = {};
    if (loaded) {
        d.render_list = gpu3d.RenderNumPolygons > 0;
        d.pending = gpu3d.NumPolygons > 0;
        // The load itself renders the (discarded) list for the first frame to show.
        d.rendered = d.render_list;
        d.bank = gpu3d.CurRAMBank;
    }
    return loaded;
}

extern "C" void melonds_rs_core_reset(MelonDSCoreHolder *core) {
    core->discarded = {};
    core->nds->Reset();
    core->nds->LoadBIOS();
    core->nds->SetupDirectBoot("nds.rom");
    core->nds->Start();
}

extern "C" u32 *melonds_rs_core_get_pixels(MelonDSCoreHolder *core, std::size_t screen) {
    // TODO: determine if and when this is necessary!
    // static_cast<SoftRenderer &>(core->nds->GetRenderer3D()).StopRenderThread();
    auto &gpu = core->nds->GPU;
    return gpu.Framebuffer[gpu.FrontBuffer][screen].get();
}

extern "C" void melonds_rs_core_set_input(MelonDSCoreHolder *core, std::uint32_t input) {
    std::uint32_t mask = 0b0000111111111111;

    auto buttons = input & mask;
    core->nds->SetKeyMask(~buttons & 0xFFF);

    if(input & 0x1000) {
        std::uint16_t y = (input >> 24) & 0xFF;
        std::uint16_t x = (input >> 16) & 0xFF;
        core->nds->TouchScreen(x, y);
    }
    else {
        core->nds->ReleaseScreen();
    }
}

extern "C" u8 *melonds_rs_core_get_ram(MelonDSCoreHolder *core) {
    return core->nds->MainRAM;
}

extern "C" u8 *melonds_rs_core_get_shared_wram(MelonDSCoreHolder *core) {
    return core->nds->SharedWRAM;
}

extern "C" u8 *melonds_rs_core_get_arm7_wram(MelonDSCoreHolder *core) {
    return core->nds->ARM7WRAM;
}

// Throw away JIT blocks compiled from memory that was just written behind the emulated CPUs'
// backs, the way melonDS's own bus writes do. `region`: 0 main RAM, 1 shared WRAM (raw offset,
// checked against both CPUs' current mappings), 2 ARM7 WRAM. The JIT tracks code in 16-byte
// cells, so one check per cell covers the range.
extern "C" void melonds_rs_core_invalidate_jit(MelonDSCoreHolder *core, std::uint32_t region, std::uint32_t offset, std::size_t length) {
    auto &nds = *core->nds;
    if(!nds.IsJITEnabled() || length == 0) {
        return;
    }

    const std::uint64_t end = static_cast<std::uint64_t>(offset) + length;
    for(std::uint64_t cell = offset & ~static_cast<std::uint64_t>(15); cell < end; cell += 16) {
        const auto o = static_cast<std::uint32_t>(cell);
        switch(region) {
            case 0:
                nds.JIT.CheckAndInvalidate<0, ARMJIT_Memory::memregion_MainRAM>(0x02000000 + o);
                break;
            case 1:
                if(nds.SWRAM_ARM9.Mem != nullptr) {
                    const auto base = static_cast<std::uint32_t>(nds.SWRAM_ARM9.Mem - nds.SharedWRAM);
                    if(o >= base && o - base <= nds.SWRAM_ARM9.Mask) {
                        nds.JIT.CheckAndInvalidate<0, ARMJIT_Memory::memregion_SharedWRAM>(0x03000000 + (o - base));
                    }
                }
                if(nds.SWRAM_ARM7.Mem != nullptr) {
                    const auto base = static_cast<std::uint32_t>(nds.SWRAM_ARM7.Mem - nds.SharedWRAM);
                    if(o >= base && o - base <= nds.SWRAM_ARM7.Mask) {
                        nds.JIT.CheckAndInvalidate<1, ARMJIT_Memory::memregion_SharedWRAM>(0x03000000 + (o - base));
                    }
                }
                break;
            case 2:
                nds.JIT.CheckAndInvalidate<1, ARMJIT_Memory::memregion_WRAM7>(0x03800000 + o);
                break;
        }
    }
}

extern "C" void melonds_rs_core_set_date(
    MelonDSCoreHolder *core,
    u16 year,
    u8 month,
    u8 day,
    u8 hour,
    u8 minute,
    u8 second
) {
    core->nds->RTC.SetDateTime(year % 100, month, day, hour, minute, second);
}

namespace melonDS::Platform {
    void SignalStop(StopReason reason, void* userdata) {}
    std::string GetLocalFilePath(const std::string& filename) { return std::string(); }
    FileHandle* OpenFile(const std::string& path, FileMode mode) { return nullptr; }
    FileHandle* OpenLocalFile(const std::string& path, FileMode mode) { return nullptr; }
    bool FileExists(const std::string& name) { return false; }
    bool LocalFileExists(const std::string& name) { return false; }
    bool CheckFileWritable(const std::string& filepath) { return false; }
    bool CheckLocalFileWritable(const std::string& filepath) { return false; }
    bool CloseFile(FileHandle* file) { return false; }
    bool IsEndOfFile(FileHandle* file) { return false; }
    bool FileReadLine(char* str, int count, FileHandle* file) { return false; }
    u64 FilePosition(FileHandle* file) { return 0; }
    bool FileSeek(FileHandle* file, s64 offset, FileSeekOrigin origin) { return false; }
    void FileRewind(FileHandle* file) {}
    u64 FileRead(void* data, u64 size, u64 count, FileHandle* file) { return 0; }
    bool FileFlush(FileHandle* file) { return false; }
    u64 FileWrite(const void* data, u64 size, u64 count, FileHandle* file) { return 0; }
    u64 FileWriteFormatted(FileHandle* file, const char* fmt, ...) { return 0; }
    u64 FileLength(FileHandle* file) { return 0; }
    void Log(LogLevel level, const char* fmt, ...) {
//        va_list args;
//        va_start(args, fmt);
//        std::printf("LOG (%d) ", level);
//        std::vprintf(fmt, args);
//        va_end(args);
    }

    struct Thread {
        std::thread thread;
        bool done = false;
    };

    // melonDS creates exactly one thread through this: the software 3D rasteriser, which the
    // main emulation thread waits on every scanline. Ask the OS to schedule it like the thread
    // it serves (no efficiency-core / EcoQoS placement, slightly elevated priority).
    static void mark_thread_latency_sensitive() {
#ifdef _WIN32
        THREAD_POWER_THROTTLING_STATE state{};
        state.Version = THREAD_POWER_THROTTLING_CURRENT_VERSION;
        state.ControlMask = THREAD_POWER_THROTTLING_EXECUTION_SPEED;
        state.StateMask = 0;
        SetThreadInformation(GetCurrentThread(), ThreadPowerThrottling, &state, sizeof(state));
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL);
#endif
    }

    Thread* Thread_Create(std::function<void()> func) {
        Thread *thread = new Thread();
        thread->thread = std::thread([func = std::move(func)]() {
            mark_thread_latency_sensitive();
            func();
        });
        return thread;
    }
    void Thread_Free(Thread* thread) { delete thread; }
    void Thread_Wait(Thread* thread) {
        if(!thread->done) {
            thread->done = true;
            thread->thread.join();
        }
    }

    struct Semaphore {
        std::counting_semaphore<256> semaphore;
        Semaphore(): semaphore(0) {}
    };

    Semaphore* Semaphore_Create() {
        return new Semaphore();
    }
    void Semaphore_Free(Semaphore* sema) {
        delete sema;
    }
    void Semaphore_Reset(Semaphore* sema) {
        while(Semaphore_TryWait(sema, 0)) {}
    }
    void Semaphore_Wait(Semaphore* sema) {
        sema->semaphore.acquire();
    }
    bool Semaphore_TryWait(Semaphore* sema, int timeout_ms) {
        if(timeout_ms == 0) {
            return sema->semaphore.try_acquire();
        }
        return sema->semaphore.try_acquire_for(std::chrono::milliseconds(timeout_ms));
    }
    void Semaphore_Post(Semaphore* sema, int count) {
        sema->semaphore.release(count);
    }

    struct Mutex {
        std::mutex mutex;
    };

    Mutex* Mutex_Create() { return new Mutex(); }
    void Mutex_Free(Mutex* mutex) { delete mutex; }
    void Mutex_Lock(Mutex* mutex) {
        mutex->mutex.lock();
    }
    void Mutex_Unlock(Mutex* mutex) {
        mutex->mutex.unlock();
    }
    bool Mutex_TryLock(Mutex* mutex) {
        return mutex->mutex.try_lock();
    }

    void Sleep(u64 usecs) { }
    u64 GetMSCount() { return 0; }
    u64 GetUSCount() { return 0; }


    void WriteNDSSave(const u8* savedata, u32 savelen, u32 writeoffset, u32 writelen, void* userdata) {}
    void WriteGBASave(const u8* savedata, u32 savelen, u32 writeoffset, u32 writelen, void* userdata) {}
    void WriteFirmware(const Firmware& firmware, u32 writeoffset, u32 writelen, void* userdata) {}
    void WriteDateTime(int year, int month, int day, int hour, int minute, int second, void* userdata) {}
    void MP_Begin(void* userdata) {}
    void MP_End(void* userdata) {}
    int MP_SendPacket(u8* data, int len, u64 timestamp, void* userdata) { return 0; }
    int MP_RecvPacket(u8* data, u64* timestamp, void* userdata) { return 0; }
    int MP_SendCmd(u8* data, int len, u64 timestamp, void* userdata) { return 0; }
    int MP_SendReply(u8* data, int len, u64 timestamp, u16 aid, void* userdata) { return 0; }
    int MP_SendAck(u8* data, int len, u64 timestamp, void* userdata) { return 0; }
    int MP_RecvHostPacket(u8* data, u64* timestamp, void* userdata) { return 0; }
    u16 MP_RecvReplies(u8* data, u64 timestamp, u16 aidmask, void* userdata) { return 0; }
    int Net_SendPacket(u8* data, int len, void* userdata) { return 0; }
    int Net_RecvPacket(u8* data, void* userdata) { return 0; }
    void Camera_Start(int num, void* userdata) {}
    void Camera_Stop(int num, void* userdata) {}
    void Camera_CaptureFrame(int num, u32* frame, int width, int height, bool yuv, void* userdata) {}
    void Mic_Start(void* userdata) {}
    void Mic_Stop(void* userdata) {}
    int Mic_ReadInput(s16* data, int maxlength, void* userdata) { return 0; }
    struct AACDecoder {};
    AACDecoder* AAC_Init() { return new AACDecoder(); }
    void AAC_DeInit(AACDecoder* dec) { delete dec; }
    bool AAC_Configure(AACDecoder* dec, int frequency, int channels) { return true; }
    bool AAC_DecodeFrame(AACDecoder* dec, const void* input, int inputlen, void* output, int outputlen) { return true; }
    bool Addon_KeyDown(KeyType type, void* userdata) { return true; }
    void Addon_RumbleStart(u32 len, void* userdata) {}
    void Addon_RumbleStop(void* userdata) {}
    float Addon_MotionQuery(MotionQueryType type, void* userdata) { return 0.0; }
}
