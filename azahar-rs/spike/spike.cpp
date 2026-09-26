// Headless Azahar (Nintendo 3DS) feasibility spike for Super Shuckie.
//
// Drives Azahar's core the way Super Shuckie's core thread would drive a core: no window, one
// emulated frame per call, scripted input from a device factory, pixels copied out of the
// renderer, in-memory save states, work-RAM hashes for determinism checks. Reports the numbers
// that decide whether a 3DS core is workable: frames per second, save-state size and save/load
// time, and whether the emulation is reproducible (same hashes in two processes, and across a
// save/load round trip).
//
//   azahar_spike <rom> [options]
//     --frames <n>          frames to time (default 1800)
//     --warmup <n>          frames to run before timing (default 300)
//     --renderer <r>        software (default) | opengl (hidden window, GL 4.3 core context)
//     --old3ds              emulate an Old 3DS (128 MB FCRAM) instead of a New 3DS (256 MB)
//     --nojit               interpreter instead of the dynarmic JIT
//     --script              feed a deterministic scripted input pattern (default: no input)
//     --mash                press A twice a second (and Start/Down now and then) to get through
//                           title screens and dialogue into gameplay
//     --dump-every <n>      with --dump: also write <prefix>-<frame>-top/bottom.bmp every n frames
//     --hash-every <n>      print an FCRAM hash every n frames (0 = off; default 600)
//     --state-every <n>     take an in-memory save state every n frames and time it (0 = off)
//     --delta-every <n>     keyframe-delta study: after the timed run, take a raw state every n
//                           frames (--delta-count samples) and report how many bytes changed
//                           since the previous one and how the changed regions compress
//     --delta-count <m>     samples for --delta-every (default 10)
//     --skip-drawing        set Azahar's skip-drawing switch (patched in): draw batches are
//                           consumed but not drawn, as a replay seek would run
//     --mask-test <k>       stale-region test: take state S0, run 120 frames to S1, then compare
//                           k frames after loading S1 against k frames after loading S1 with its
//                           linear heap (and, second variant, all non-heap FCRAM) copied from S0:
//                           heap hashes and pixel differences. Says whether those regions can be
//                           left out of keyframe deltas (as the DS transient masks do).
//     --raw-bench <n>       after the timed run: n times, serialise the state with no
//                           compression (SaveStateRaw) and load it back (LoadStateRaw), timing
//                           each; the split of Azahar's save/load cost between zstd and
//                           serialisation
//     --dirty-bench <n>     after the timed run: n times, run --dirty-frames frames, then take a raw
//                           state into a buffer still holding the previous one (only the RAM pages
//                           written since are copied) and one into a buffer holding nothing, and
//                           compare them byte for byte: the incremental copy's cost and correctness
//     --dirty-frames <f>    frames between --dirty-bench states (default 480, a keyframe interval)
//     --roundtrip <m>       at the end: save, run m frames, hash; load, run m frames, hash; compare
//     --save-file <path>    write the final save state to a file
//     --load-file <path>    load a save state file before the timed run
//     --dump <prefix>       write <prefix>-top.bmp / <prefix>-bottom.bmp of the last frame
//     --user-dir <dir>      Azahar user directory (default: ./azahar-spike-user/)
//     --log <level>         Trace|Debug|Info|Warning|Error|Critical (default Error)
//
// Output is `key=value` lines so a script can collect them.

#include <algorithm>
#include <array>
#include <chrono>
#include <cmath>
#include <cstdio>
#include <cstring>
#include <fstream>
#include <memory>
#include <optional>
#include <string>
#include <vector>

#include "audio_core/input_details.h"
#include "audio_core/sink_details.h"
#include "common/file_util.h"
#include "common/logging/backend.h"
#include "common/logging/filter.h"
#include "common/logging/log.h"
#include "common/param_package.h"
#include "common/settings.h"
#include "common/zstd_compression.h"
#include "core/core.h"
#include "core/frontend/applets/default_applets.h"
#include "core/frontend/emu_window.h"
#include "core/frontend/image_interface.h"
#include "core/frontend/input.h"
#include "core/hle/kernel/kernel.h"
#include "core/hle/kernel/process.h"
#include "core/hle/kernel/vm_manager.h"
#include "core/hle/service/service.h"
#include "core/memory.h"
#include "video_core/gpu.h"
#include "video_core/renderer_base.h"
#include "video_core/renderer_software/renderer_software.h"
#include "xxhash.h"
#include "zstd.h"

#ifdef SPIKE_OPENGL
#ifdef _WIN32
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#endif
#include "glad/glad.h"
#include "video_core/renderer_opengl/gl_state.h"
#endif

namespace {

using Clock = std::chrono::steady_clock;

double ms_since(Clock::time_point t) {
    return std::chrono::duration<double, std::milli>(Clock::now() - t).count();
}

// ---------------------------------------------------------------- scripted input

struct InputState {
    std::array<bool, Settings::NativeButton::NumButtons> buttons{};
    float circle_x = 0.0f;
    float circle_y = 0.0f;
};

InputState g_input;

class SpikeButton final : public Input::ButtonDevice {
public:
    explicit SpikeButton(int index) : index(index) {}
    bool GetStatus() const override {
        return index >= 0 && index < static_cast<int>(g_input.buttons.size()) &&
               g_input.buttons[index];
    }

private:
    int index;
};

class SpikeButtonFactory final : public Input::Factory<Input::ButtonDevice> {
public:
    std::unique_ptr<Input::ButtonDevice> Create(const Common::ParamPackage& params) override {
        return std::make_unique<SpikeButton>(params.Get("button", 0));
    }
};

class SpikeAxis final : public Input::AnalogDevice {
public:
    explicit SpikeAxis(int axis) : axis(axis) {}
    std::tuple<float, float> GetStatus() const override {
        if (axis == 0) {
            return {g_input.circle_x, g_input.circle_y};
        }
        return {0.0f, 0.0f};
    }

private:
    int axis;
};

class SpikeAxisFactory final : public Input::Factory<Input::AnalogDevice> {
public:
    std::unique_ptr<Input::AnalogDevice> Create(const Common::ParamPackage& params) override {
        return std::make_unique<SpikeAxis>(params.Get("axis", 0));
    }
};

// A fixed pattern that exercises buttons and the circle pad, so two runs with --script have
// identical input and the hashes must match if the emulation is deterministic.
void script_input(u64 frame) {
    using namespace Settings::NativeButton;
    g_input = InputState{};
    const u64 t = frame % 600;
    g_input.buttons[A] = t < 6;
    g_input.buttons[B] = t >= 150 && t < 156;
    g_input.buttons[Down] = t >= 200 && t < 230;
    g_input.buttons[Up] = t >= 300 && t < 330;
    g_input.buttons[Start] = t >= 450 && t < 456;
    g_input.buttons[Right] = t >= 500 && t < 520;
    if (t >= 360 && t < 440) {
        const float a = static_cast<float>(t - 360) / 80.0f * 6.2831853f;
        g_input.circle_x = std::cos(a);
        g_input.circle_y = std::sin(a);
    }
}

// Get through title screens and dialogue: a short A press twice a second, Start now and then,
// and a Down/A pair once in a while to leave a menu entry that A alone would not.
void mash_input(u64 frame) {
    using namespace Settings::NativeButton;
    g_input = InputState{};
    const u64 t = frame % 30;
    g_input.buttons[A] = t < 4;
    g_input.buttons[Start] = (frame % 600) >= 300 && (frame % 600) < 304;
    // Walk about: hold one direction for two seconds, cycling through four, so the overworld
    // scrolls, encounters happen and memory churns the way it does in play.
    const u64 leg = (frame / 120) % 8;
    const bool walking = (frame % 120) < 100;
    g_input.buttons[Down] = walking && leg == 1;
    g_input.buttons[Right] = walking && leg == 3;
    g_input.buttons[Up] = walking && leg == 5;
    g_input.buttons[Left] = walking && leg == 7;
}

// ---------------------------------------------------------------- headless window

constexpr unsigned LAYOUT_WIDTH = 400;
constexpr unsigned LAYOUT_HEIGHT = 480; // top 400x240 above bottom 320x240 (default layout)

struct Screen {
    unsigned width = 0;
    unsigned height = 0;
    std::vector<u8> bgra; // row-major, 4 bytes per pixel
};

class HeadlessWindow final : public Frontend::EmuWindow {
public:
    explicit HeadlessWindow(bool opengl) : opengl(opengl) {
        strict_context_required = true; // render on the emulation thread, no present thread
        window_info.type = Frontend::WindowSystemType::Headless;
        UpdateCurrentFramebufferLayout(LAYOUT_WIDTH, LAYOUT_HEIGHT);
    }

    ~HeadlessWindow() override {
#ifdef SPIKE_OPENGL
#ifdef _WIN32
        if (hglrc) {
            wglMakeCurrent(nullptr, nullptr);
            wglDeleteContext(hglrc);
        }
        if (hdc && hwnd) {
            ReleaseDC(hwnd, hdc);
        }
        if (hwnd) {
            DestroyWindow(hwnd);
        }
#endif
#endif
    }

    // Called from RendererBase::EndFrame at every emulated VBlank, whichever renderer is in use.
    // Neither renderer calls SwapBuffers on the window outside libretro builds: the software
    // renderer fills its screen buffers for a presentation thread to read, and the OpenGL
    // renderer draws into a texture mailbox that a presenter drains with TryPresent. So the
    // frame is counted here, and pixels are pulled out separately.
    void PollEvents() override {
        submitted = true;
        if (!opengl && capture_pixels) {
            CaptureSoftware();
        }
    }

    void SwapBuffers() override {
        submitted = true;
    }

    void MakeCurrent() override {
#ifdef SPIKE_OPENGL
#ifdef _WIN32
        if (hglrc) {
            wglMakeCurrent(hdc, hglrc);
        }
#endif
#endif
    }

    void DoneCurrent() override {
#ifdef SPIKE_OPENGL
#ifdef _WIN32
        if (hglrc) {
            wglMakeCurrent(nullptr, nullptr);
        }
#endif
#endif
    }

    bool HasSubmittedFrame() {
        const bool s = submitted;
        submitted = false;
        return s;
    }

    bool capture_pixels = false;
    Screen top;
    Screen bottom;

#ifdef SPIKE_OPENGL
#ifdef _WIN32
    // A hidden Win32 window with a GL 4.3 core context, created before System::Load so the
    // renderer's constructor finds a current context.
    bool CreateGlContext() {
        WNDCLASSA wc{};
        wc.style = CS_OWNDC;
        wc.lpfnWndProc = DefWindowProcA;
        wc.hInstance = GetModuleHandleA(nullptr);
        wc.lpszClassName = "AzaharSpikeGL";
        RegisterClassA(&wc);
        hwnd = CreateWindowExA(0, wc.lpszClassName, "azahar spike", WS_OVERLAPPEDWINDOW, 0, 0,
                               LAYOUT_WIDTH, LAYOUT_HEIGHT, nullptr, nullptr, wc.hInstance,
                               nullptr);
        if (!hwnd) {
            return false;
        }
        hdc = GetDC(hwnd);
        PIXELFORMATDESCRIPTOR pfd{};
        pfd.nSize = sizeof(pfd);
        pfd.nVersion = 1;
        pfd.dwFlags = PFD_DRAW_TO_WINDOW | PFD_SUPPORT_OPENGL | PFD_DOUBLEBUFFER;
        pfd.iPixelType = PFD_TYPE_RGBA;
        pfd.cColorBits = 32;
        pfd.cDepthBits = 24;
        pfd.cStencilBits = 8;
        const int format = ChoosePixelFormat(hdc, &pfd);
        if (format == 0 || !SetPixelFormat(hdc, format, &pfd)) {
            return false;
        }
        HGLRC legacy = wglCreateContext(hdc);
        if (!legacy || !wglMakeCurrent(hdc, legacy)) {
            return false;
        }
        using CreateContextAttribs = HGLRC(WINAPI*)(HDC, HGLRC, const int*);
        auto create = reinterpret_cast<CreateContextAttribs>(
            wglGetProcAddress("wglCreateContextAttribsARB"));
        if (!create) {
            return false;
        }
        const int attribs[] = {
            0x2091, 4, // WGL_CONTEXT_MAJOR_VERSION_ARB
            0x2092, 3, // WGL_CONTEXT_MINOR_VERSION_ARB
            0x9126, 1, // WGL_CONTEXT_PROFILE_MASK_ARB = core
            0,
        };
        hglrc = create(hdc, nullptr, attribs);
        wglMakeCurrent(nullptr, nullptr);
        wglDeleteContext(legacy);
        if (!hglrc || !wglMakeCurrent(hdc, hglrc)) {
            return false;
        }
        if (!gladLoadGL()) {
            return false;
        }
        std::printf("gl_renderer=%s\n", reinterpret_cast<const char*>(glGetString(GL_RENDERER)));
        std::printf("gl_version=%s\n", reinterpret_cast<const char*>(glGetString(GL_VERSION)));
        return true;
    }

private:
    HWND hwnd = nullptr;
    HDC hdc = nullptr;
    HGLRC hglrc = nullptr;
#endif

public:
    // What a presenter thread does in the real frontends: take the newest rendered frame from
    // the mailbox and blit it, here into an offscreen framebuffer that is then read back.
    void CaptureOpenGL() {
        auto& renderer = Core::System::GetInstance().GPU().Renderer();
        const auto prev_state = OpenGL::OpenGLState::GetCurState();
        if (!capture_fbo) {
            glGenFramebuffers(1, &capture_fbo);
            glGenRenderbuffers(1, &capture_rbo);
            glBindRenderbuffer(GL_RENDERBUFFER, capture_rbo);
            glRenderbufferStorage(GL_RENDERBUFFER, GL_RGBA8, LAYOUT_WIDTH, LAYOUT_HEIGHT);
            glBindFramebuffer(GL_FRAMEBUFFER, capture_fbo);
            glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_RENDERBUFFER, capture_rbo);
        }
        glBindFramebuffer(GL_DRAW_FRAMEBUFFER, capture_fbo);
        renderer.TryPresent(100);
        std::vector<u8> rgba(static_cast<size_t>(LAYOUT_WIDTH) * LAYOUT_HEIGHT * 4);
        glBindFramebuffer(GL_READ_FRAMEBUFFER, capture_fbo);
        glReadBuffer(GL_COLOR_ATTACHMENT0);
        glPixelStorei(GL_PACK_ALIGNMENT, 1);
        glReadPixels(0, 0, LAYOUT_WIDTH, LAYOUT_HEIGHT, GL_RGBA, GL_UNSIGNED_BYTE, rgba.data());
        // The rasterizer tracks GL bindings in OpenGLState; force it to rebind everything.
        OpenGL::OpenGLState{}.Apply();
        prev_state.Apply();
        const auto& layout = GetFramebufferLayout();
        auto crop = [&](Screen& out, const Common::Rectangle<u32>& rect) {
            out.width = rect.GetWidth();
            out.height = rect.GetHeight();
            out.bgra.assign(static_cast<size_t>(out.width) * out.height * 4, 0);
            for (unsigned y = 0; y < out.height; y++) {
                // GL rows run bottom-up.
                const unsigned gy = LAYOUT_HEIGHT - 1 - (rect.top + y);
                for (unsigned x = 0; x < out.width; x++) {
                    const u8* src = rgba.data() + (static_cast<size_t>(gy) * LAYOUT_WIDTH + rect.left + x) * 4;
                    u8* dst = out.bgra.data() + (static_cast<size_t>(y) * out.width + x) * 4;
                    dst[0] = src[2];
                    dst[1] = src[1];
                    dst[2] = src[0];
                    dst[3] = 255;
                }
            }
        };
        crop(top, layout.top_screen);
        crop(bottom, layout.bottom_screen);
    }

private:
    GLuint capture_fbo = 0;
    GLuint capture_rbo = 0;
#endif

private:
    void CaptureSoftware() {
        auto& system = Core::System::GetInstance();
        const auto& renderer =
            static_cast<const SwRenderer::RendererSoftware&>(system.GPU().Renderer());
        auto copy = [](Screen& out, const SwRenderer::ScreenInfo& info) {
            // ScreenInfo holds the portrait framebuffer column-major: pixel (x along the 3DS's
            // 240-pixel axis, y along its 400/320-pixel axis) at (x * height + y). Viewed in
            // landscape that is `width` rows of `height` pixels, RGBA.
            if (info.pixels.empty()) {
                return;
            }
            out.width = info.height;
            out.height = info.width;
            out.bgra.resize(static_cast<size_t>(out.width) * out.height * 4);
            for (unsigned row = 0; row < out.height; row++) {
                for (unsigned col = 0; col < out.width; col++) {
                    const u8* src = info.pixels.data() + (static_cast<size_t>(row) * info.height + col) * 4;
                    u8* dst = out.bgra.data() + (static_cast<size_t>(row) * out.width + col) * 4;
                    dst[0] = src[2];
                    dst[1] = src[1];
                    dst[2] = src[0];
                    dst[3] = 255;
                }
            }
        };
        copy(top, renderer.Screen(VideoCore::ScreenId::TopLeft));
        copy(bottom, renderer.Screen(VideoCore::ScreenId::Bottom));
    }

    bool opengl;
    bool submitted = false;
};

// ---------------------------------------------------------------- helpers

bool write_bmp(const std::string& path, const Screen& s) {
    if (s.width == 0 || s.height == 0) {
        return false;
    }
    std::ofstream f(path, std::ios::binary);
    if (!f) {
        return false;
    }
    const u32 row = s.width * 4;
    const u32 image = row * s.height;
    const u32 file_size = 54 + image;
    u8 header[54] = {'B', 'M'};
    auto put32 = [&](int at, u32 v) { std::memcpy(header + at, &v, 4); };
    auto put16 = [&](int at, u16 v) { std::memcpy(header + at, &v, 2); };
    put32(2, file_size);
    put32(10, 54);
    put32(14, 40);
    put32(18, s.width);
    put32(22, s.height);
    put16(26, 1);
    put16(28, 32);
    put32(34, image);
    f.write(reinterpret_cast<const char*>(header), sizeof(header));
    for (int y = static_cast<int>(s.height) - 1; y >= 0; y--) { // BMP rows are bottom-up
        f.write(reinterpret_cast<const char*>(s.bgra.data() + static_cast<size_t>(y) * row), row);
    }
    return true;
}

struct Options {
    std::string rom;
    u64 frames = 1800;
    u64 warmup = 300;
    bool opengl = false;
    bool old3ds = false;
    bool jit = true;
    bool script = false;
    bool mash = false;
    u64 dump_every = 0;
    u64 hash_every = 600;
    u64 state_every = 0;
    u64 delta_every = 0;
    u64 delta_count = 10;
    u64 raw_bench = 0;
    u64 dirty_bench = 0;
    u64 dirty_frames = 480;
    u64 mask_test = 0;
    bool skip_drawing = false;
    u64 roundtrip = 0;
    std::string save_file;
    std::string load_file;
    std::string dump;
    std::string user_dir = "azahar-spike-user";
    std::string log = "Error";
};

std::optional<Options> parse(int argc, char** argv) {
    Options o;
    for (int i = 1; i < argc; i++) {
        const std::string a = argv[i];
        auto next = [&]() -> const char* {
            if (i + 1 >= argc) {
                std::fprintf(stderr, "%s needs a value\n", a.c_str());
                std::exit(2);
            }
            return argv[++i];
        };
        if (a == "--frames") o.frames = std::stoull(next());
        else if (a == "--warmup") o.warmup = std::stoull(next());
        else if (a == "--renderer") o.opengl = std::string(next()) == "opengl";
        else if (a == "--old3ds") o.old3ds = true;
        else if (a == "--nojit") o.jit = false;
        else if (a == "--script") o.script = true;
        else if (a == "--mash") o.mash = true;
        else if (a == "--dump-every") o.dump_every = std::stoull(next());
        else if (a == "--hash-every") o.hash_every = std::stoull(next());
        else if (a == "--state-every") o.state_every = std::stoull(next());
        else if (a == "--delta-every") o.delta_every = std::stoull(next());
        else if (a == "--delta-count") o.delta_count = std::stoull(next());
        else if (a == "--raw-bench") o.raw_bench = std::stoull(next());
        else if (a == "--dirty-bench") o.dirty_bench = std::stoull(next());
        else if (a == "--dirty-frames") o.dirty_frames = std::stoull(next());
        else if (a == "--mask-test") o.mask_test = std::stoull(next());
        else if (a == "--skip-drawing") o.skip_drawing = true;
        else if (a == "--roundtrip") o.roundtrip = std::stoull(next());
        else if (a == "--save-file") o.save_file = next();
        else if (a == "--load-file") o.load_file = next();
        else if (a == "--dump") o.dump = next();
        else if (a == "--user-dir") o.user_dir = next();
        else if (a == "--log") o.log = next();
        else if (!a.empty() && a[0] == '-') {
            std::fprintf(stderr, "unknown option %s\n", a.c_str());
            return std::nullopt;
        } else o.rom = a;
    }
    if (o.rom.empty()) {
        return std::nullopt;
    }
    return o;
}

Common::Log::Level log_level(const std::string& s) {
    using L = Common::Log::Level;
    if (s == "Trace") return L::Trace;
    if (s == "Debug") return L::Debug;
    if (s == "Info") return L::Info;
    if (s == "Warning") return L::Warning;
    if (s == "Critical") return L::Critical;
    return L::Error;
}

const char* status_name(Core::System::ResultStatus s) {
    using R = Core::System::ResultStatus;
    switch (s) {
    case R::Success: return "Success";
    case R::ErrorNotInitialized: return "ErrorNotInitialized";
    case R::ErrorGetLoader: return "ErrorGetLoader";
    case R::ErrorSystemMode: return "ErrorSystemMode";
    case R::ErrorLoader: return "ErrorLoader";
    case R::ErrorLoader_ErrorEncrypted: return "ErrorLoader_ErrorEncrypted";
    case R::ErrorLoader_ErrorInvalidFormat: return "ErrorLoader_ErrorInvalidFormat";
    case R::ErrorLoader_ErrorGbaTitle: return "ErrorLoader_ErrorGbaTitle";
    case R::ErrorSystemFiles: return "ErrorSystemFiles";
    case R::ErrorSavestate: return "ErrorSavestate";
    case R::ErrorCoreExceptionRaised: return "ErrorCoreExceptionRaised";
    case R::ErrorSavestateBuildMismatch: return "ErrorSavestateBuildMismatch";
    case R::ShutdownRequested: return "ShutdownRequested";
    default: return "Error(other)";
    }
}

struct Runner {
    Core::System& system;
    HeadlessWindow& window;
    const Options& opts;
    u64 frame = 0;
    bool stopped = false;

    // One emulated frame: run the core until the renderer reports a VBlank.
    bool run_frame() {
        if (opts.script) {
            script_input(frame);
        } else if (opts.mash) {
            mash_input(frame);
        }
        while (!window.HasSubmittedFrame()) {
            const auto result = system.RunLoop();
            if (result != Core::System::ResultStatus::Success) {
                std::printf("run_loop_status=%s details=%s\n", status_name(result),
                            system.GetStatusDetails().c_str());
                stopped = true;
                return false;
            }
        }
        frame++;
        return true;
    }

    bool run_frames(u64 n) {
        for (u64 i = 0; i < n; i++) {
            if (!run_frame()) {
                return false;
            }
        }
        return true;
    }

    u64 fcram_size() const {
        return Settings::values.is_new_3ds.GetValue() ? Memory::FCRAM_N3DS_SIZE : Memory::FCRAM_SIZE;
    }

    u64 hash_fcram() {
        const u8* p = system.Memory().GetFCRAMPointer(0);
        return XXH3_64bits(p, fcram_size());
    }

    u64 hash_vram() {
        const u8* p = system.Memory().GetPhysicalPointer(Memory::VRAM_PADDR);
        return p ? XXH3_64bits(p, Memory::VRAM_SIZE) : 0;
    }

    // The application's own memory, split the way a Play Together sync hash would want it:
    // `heap` is the process heap (game state: what a desync check must cover), `linear` the
    // linear heap where GPU buffers live (framebuffers, textures, command lists: written back by
    // the renderer, so it may legitimately differ between renderers or drivers).
    struct RegionHashes {
        u64 heap = 0;
        u64 linear = 0;
        u64 heap_bytes = 0;
        u64 linear_bytes = 0;
    };

    RegionHashes hash_regions() {
        RegionHashes out;
        if (!system.KernelRunning()) {
            return out;
        }
        auto process = system.Kernel().GetCurrentProcess();
        if (!process) {
            return out;
        }
        XXH3_state_t* heap = XXH3_createState();
        XXH3_state_t* linear = XXH3_createState();
        XXH3_64bits_reset(heap);
        XXH3_64bits_reset(linear);
        for (const auto& [addr, vma] : process->vm_manager.vma_map) {
            if (vma.type != Kernel::VMAType::BackingMemory || vma.size == 0 || !vma.backing_memory) {
                continue;
            }
            const u8* p = vma.backing_memory.GetPtr();
            if (vma.base >= Memory::HEAP_VADDR && vma.base < Memory::HEAP_VADDR_END) {
                XXH3_64bits_update(heap, p, vma.size);
                out.heap_bytes += vma.size;
            } else if ((vma.base >= Memory::LINEAR_HEAP_VADDR && vma.base < Memory::LINEAR_HEAP_VADDR_END) ||
                       (vma.base >= Memory::NEW_LINEAR_HEAP_VADDR && vma.base < Memory::NEW_LINEAR_HEAP_VADDR_END)) {
                XXH3_64bits_update(linear, p, vma.size);
                out.linear_bytes += vma.size;
            }
        }
        out.heap = XXH3_64bits_digest(heap);
        out.linear = XXH3_64bits_digest(linear);
        XXH3_freeState(heap);
        XXH3_freeState(linear);
        return out;
    }

    // Where FCRAM sits inside a raw serialised state, and what each 4 KiB page of it is:
    // 0 = other, 1 = process heap, 2 = linear heap. Found by searching the raw state for a
    // distinctive run of FCRAM bytes (FCRAM is serialised as one contiguous block).
    struct RawLayout {
        size_t fcram_off = SIZE_MAX;
        u64 fcram_size = 0;
        std::vector<u8> page_class;
    };

    RawLayout raw_layout(const std::vector<u8>& raw) {
        RawLayout l;
        l.fcram_size = fcram_size();
        const u8* fcram = system.Memory().GetFCRAMPointer(0);
        // A needle with some entropy: the first 64-byte block (at a 4 KiB boundary) that is not
        // mostly one byte value.
        size_t needle_at = SIZE_MAX;
        for (size_t at = 0x1000; at + 64 <= l.fcram_size && needle_at == SIZE_MAX; at += 0x1000) {
            int distinct = 0;
            bool seen[256] = {};
            for (int i = 0; i < 64; i++) {
                if (!seen[fcram[at + i]]) { seen[fcram[at + i]] = true; distinct++; }
            }
            if (distinct >= 24) needle_at = at;
        }
        if (needle_at == SIZE_MAX) return l;
        auto it = std::search(raw.begin(), raw.end(), fcram + needle_at, fcram + needle_at + 64);
        if (it == raw.end()) return l;
        const size_t pos = static_cast<size_t>(it - raw.begin());
        if (pos < needle_at || pos - needle_at + l.fcram_size > raw.size()) return l;
        l.fcram_off = pos - needle_at;
        if (std::memcmp(raw.data() + l.fcram_off, fcram, std::min<size_t>(l.fcram_size, 8 << 20)) != 0) {
            l.fcram_off = SIZE_MAX;
            return l;
        }
        l.page_class.assign(l.fcram_size / 4096, 0);
        auto process = system.Kernel().GetCurrentProcess();
        for (const auto& [addr, vma] : process->vm_manager.vma_map) {
            if (vma.type != Kernel::VMAType::BackingMemory || vma.size == 0 || !vma.backing_memory) continue;
            const u8* p = vma.backing_memory.GetPtr();
            if (p < fcram || p >= fcram + l.fcram_size) continue;
            u8 cls = 0;
            if (vma.base >= Memory::HEAP_VADDR && vma.base < Memory::HEAP_VADDR_END) cls = 1;
            else if ((vma.base >= Memory::LINEAR_HEAP_VADDR && vma.base < Memory::LINEAR_HEAP_VADDR_END) ||
                     (vma.base >= Memory::NEW_LINEAR_HEAP_VADDR && vma.base < Memory::NEW_LINEAR_HEAP_VADDR_END)) cls = 2;
            else continue;
            const size_t first = static_cast<size_t>(p - fcram) / 4096;
            const size_t count = vma.size / 4096;
            for (size_t i = first; i < first + count && i < l.page_class.size(); i++) l.page_class[i] = cls;
        }
        return l;
    }

    // Class of a raw-state byte offset: 0 other-FCRAM, 1 heap, 2 linear, 3 outside FCRAM.
    static u8 classify(const RawLayout& l, size_t raw_off) {
        if (l.fcram_off == SIZE_MAX || raw_off < l.fcram_off || raw_off >= l.fcram_off + l.fcram_size) return 3;
        return l.page_class[(raw_off - l.fcram_off) / 4096];
    }

    void print_hashes(const char* tag) {
        const auto t = Clock::now();
        const u64 h = hash_fcram();
        const u64 hv = hash_vram();
        const auto r = hash_regions();
        hash_ms_total += ms_since(t);
        std::printf("%s frame=%llu heap=%016llx linear=%016llx fcram=%016llx vram=%016llx heap_bytes=%llu linear_bytes=%llu\n",
                    tag, static_cast<unsigned long long>(frame), static_cast<unsigned long long>(r.heap),
                    static_cast<unsigned long long>(r.linear), static_cast<unsigned long long>(h),
                    static_cast<unsigned long long>(hv), static_cast<unsigned long long>(r.heap_bytes),
                    static_cast<unsigned long long>(r.linear_bytes));
    }

    double hash_ms_total = 0;

    // Save states must not be taken while HLE file/network operations are in flight; run the
    // core until the kernel reports none pending (the libretro glue does the same).
    bool drain_async() {
        if (!system.KernelRunning() || !system.Kernel().AreAsyncOperationsPending()) {
            return true;
        }
        const auto start = Clock::now();
        while (system.Kernel().AreAsyncOperationsPending()) {
            if (ms_since(start) > 5000.0) {
                std::printf("drain_async=timeout\n");
                return false;
            }
            if (system.RunLoop() != Core::System::ResultStatus::Success) {
                return false;
            }
        }
        window.HasSubmittedFrame(); // discard any VBlank that passed while draining
        return true;
    }

    std::optional<std::vector<u8>> save_state(double* ms) {
        if (!drain_async()) {
            return std::nullopt;
        }
        const auto t = Clock::now();
        std::vector<u8> state;
        try {
            state = system.SaveStateBuffer();
        } catch (const std::exception& e) {
            std::printf("save_state_error=%s\n", e.what());
            return std::nullopt;
        }
        *ms = ms_since(t);
        return state;
    }

    bool load_state(const std::vector<u8>& state, double* ms) {
        const auto t = Clock::now();
        bool ok = false;
        try {
            ok = system.LoadStateBuffer(state);
        } catch (const std::exception& e) {
            std::printf("load_state_error=%s\n", e.what());
            return false;
        }
        *ms = ms_since(t);
        window.HasSubmittedFrame();
        return ok;
    }
};

std::optional<std::vector<u8>> read_file(const std::string& path) {
    std::ifstream f(path, std::ios::binary);
    if (!f) {
        return std::nullopt;
    }
    return std::vector<u8>((std::istreambuf_iterator<char>(f)), std::istreambuf_iterator<char>());
}

} // namespace

int main(int argc, char** argv) {
    const auto parsed = parse(argc, argv);
    if (!parsed) {
        std::fprintf(stderr, "usage: azahar_spike <rom> [options]  (see the top of spike.cpp)\n");
        return 2;
    }
    const Options& opts = *parsed;

    // Logging to the console only, at the requested level.
    Common::Log::Initialize();
    Common::Log::SetColorConsoleBackendEnabled(false);
    Common::Log::SetGlobalFilter(Common::Log::Filter(log_level(opts.log)));
    Common::Log::Start();

    std::string user_dir = opts.user_dir;
    if (user_dir.back() != '/' && user_dir.back() != '\\') {
        user_dir += '/';
    }
    FileUtil::CreateFullPath(user_dir);
    FileUtil::SetUserPath(user_dir);

    // Settings: everything that could differ between two machines pinned; no pacing; no audio
    // device; software or OpenGL rendering at native resolution.
    auto& v = Settings::values;
    for (const auto& module : Service::service_module_map) {
        v.lle_modules.emplace(module.name, false);
    }
    v.use_cpu_jit.SetValue(opts.jit);
    v.cpu_clock_percentage.SetValue(100);
    v.is_new_3ds.SetValue(!opts.old3ds);
    v.init_clock.SetValue(Settings::InitClock::FixedTime);
    v.init_time.SetValue(946681277ULL);
    v.init_ticks_type.SetValue(Settings::InitTicks::Fixed);
    v.init_ticks_override.SetValue(0);
    // HLE file-system requests normally complete on host threads, so the emulated program sees
    // them finish at a wall-clock-dependent emulated time; both switches make them synchronous
    // or deterministic in emulated time (the first matrix run diverged between processes
    // without these).
    v.deterministic_async_operations.SetValue(true);
    v.async_fs_operations.SetValue(false);
    v.async_presentation.SetValue(false);
    v.async_custom_loading.SetValue(false);
    v.graphics_api.SetValue(opts.opengl ? Settings::GraphicsAPI::OpenGL : Settings::GraphicsAPI::Software);
    v.use_hw_shader.SetValue(true);
    v.use_disk_shader_cache.SetValue(false);
    v.async_shader_compilation.SetValue(false);
    v.use_shader_jit.SetValue(true);
    v.resolution_factor.SetValue(1);
    v.frame_limit.SetValue(0); // 0 = no frame limiting
    v.layout_option.SetValue(Settings::LayoutOption::Default);
    v.render_3d.SetValue(Settings::StereoRenderOption::Off);
    v.audio_emulation.SetValue(Settings::AudioEmulation::HLE);
    v.enable_audio_stretching.SetValue(false);
    v.output_type.SetValue(AudioCore::SinkType::Null);
    v.input_type.SetValue(AudioCore::InputType::Static);
    v.custom_textures.SetValue(false);
    v.dump_textures.SetValue(false);

    // Input: every native button and the circle pad come from this program's factories; the
    // touch screen stays on the window's own device, motion on the null device.
    Input::RegisterFactory<Input::ButtonDevice>("spike", std::make_shared<SpikeButtonFactory>());
    Input::RegisterFactory<Input::AnalogDevice>("spike", std::make_shared<SpikeAxisFactory>());
    auto& profile = v.current_input_profile;
    for (int i = 0; i < Settings::NativeButton::NumButtons; i++) {
        profile.buttons[i] = "engine:spike,button:" + std::to_string(i);
    }
    profile.analogs[Settings::NativeAnalog::CirclePad] = "engine:spike,axis:0";
    profile.analogs[Settings::NativeAnalog::CStick] = "engine:spike,axis:1";
    profile.motion_device = "engine:null";
    profile.touch_device = "engine:emu_window";

    auto& system = Core::System::GetInstance();
    Frontend::RegisterDefaultApplets(system);
    system.RegisterImageInterface(std::make_shared<Frontend::ImageInterface>());

    VideoCore::g_skip_drawing.store(opts.skip_drawing);
    HeadlessWindow window(opts.opengl);
#ifdef SPIKE_OPENGL
#ifdef _WIN32
    if (opts.opengl && !window.CreateGlContext()) {
        std::printf("error=could not create an OpenGL 4.3 context\n");
        return 1;
    }
#endif
#else
    if (opts.opengl) {
        std::printf("error=built without OpenGL\n");
        return 1;
    }
#endif

    std::printf("rom=%s\n", opts.rom.c_str());
    std::printf("renderer=%s jit=%d new3ds=%d script=%d\n", opts.opengl ? "opengl" : "software",
                opts.jit ? 1 : 0, opts.old3ds ? 0 : 1, opts.script ? 1 : 0);

    const auto load_start = Clock::now();
    const auto load_status = system.Load(window, opts.rom);
    std::printf("load_status=%s load_ms=%.1f\n", status_name(load_status), ms_since(load_start));
    if (load_status != Core::System::ResultStatus::Success) {
        std::printf("details=%s\n", system.GetStatusDetails().c_str());
        return 1;
    }
    system.RegisterCoreLoopThreadId();

    Runner r{system, window, opts};

    if (!opts.load_file.empty()) {
        // One frame first so the kernel and GPU are fully up before a state replaces them.
        if (!r.run_frame()) return 1;
        auto data = read_file(opts.load_file);
        if (!data) {
            std::printf("error=cannot read %s\n", opts.load_file.c_str());
            return 1;
        }
        double ms = 0;
        const bool ok = r.load_state(*data, &ms);
        std::printf("load_file=%s ok=%d load_state_ms=%.1f\n", opts.load_file.c_str(), ok ? 1 : 0, ms);
        if (!ok) return 1;
    }

    // Warm-up (boot, shader compilation, JIT population).
    const auto warm_start = Clock::now();
    if (!r.run_frames(opts.warmup)) return 1;
    std::printf("warmup_frames=%llu warmup_ms=%.1f\n", static_cast<unsigned long long>(opts.warmup),
                ms_since(warm_start));

    // Timed run.
    double save_ms_total = 0;
    u64 saves = 0;
    size_t last_state_size = 0;
    size_t last_state_raw = 0;
    const auto run_start = Clock::now();
    for (u64 i = 1; i <= opts.frames; i++) {
        if (!r.run_frame()) break;
        if (opts.hash_every && i % opts.hash_every == 0) {
            r.print_hashes("hash");
        }
        if (opts.dump_every && !opts.dump.empty() && i % opts.dump_every == 0) {
            window.capture_pixels = true;
            r.run_frame();
#ifdef SPIKE_OPENGL
            if (opts.opengl) {
                window.CaptureOpenGL();
            }
#endif
            window.capture_pixels = false;
            const std::string prefix = opts.dump + "-" + std::to_string(r.frame);
            write_bmp(prefix + "-top.bmp", window.top);
            write_bmp(prefix + "-bottom.bmp", window.bottom);
        }
        if (opts.state_every && i % opts.state_every == 0) {
            double ms = 0;
            auto state = r.save_state(&ms);
            if (state) {
                saves++;
                save_ms_total += ms;
                last_state_size = state->size();
                if (state->size() > 256) {
                    try {
                        last_state_raw = Common::Compression::DecompressDataZSTD(
                                             std::span<const u8>(state->data() + 256, state->size() - 256))
                                             .size();
                    } catch (...) {
                        last_state_raw = 0;
                    }
                }
            }
        }
    }
    const double run_ms = ms_since(run_start) - save_ms_total - r.hash_ms_total;
    const u64 timed = r.frame - opts.warmup - (opts.load_file.empty() ? 0 : 1);
    std::printf("timed_frames=%llu run_ms=%.1f fps=%.1f ms_per_frame=%.3f\n",
                static_cast<unsigned long long>(timed), run_ms, timed * 1000.0 / run_ms, run_ms / timed);
    if (saves) {
        std::printf("states=%llu save_ms_avg=%.1f state_bytes=%zu state_raw_bytes=%zu\n",
                    static_cast<unsigned long long>(saves), save_ms_total / saves, last_state_size,
                    last_state_raw);
    }
    if (r.hash_ms_total > 0) {
        std::printf("hash_ms_total=%.1f\n", r.hash_ms_total);
    }

    if (!opts.dump.empty()) {
        window.capture_pixels = true;
        r.run_frame();
#ifdef SPIKE_OPENGL
        if (opts.opengl) {
            window.CaptureOpenGL();
        }
#endif
        window.capture_pixels = false;
        std::printf("dump_top=%d dump_bottom=%d (top %ux%u, bottom %ux%u)\n",
                    write_bmp(opts.dump + "-top.bmp", window.top) ? 1 : 0,
                    write_bmp(opts.dump + "-bottom.bmp", window.bottom) ? 1 : 0, window.top.width,
                    window.top.height, window.bottom.width, window.bottom.height);
    }

    if (opts.raw_bench && !r.stopped) {
        std::vector<u8> raw;
        double ms = 0;
        for (u64 k = 0; k < opts.raw_bench; k++) {
            if (!r.drain_async()) break;
            auto t = Clock::now();
            system.SaveStateRaw(raw);
            const double save_ms = ms_since(t);
            t = Clock::now();
            const bool ok = system.LoadStateRaw(raw);
            const double load_ms = ms_since(t);
            window.HasSubmittedFrame();
            t = Clock::now();
            auto compressed = r.save_state(&ms);
            const double save_z_ms = compressed ? ms : -1;
            t = Clock::now();
            const bool ok2 = compressed && r.load_state(*compressed, &ms);
            const double load_z_ms = ok2 ? ms : -1;
            std::printf("raw_bench k=%llu raw_bytes=%zu save_raw_ms=%.0f load_raw_ms=%.0f ok=%d save_zstd_ms=%.0f load_zstd_ms=%.0f\n",
                        static_cast<unsigned long long>(k), raw.size(), save_ms, load_ms, ok ? 1 : 0,
                        save_z_ms, load_z_ms);
            if (!r.run_frames(60)) break;
        }
    }

    if (opts.dirty_bench && !r.stopped) {
        std::vector<u8> incremental;
        std::vector<u8> fresh;
        for (u64 k = 0; k < opts.dirty_bench; k++) {
            if (!r.run_frames(opts.dirty_frames) || !r.drain_async()) break;
            // Incremental: the buffer still holds the previous state. (Vectors are kept at their
            // capacity: growing one after the write would zero what was written.)
            auto save_into = [&](std::vector<u8>& buffer, std::size_t* pages_out) {
                for (;;) {
                    const std::size_t previous = buffer.size();
                    buffer.resize(buffer.capacity());
                    const std::size_t n = system.SaveStateRawInto(buffer.data(), buffer.size(), previous, pages_out);
                    if (n <= buffer.size()) { buffer.resize(n); return; }
                    buffer.resize(previous);
                    buffer.reserve(n + (1u << 20));
                }
            };
            auto t = Clock::now();
            std::size_t pages = 0;
            save_into(incremental, &pages);
            const double inc_ms = ms_since(t);
            // Full: the same allocation, but its header made unrecognisable.
            if (fresh.capacity() < incremental.size() + (1u << 20)) {
                fresh.resize(incremental.size() + (1u << 20));
            }
            fresh.resize(incremental.size());
            std::fill(fresh.begin(), fresh.begin() + 8, 0);
            t = Clock::now();
            std::size_t full_pages = 0;
            save_into(fresh, &full_pages);
            const double full_ms = ms_since(t);
            // Same bytes apart from the generation (bytes 8..16 of the header).
            const bool same_size = incremental.size() == fresh.size();
            const bool equal = same_size && std::memcmp(incremental.data(), fresh.data(), 8) == 0 &&
                               std::memcmp(incremental.data() + 16, fresh.data() + 16, incremental.size() - 16) == 0;
            std::size_t first_diff = 0;
            if (same_size && !equal) {
                for (first_diff = 16; first_diff < incremental.size(); first_diff++) {
                    if (incremental[first_diff] != fresh[first_diff]) break;
                }
            }
            // The frames right after a save: the rasterizer cache was dropped by it, so they
            // upload textures and surfaces again.
            std::string after;
            double after_total = 0;
            for (int f = 0; f < 8; f++) {
                const auto ft = Clock::now();
                if (!r.run_frame()) break;
                const double fms = ms_since(ft);
                after_total += fms;
                after += (f ? "," : "") + std::to_string(static_cast<int>(fms * 10 + 0.5) / 10) + "." + std::to_string(static_cast<int>(fms * 10 + 0.5) % 10);
            }
            std::printf("dirty_bench k=%llu frames=%llu incremental_ms=%.2f pages=%zu (%.1f MB) full_ms=%.2f full_pages=%zu bytes=%zu equal=%d first_diff=%zu next_frames_ms=%s (%.1f over 8)\n",
                        static_cast<unsigned long long>(k), static_cast<unsigned long long>(opts.dirty_frames), inc_ms,
                        pages, pages * 4096.0 / (1 << 20), full_ms, full_pages, incremental.size(), equal ? 1 : 0, first_diff,
                        after.c_str(), after_total);
            std::fflush(stdout);
        }
        // And that the incremental state loads: run on from it and from the full one (with the
        // same scripted input: the frame counter is put back), compare.
        if (!incremental.empty() && !fresh.empty()) {
            const u64 frame_at_state = r.frame;
            const bool ok_inc = system.LoadStateRaw(incremental);
            window.HasSubmittedFrame();
            if (ok_inc && r.run_frames(60)) {
                const u64 h_inc = r.hash_fcram();
                r.frame = frame_at_state;
                const bool ok_full = system.LoadStateRaw(fresh);
                window.HasSubmittedFrame();
                if (ok_full && r.run_frames(60)) {
                    std::printf("dirty_bench_load incremental_ok=%d full_ok=%d fcram_match=%d\n", ok_inc ? 1 : 0, ok_full ? 1 : 0,
                                h_inc == r.hash_fcram() ? 1 : 0);
                }
            }
        }
    }

    // zstd as the recorder's blobs use it (level 3, 128 MiB window, long-distance matching),
    // optionally with earlier bytes as a reference prefix the way a decoder that already holds
    // the previous keyframes' payloads could.
    auto big_zstd = [](const std::vector<u8>& data, std::span<const u8> prefix, int level) {
        ZSTD_CCtx* cctx = ZSTD_createCCtx();
        ZSTD_CCtx_setParameter(cctx, ZSTD_c_compressionLevel, level);
        ZSTD_CCtx_setParameter(cctx, ZSTD_c_windowLog, 27);
        ZSTD_CCtx_setParameter(cctx, ZSTD_c_enableLongDistanceMatching, 1);
        if (!prefix.empty()) {
            ZSTD_CCtx_refPrefix(cctx, prefix.data(), prefix.size());
        }
        std::vector<u8> out(ZSTD_compressBound(data.size()));
        const size_t n = ZSTD_compress2(cctx, out.data(), out.size(), data.data(), data.size());
        ZSTD_freeCCtx(cctx);
        return ZSTD_isError(n) ? static_cast<size_t>(0) : n;
    };

    // Keyframe-delta study: what a replay keyframe every n frames would cost. The recorder
    // stores each keyframe as the regions (4-byte granularity) that differ from the previous
    // keyframe, then zstd-compresses many keyframes together; this approximates one keyframe's
    // share with a per-sample zstd of its region payload (an upper bound on what a long blob
    // achieves, since cross-keyframe context is lost).
    if (opts.delta_every && !r.stopped) {
        auto save_raw = [&](std::vector<u8>& out, double* ms) {
            if (!r.drain_async()) return false;
            const auto t = Clock::now();
            system.SaveStateRaw(out);
            *ms = ms_since(t);
            return true;
        };
        double ms = 0, dec_ms = 0;
        std::vector<u8> prev;
        if (!save_raw(prev, &ms)) return 1;
        std::printf("delta_study every=%llu raw_bytes=%zu\n",
                    static_cast<unsigned long long>(opts.delta_every), prev.size());
        u64 sum_changed = 0, sum_delta_zstd = 0, sum_full_zstd = 0, sum_regions = 0, samples = 0;
        std::array<u64, 4> class_sum_changed{}, class_sum_z{};
        Runner::RawLayout layout;
        // Every sample's region payload appended, the way keyframes share one compressed blob.
        std::vector<u8> blob_all, blob_cpu, blob_linear;
        std::vector<u8> prev_payload, prev_payload2; // the previous two keyframes' payloads
        u64 sum_solo = 0, sum_prefix1 = 0, sum_prefix2 = 0;
        for (u64 k = 1; k <= opts.delta_count; k++) {
            if (!r.run_frames(opts.delta_every)) break;
            double save_ms = 0;
            std::vector<u8> cur;
            if (!save_raw(cur, &save_ms)) break;
            const size_t n = std::min(prev.size(), cur.size());
            const auto t_diff = Clock::now();
            // Changed 4-byte words, merged into regions when the gap between changes is
            // under 8 words (a gap costs less to carry than a new region header).
            if (layout.fcram_off == SIZE_MAX) {
                layout = r.raw_layout(cur);
                std::printf("raw_layout fcram_off=%zu fcram_size=%llu heap_pages=%zu linear_pages=%zu\n",
                            layout.fcram_off, static_cast<unsigned long long>(layout.fcram_size),
                            static_cast<size_t>(std::count(layout.page_class.begin(), layout.page_class.end(), 1)),
                            static_cast<size_t>(std::count(layout.page_class.begin(), layout.page_class.end(), 2)));
            }
            std::vector<u8> payload;
            std::array<std::vector<u8>, 4> class_payload;
            std::array<u64, 4> class_changed{};
            u64 changed_words = 0, regions = 0;
            size_t region_start = SIZE_MAX, region_end = 0;
            const u32* a = reinterpret_cast<const u32*>(prev.data());
            const u32* b = reinterpret_cast<const u32*>(cur.data());
            const size_t words = n / 4;
            auto flush = [&]() {
                if (region_start == SIZE_MAX) return;
                regions++;
                // control: start and length as 4-byte values (varints in the real format).
                const u32 hdr[2] = {static_cast<u32>(region_start), static_cast<u32>(region_end - region_start)};
                payload.insert(payload.end(), reinterpret_cast<const u8*>(hdr), reinterpret_cast<const u8*>(hdr) + 8);
                payload.insert(payload.end(), cur.data() + region_start * 4, cur.data() + region_end * 4);
                region_start = SIZE_MAX;
            };
            for (size_t w = 0; w < words; w++) {
                if (a[w] != b[w]) {
                    changed_words++;
                    const u8 c = Runner::classify(layout, w * 4);
                    class_changed[c] += 4;
                    class_payload[c].insert(class_payload[c].end(), cur.data() + w * 4, cur.data() + w * 4 + 4);
                    if (region_start != SIZE_MAX && w - region_end >= 8) flush();
                    if (region_start == SIZE_MAX) region_start = w;
                    region_end = w + 1;
                }
            }
            flush();
            const double diff_ms = ms_since(t_diff);
            const auto t_z = Clock::now();
            const auto delta_z = Common::Compression::CompressDataZSTD(payload, 3);
            const double delta_z_ms = ms_since(t_z);
            const auto t_full = Clock::now();
            const auto full_z = Common::Compression::CompressDataZSTD(cur, 3);
            const double full_z_ms = ms_since(t_full);
            {
                const size_t solo = big_zstd(payload, {}, 3);
                const size_t p1 = big_zstd(payload, prev_payload, 3);
                std::vector<u8> two = prev_payload2;
                two.insert(two.end(), prev_payload.begin(), prev_payload.end());
                const size_t p2 = big_zstd(payload, two, 3);
                std::printf("delta_ref k=%llu solo_bigwindow=%zu prefix_prev1=%zu prefix_prev2=%zu\n",
                            static_cast<unsigned long long>(k), solo, p1, p2);
                sum_solo += solo; sum_prefix1 += p1; sum_prefix2 += p2;
                prev_payload2 = std::move(prev_payload);
                prev_payload = payload;
            }
            blob_all.insert(blob_all.end(), payload.begin(), payload.end());
            for (int c = 0; c < 4; c++) {
                auto& dst = (c == 2) ? blob_linear : blob_cpu;
                dst.insert(dst.end(), class_payload[c].begin(), class_payload[c].end());
            }
            static const char* class_names[4] = {"fcram_other", "heap", "linear", "non_fcram"};
            for (int c = 0; c < 4; c++) {
                const auto z = Common::Compression::CompressDataZSTD(class_payload[c], 3);
                std::printf("delta_class k=%llu %s changed_bytes=%llu zstd3=%zu\n",
                            static_cast<unsigned long long>(k), class_names[c],
                            static_cast<unsigned long long>(class_changed[c]), z.size());
                class_sum_changed[c] += class_changed[c];
                class_sum_z[c] += z.size();
            }
            std::printf("delta k=%llu frame=%llu changed_bytes=%llu regions=%llu payload=%zu delta_zstd3=%zu full_zstd3=%zu azahar_state=%zu save_ms=%.0f decompress_ms=%.0f diff_ms=%.0f delta_zstd_ms=%.0f full_zstd_ms=%.0f\n",
                        static_cast<unsigned long long>(k), static_cast<unsigned long long>(r.frame),
                        static_cast<unsigned long long>(changed_words * 4), static_cast<unsigned long long>(regions),
                        payload.size(), delta_z.size(), full_z.size(), full_z.size(), save_ms, dec_ms, diff_ms,
                        delta_z_ms, full_z_ms);
            sum_changed += changed_words * 4;
            sum_delta_zstd += delta_z.size();
            sum_full_zstd += full_z.size();
            sum_regions += regions;
            samples++;
            prev = std::move(cur);
        }
        samples = std::max<u64>(1, samples);
        {
            // The recorder's blob compression: zstd level 3, 128 MiB window, long-distance matching.
            auto blob_zstd = [](const std::vector<u8>& data, int level) {
                ZSTD_CCtx* cctx = ZSTD_createCCtx();
                ZSTD_CCtx_setParameter(cctx, ZSTD_c_compressionLevel, level);
                ZSTD_CCtx_setParameter(cctx, ZSTD_c_windowLog, 27);
                ZSTD_CCtx_setParameter(cctx, ZSTD_c_enableLongDistanceMatching, 1);
                std::vector<u8> out(ZSTD_compressBound(data.size()));
                const size_t n = ZSTD_compress2(cctx, out.data(), out.size(), data.data(), data.size());
                ZSTD_freeCCtx(cctx);
                return ZSTD_isError(n) ? static_cast<size_t>(0) : n;
            };
            std::printf("delta_ref_avg every=%llu solo_bigwindow=%llu prefix_prev1=%llu prefix_prev2=%llu\n",
                        static_cast<unsigned long long>(opts.delta_every),
                        static_cast<unsigned long long>(sum_solo / samples),
                        static_cast<unsigned long long>(sum_prefix1 / samples),
                        static_cast<unsigned long long>(sum_prefix2 / samples));
            for (int level : {3}) {
                const auto t = Clock::now();
                const size_t all = blob_zstd(blob_all, level);
                const double all_ms = ms_since(t);
                const size_t cpu = blob_zstd(blob_cpu, level);
                const size_t lin = blob_zstd(blob_linear, level);
                std::printf("blob_zstd every=%llu level=%d samples=%llu all_per_keyframe=%zu cpu_per_keyframe=%zu linear_per_keyframe=%zu all_payload=%zu compress_ms=%.0f\n",
                            static_cast<unsigned long long>(opts.delta_every), level,
                            static_cast<unsigned long long>(samples), all / samples, cpu / samples, lin / samples,
                            blob_all.size(), all_ms);
            }
            static const char* class_names[4] = {"fcram_other", "heap", "linear", "non_fcram"};
            for (int c = 0; c < 4; c++) {
                std::printf("delta_class_avg every=%llu %s changed_bytes=%llu zstd3=%llu\n",
                            static_cast<unsigned long long>(opts.delta_every), class_names[c],
                            static_cast<unsigned long long>(class_sum_changed[c] / samples),
                            static_cast<unsigned long long>(class_sum_z[c] / samples));
            }
        }
        std::printf("delta_avg every=%llu samples=%llu changed_bytes=%llu regions=%llu delta_zstd3=%llu full_zstd3=%llu\n",
                    static_cast<unsigned long long>(opts.delta_every), static_cast<unsigned long long>(samples),
                    static_cast<unsigned long long>(sum_changed / samples),
                    static_cast<unsigned long long>(sum_regions / samples),
                    static_cast<unsigned long long>(sum_delta_zstd / samples),
                    static_cast<unsigned long long>(sum_full_zstd / samples));
    }

    // Stale-region test: can the linear heap (variant A) or everything in FCRAM but the process
    // heap (variant B) be left stale in a keyframe without changing what the game does?
    if (opts.mask_test && !r.stopped) {
        auto save_raw = [&](std::vector<u8>& out) {
            if (!r.drain_async()) return false;
            system.SaveStateRaw(out);
            return true;
        };
        auto load_raw = [&](const std::vector<u8>& raw) {
            const bool ok = system.LoadStateRaw(raw);
            window.HasSubmittedFrame();
            return ok;
        };
        auto capture = [&]() {
            window.capture_pixels = true;
            r.run_frame();
#ifdef SPIKE_OPENGL
            if (opts.opengl) window.CaptureOpenGL();
#endif
            window.capture_pixels = false;
            return std::make_pair(window.top.bgra, window.bottom.bgra);
        };
        auto pixel_diff = [](const std::vector<u8>& x, const std::vector<u8>& y) {
            if (x.size() != y.size()) return static_cast<u64>(-1);
            u64 n = 0;
            for (size_t i = 0; i + 3 < x.size(); i += 4) {
                if (x[i] != y[i] || x[i + 1] != y[i + 1] || x[i + 2] != y[i + 2]) n++;
            }
            return n;
        };

        std::vector<u8> raw0, raw1;
        if (!save_raw(raw0)) return 1;
        const auto layout = r.raw_layout(raw0);
        if (layout.fcram_off == SIZE_MAX) {
            std::printf("mask_test=no_layout\n");
            return 1;
        }
        const u64 frame0 = r.frame;
        if (!r.run_frames(120)) return 1;
        if (!save_raw(raw1)) return 1;
        const u64 frame1 = r.frame;

        struct Variant { const char* name; bool stale_linear; bool stale_other; };
        const Variant variants[] = {{"control", false, false}, {"stale_linear", true, false},
                                    {"stale_all_but_heap", true, true}};
        std::vector<u8> control_top_first, control_bottom_first, control_top_last, control_bottom_last;
        u64 control_heap = 0;
        for (const auto& v : variants) {
            std::vector<u8> state = raw1;
            u64 stale_bytes = 0;
            if (v.stale_linear || v.stale_other) {
                for (size_t page = 0; page < layout.page_class.size(); page++) {
                    const u8 c = layout.page_class[page];
                    const bool stale = (c == 2 && v.stale_linear) || (c == 0 && v.stale_other);
                    if (!stale) continue;
                    const size_t off = layout.fcram_off + page * 4096;
                    if (std::memcmp(state.data() + off, raw0.data() + off, 4096) != 0) stale_bytes += 4096;
                    std::memcpy(state.data() + off, raw0.data() + off, 4096);
                }
            }
            if (!load_raw(state)) return 1;
            r.frame = frame1;
            auto first = capture();
            if (!r.run_frames(opts.mask_test - 2)) return 1;
            auto last = capture();
            const auto h = r.hash_regions();
            if (std::string(v.name) == "control") {
                control_top_first = first.first; control_bottom_first = first.second;
                control_top_last = last.first; control_bottom_last = last.second;
                control_heap = h.heap;
                std::printf("mask_test variant=%s frames=%llu heap=%016llx\n", v.name,
                            static_cast<unsigned long long>(opts.mask_test), static_cast<unsigned long long>(h.heap));
            } else {
                std::printf("mask_test variant=%s stale_pages_bytes=%llu heap_match=%d first_frame_pixels_differ top=%llu bottom=%llu last_frame_pixels_differ top=%llu bottom=%llu\n",
                            v.name, static_cast<unsigned long long>(stale_bytes), h.heap == control_heap ? 1 : 0,
                            static_cast<unsigned long long>(pixel_diff(first.first, control_top_first)),
                            static_cast<unsigned long long>(pixel_diff(first.second, control_bottom_first)),
                            static_cast<unsigned long long>(pixel_diff(last.first, control_top_last)),
                            static_cast<unsigned long long>(pixel_diff(last.second, control_bottom_last)));
                if (!opts.dump.empty()) {
                    Screen s; s.width = 400; s.height = 240; s.bgra = last.first;
                    write_bmp(opts.dump + "-" + v.name + "-last-top.bmp", s);
                    s.bgra = first.first;
                    write_bmp(opts.dump + "-" + v.name + "-first-top.bmp", s);
                }
            }
            if (std::string(v.name) == "control" && !opts.dump.empty()) {
                Screen s; s.width = 400; s.height = 240; s.bgra = last.first;
                write_bmp(opts.dump + "-control-last-top.bmp", s);
            }
        }
        (void)frame0;
    }

    // Final state: for the round trip and/or the file.
    if ((opts.roundtrip || !opts.save_file.empty()) && !r.stopped) {
        double ms = 0;
        auto state = r.save_state(&ms);
        if (!state) {
            std::printf("final_state=failed\n");
            return 1;
        }
        std::printf("final_state_bytes=%zu save_ms=%.1f at_frame=%llu\n", state->size(), ms,
                    static_cast<unsigned long long>(r.frame));
        if (!opts.save_file.empty()) {
            std::ofstream f(opts.save_file, std::ios::binary);
            f.write(reinterpret_cast<const char*>(state->data()), state->size());
            std::printf("save_file=%s\n", opts.save_file.c_str());
        }
        if (opts.roundtrip) {
            const u64 at = r.frame;
            if (!r.run_frames(opts.roundtrip)) return 1;
            r.print_hashes("roundtrip_a");
            const auto a = r.hash_regions();
            double load_ms = 0;
            if (!r.load_state(*state, &load_ms)) {
                std::printf("roundtrip=load_failed\n");
                return 1;
            }
            r.frame = at;
            if (!r.run_frames(opts.roundtrip)) return 1;
            r.print_hashes("roundtrip_b");
            const auto b = r.hash_regions();
            std::printf("roundtrip frames=%llu load_ms=%.1f heap_match=%d linear_match=%d\n",
                        static_cast<unsigned long long>(opts.roundtrip), load_ms,
                        a.heap == b.heap ? 1 : 0, a.linear == b.linear ? 1 : 0);
        }
    }

    system.Shutdown();
    std::printf("done frames=%llu\n", static_cast<unsigned long long>(r.frame));
    return 0;
}
