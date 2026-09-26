// C ABI over Azahar's core for the azahar-rs crate. See src/lib.rs for the contract.
//
// Shape follows the headless spike (azahar-rs/spike/spike.cpp): one frame = run the core until
// the renderer's VBlank; pixels come out of the OpenGL renderer's texture mailbox through
// TryPresent into an offscreen framebuffer; input goes in through Azahar's device factories;
// states are the raw serialisation (patch 0002); seeks skip drawing (patch 0003).

#include <algorithm>
#include <array>
#include <atomic>
#include <cstdint>
#include <cstring>
#include <memory>
#include <string>
#include <vector>

#include "audio_core/input_details.h"
#include "audio_core/sink_details.h"
#include "common/common_paths.h"
#include "common/file_util.h"
#include "common/logging/backend.h"
#include "common/logging/filter.h"
#include "common/logging/log.h"
#include "common/param_package.h"
#include "common/settings.h"
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
#include "video_core/rasterizer_interface.h"
#include "video_core/renderer_base.h"
#include "common/archives.h"

#ifdef _WIN32
#ifndef WIN32_LEAN_AND_MEAN
#define WIN32_LEAN_AND_MEAN
#endif
#include <windows.h>
#define AZAHAR_RS_HAVE_GL 1
#endif

#ifdef AZAHAR_RS_HAVE_GL
#include "glad/glad.h"
#include "video_core/renderer_opengl/gl_state.h"
#endif

namespace {

// ------------------------------------------------------------------ ABI structs (mirror lib.rs)

struct AzaharRsSettings {
    bool new_3ds;
    bool jit;
    int32_t region;
    uint64_t init_time;
};

struct AzaharRsInput {
    uint32_t buttons;
    int8_t circle_x;
    int8_t circle_y;
    int8_t c_stick_x;
    int8_t c_stick_y;
    bool touch_pressed;
    uint16_t touch_x;
    uint16_t touch_y;
};

struct AzaharRsRegion {
    uint32_t virtual_address;
    uint32_t length;
    uint32_t kind;
    const uint8_t* data;
};

constexpr unsigned TOP_W = 400, TOP_H = 240, BOTTOM_W = 320, BOTTOM_H = 240;
constexpr unsigned LAYOUT_W = 400, LAYOUT_H = 480;

// ------------------------------------------------------------------ input devices

AzaharRsInput g_input{};

class RsButton final : public Input::ButtonDevice {
public:
    explicit RsButton(int index) : bit(index >= 0 && index < 32 ? (1u << index) : 0) {}
    bool GetStatus() const override {
        return (g_input.buttons & bit) != 0;
    }

private:
    uint32_t bit;
};

class RsButtonFactory final : public Input::Factory<Input::ButtonDevice> {
public:
    std::unique_ptr<Input::ButtonDevice> Create(const Common::ParamPackage& params) override {
        return std::make_unique<RsButton>(params.Get("button", 0));
    }
};

class RsAxis final : public Input::AnalogDevice {
public:
    explicit RsAxis(int axis) : axis(axis) {}
    std::tuple<float, float> GetStatus() const override {
        if (axis == 0) {
            return {g_input.circle_x / 127.0f, g_input.circle_y / 127.0f};
        }
        return {g_input.c_stick_x / 127.0f, g_input.c_stick_y / 127.0f};
    }

private:
    int axis;
};

class RsAxisFactory final : public Input::Factory<Input::AnalogDevice> {
public:
    std::unique_ptr<Input::AnalogDevice> Create(const Common::ParamPackage& params) override {
        return std::make_unique<RsAxis>(params.Get("axis", 0));
    }
};

// ------------------------------------------------------------------ window

class RsWindow final : public Frontend::EmuWindow {
public:
    RsWindow() {
        strict_context_required = true;
        window_info.type = Frontend::WindowSystemType::Headless;
        UpdateCurrentFramebufferLayout(LAYOUT_W, LAYOUT_H);
    }

    ~RsWindow() override {
#ifdef _WIN32
        if (hglrc) {
            wglMakeCurrent(nullptr, nullptr);
            wglDeleteContext(hglrc);
        }
        if (hdc && hwnd) ReleaseDC(hwnd, hdc);
        if (hwnd) DestroyWindow(hwnd);
#endif
    }

    // Every emulated VBlank (RendererBase::EndFrame) lands here.
    void PollEvents() override {
        submitted = true;
    }
    void SwapBuffers() override {
        submitted = true;
    }
    void MakeCurrent() override {
#ifdef _WIN32
        if (hglrc) wglMakeCurrent(hdc, hglrc);
#endif
    }
    void DoneCurrent() override {
#ifdef _WIN32
        if (hglrc) wglMakeCurrent(nullptr, nullptr);
#endif
    }

    // The context follows the thread that calls into the core: the app makes the core on its UI
    // thread and runs it on its core thread, and a WGL context is current on one thread at a
    // time. False when it is still held by another thread (nothing released it there).
    bool EnsureCurrent() {
#ifdef _WIN32
        if (!hglrc) return false;
        if (wglGetCurrentContext() == hglrc) return true;
        return wglMakeCurrent(hdc, hglrc) != FALSE;
#else
        return false;
#endif
    }

    bool TakeSubmitted() {
        const bool s = submitted;
        submitted = false;
        return s;
    }

    bool CreateGlContext() {
#ifdef _WIN32
        WNDCLASSA wc{};
        wc.style = CS_OWNDC;
        wc.lpfnWndProc = DefWindowProcA;
        wc.hInstance = GetModuleHandleA(nullptr);
        wc.lpszClassName = "SuperShuckieAzaharGL";
        RegisterClassA(&wc);
        hwnd = CreateWindowExA(0, wc.lpszClassName, "supershuckie 3ds", WS_OVERLAPPEDWINDOW, 0, 0,
                               LAYOUT_W, LAYOUT_H, nullptr, nullptr, wc.hInstance, nullptr);
        if (!hwnd) return false;
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
        if (format == 0 || !SetPixelFormat(hdc, format, &pfd)) return false;
        HGLRC legacy = wglCreateContext(hdc);
        if (!legacy || !wglMakeCurrent(hdc, legacy)) return false;
        using CreateContextAttribs = HGLRC(WINAPI*)(HDC, HGLRC, const int*);
        auto create = reinterpret_cast<CreateContextAttribs>(wglGetProcAddress("wglCreateContextAttribsARB"));
        if (!create) return false;
        const int attribs[] = {0x2091, 4, 0x2092, 3, 0x9126, 1, 0};
        hglrc = create(hdc, nullptr, attribs);
        wglMakeCurrent(nullptr, nullptr);
        wglDeleteContext(legacy);
        if (!hglrc || !wglMakeCurrent(hdc, hglrc)) return false;
        static bool glad_loaded = false;
        if (!glad_loaded) {
            if (!gladLoadGL()) return false;
            glad_loaded = true;
        }
        return true;
#else
        return false;
#endif
    }

    // Take the newest rendered frame out of the renderer's mailbox into our framebuffer and read
    // it back as 0xAARRGGBB, split into the two screens.
    void Capture(std::vector<uint32_t>& top, std::vector<uint32_t>& bottom) {
#ifdef AZAHAR_RS_HAVE_GL
        auto& renderer = Core::System::GetInstance().GPU().Renderer();
        const auto prev_state = OpenGL::OpenGLState::GetCurState();
        if (!fbo) {
            glGenFramebuffers(1, &fbo);
            glGenRenderbuffers(1, &rbo);
            glBindRenderbuffer(GL_RENDERBUFFER, rbo);
            glRenderbufferStorage(GL_RENDERBUFFER, GL_RGBA8, LAYOUT_W, LAYOUT_H);
            glBindFramebuffer(GL_FRAMEBUFFER, fbo);
            glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_RENDERBUFFER, rbo);
            rgba.resize(static_cast<size_t>(LAYOUT_W) * LAYOUT_H * 4);
        }
        glBindFramebuffer(GL_DRAW_FRAMEBUFFER, fbo);
        renderer.TryPresent(0);
        glBindFramebuffer(GL_READ_FRAMEBUFFER, fbo);
        glReadBuffer(GL_COLOR_ATTACHMENT0);
        glPixelStorei(GL_PACK_ALIGNMENT, 1);
        glReadPixels(0, 0, LAYOUT_W, LAYOUT_H, GL_RGBA, GL_UNSIGNED_BYTE, rgba.data());
        OpenGL::OpenGLState{}.Apply();
        prev_state.Apply();

        const auto& layout = GetFramebufferLayout();
        auto crop = [&](std::vector<uint32_t>& out, const Common::Rectangle<u32>& rect, unsigned w, unsigned h) {
            if (rect.GetWidth() != w || rect.GetHeight() != h) return;
            for (unsigned y = 0; y < h; y++) {
                const unsigned gy = LAYOUT_H - 1 - (rect.top + y); // GL rows run bottom-up
                const uint8_t* src = rgba.data() + (static_cast<size_t>(gy) * LAYOUT_W + rect.left) * 4;
                uint32_t* dst = out.data() + static_cast<size_t>(y) * w;
                for (unsigned x = 0; x < w; x++, src += 4) {
                    dst[x] = 0xFF000000u | (static_cast<uint32_t>(src[0]) << 16) |
                             (static_cast<uint32_t>(src[1]) << 8) | src[2];
                }
            }
        };
        crop(top, layout.top_screen, TOP_W, TOP_H);
        crop(bottom, layout.bottom_screen, BOTTOM_W, BOTTOM_H);
#endif
    }

    void Touch(const AzaharRsInput& in) {
        const auto& layout = GetFramebufferLayout();
        if (in.touch_pressed) {
            const unsigned x = layout.bottom_screen.left + std::min<unsigned>(in.touch_x, BOTTOM_W - 1);
            const unsigned y = layout.bottom_screen.top + std::min<unsigned>(in.touch_y, BOTTOM_H - 1);
            if (touching) TouchMoved(x, y);
            else touching = TouchPressed(x, y);
        } else if (touching) {
            TouchReleased();
            touching = false;
        }
    }

private:
#ifdef _WIN32
    HWND hwnd = nullptr;
    HDC hdc = nullptr;
    HGLRC hglrc = nullptr;
#endif
#ifdef AZAHAR_RS_HAVE_GL
    GLuint fbo = 0, rbo = 0;
    std::vector<uint8_t> rgba;
#endif
    bool submitted = false;
    bool touching = false;
};

// ------------------------------------------------------------------ the core

struct AzaharCore {
    std::unique_ptr<RsWindow> window;
    std::vector<uint32_t> top = std::vector<uint32_t>(TOP_W * TOP_H, 0xFF000000u);
    std::vector<uint32_t> bottom = std::vector<uint32_t>(BOTTOM_W * BOTTOM_H, 0xFF000000u);
    std::vector<uint8_t> state;
    std::string rom_path;
    std::string error;
    bool loaded = false;
};

std::atomic<bool> g_instance{false};
std::string g_static_error;
bool g_static_init = false;

// A second game in the same process gets its own directory: the derived paths were filled in
// for the first one, so point each at the new root explicitly.
void SetDerivedUserPaths(const std::string& dir) {
    const std::pair<FileUtil::UserPath, const char*> derived[] = {
        {FileUtil::UserPath::ConfigDir, CONFIG_DIR},   {FileUtil::UserPath::CacheDir, CACHE_DIR},
        {FileUtil::UserPath::SDMCDir, SDMC_DIR},       {FileUtil::UserPath::NANDDir, NAND_DIR},
        {FileUtil::UserPath::SysDataDir, SYSDATA_DIR}, {FileUtil::UserPath::LogDir, LOG_DIR},
        {FileUtil::UserPath::CheatsDir, CHEATS_DIR},   {FileUtil::UserPath::DLLDir, DLL_DIR},
        {FileUtil::UserPath::ShaderDir, SHADER_DIR},   {FileUtil::UserPath::DumpDir, DUMP_DIR},
        {FileUtil::UserPath::LoadDir, LOAD_DIR},       {FileUtil::UserPath::StatesDir, STATES_DIR},
        {FileUtil::UserPath::IconsDir, ICONS_DIR},
    };
    for (const auto& [path, name] : derived) {
        const std::string full = dir + name + "/";
        FileUtil::CreateFullPath(full);
        FileUtil::UpdateUserPath(path, full);
    }
}

void StaticInit() {
    if (g_static_init) return;
    g_static_init = true;
    Common::Log::Initialize();
    Common::Log::SetColorConsoleBackendEnabled(false);
    Common::Log::SetGlobalFilter(Common::Log::Filter(Common::Log::Level::Error));
    Common::Log::Start();
    auto& system = Core::System::GetInstance();
    Frontend::RegisterDefaultApplets(system);
    system.RegisterImageInterface(std::make_shared<Frontend::ImageInterface>());
    Input::RegisterFactory<Input::ButtonDevice>("supershuckie", std::make_shared<RsButtonFactory>());
    Input::RegisterFactory<Input::AnalogDevice>("supershuckie", std::make_shared<RsAxisFactory>());
    for (const auto& module : Service::service_module_map) {
        Settings::values.lle_modules.emplace(module.name, false);
    }
}

// Everything that could differ between two machines or two runs is pinned here; the replay
// header records the choices that matter (see replay-3ds-spec.md §3.1).
void ApplySettings(const AzaharRsSettings& s) {
    auto& v = Settings::values;
    v.use_cpu_jit.SetValue(s.jit);
    v.cpu_clock_percentage.SetValue(100);
    v.is_new_3ds.SetValue(s.new_3ds);
    v.region_value.SetValue(s.region);
    v.init_clock.SetValue(Settings::InitClock::FixedTime);
    v.init_time.SetValue(s.init_time);
    v.init_ticks_type.SetValue(Settings::InitTicks::Fixed);
    v.init_ticks_override.SetValue(0);
    v.deterministic_async_operations.SetValue(true);
    v.async_fs_operations.SetValue(false);
    v.async_presentation.SetValue(false);
    v.async_custom_loading.SetValue(false);
    v.graphics_api.SetValue(Settings::GraphicsAPI::OpenGL);
    v.use_hw_shader.SetValue(true);
    // Shaders compile on first use (a scene change can stall for seconds); the cache lives in the
    // user directory so a game only pays that once per machine.
    v.use_disk_shader_cache.SetValue(true);
    v.async_shader_compilation.SetValue(false);
    v.use_shader_jit.SetValue(true);
    v.resolution_factor.SetValue(1);
    v.frame_limit.SetValue(0);
    v.layout_option.SetValue(Settings::LayoutOption::Default);
    v.render_3d.SetValue(Settings::StereoRenderOption::Off);
    v.audio_emulation.SetValue(Settings::AudioEmulation::HLE);
    v.enable_audio_stretching.SetValue(false);
    v.output_type.SetValue(AudioCore::SinkType::Null);
    v.input_type.SetValue(AudioCore::InputType::Static);
    v.custom_textures.SetValue(false);
    v.dump_textures.SetValue(false);

    auto& profile = v.current_input_profile;
    for (int i = 0; i < Settings::NativeButton::NumButtons; i++) {
        profile.buttons[i] = "engine:supershuckie,button:" + std::to_string(i);
    }
    profile.analogs[Settings::NativeAnalog::CirclePad] = "engine:supershuckie,axis:0";
    profile.analogs[Settings::NativeAnalog::CStick] = "engine:supershuckie,axis:1";
    profile.motion_device = "engine:null";
    profile.touch_device = "engine:emu_window";
}

const char* StatusName(Core::System::ResultStatus s) {
    using R = Core::System::ResultStatus;
    switch (s) {
    case R::ErrorLoader_ErrorEncrypted: return "the game is encrypted (Azahar needs a decrypted dump)";
    case R::ErrorLoader_ErrorInvalidFormat: return "not a 3DS game file Azahar understands";
    case R::ErrorLoader: return "the loader failed";
    case R::ErrorSystemFiles: return "a 3DS system file is missing";
    case R::ErrorN3DSApplication: return "this game needs New 3DS mode";
    case R::ErrorCoreExceptionRaised: return "the emulated program crashed";
    case R::ShutdownRequested: return "the emulated program shut down";
    case R::ErrorSavestate: return "save state error";
    default: return "error";
    }
}

bool Load(AzaharCore& c) {
    auto& system = Core::System::GetInstance();
    const auto status = system.Load(*c.window, c.rom_path);
    if (status != Core::System::ResultStatus::Success) {
        c.error = std::string(StatusName(status)) + ": " + system.GetStatusDetails();
        c.loaded = false;
        return false;
    }
    system.RegisterCoreLoopThreadId();
    c.window->TakeSubmitted();
    c.loaded = true;
    return true;
}

} // namespace

extern "C" AzaharCore* azahar_rs_core_new(const char* rom_path, const char* user_dir,
                                           const AzaharRsSettings* settings, uint32_t* error_out) {
    *error_out = 0;
#ifndef AZAHAR_RS_HAVE_GL
    *error_out = 5;
    return nullptr;
#endif
    if (g_instance.exchange(true)) {
        *error_out = 3;
        return nullptr;
    }
    std::string dir = user_dir ? user_dir : "";
    if (!dir.empty() && dir.back() != '/' && dir.back() != '\\') dir += '/';
    FileUtil::CreateFullPath(dir);
    // Before anything asks for a path: the first request fills in every directory (SD card,
    // NAND, shaders, log...) from the user directory of that moment, and a later SetUserPath
    // only replaces the root. Logging's start in StaticInit is such a request, and with the
    // order the other way round every game's saves and shader cache went to the user's own
    // Azahar folder under AppData.
    FileUtil::SetUserPath(dir);
    SetDerivedUserPaths(dir);
    StaticInit();
    ApplySettings(*settings);

    auto core = std::make_unique<AzaharCore>();
    core->rom_path = rom_path ? rom_path : "";
    core->window = std::make_unique<RsWindow>();
    if (!core->window->CreateGlContext()) {
        g_instance = false;
        *error_out = 4;
        return nullptr;
    }
    if (!Load(*core)) {
        g_static_error = core->error;
        g_instance = false;
        *error_out = 1;
        return nullptr;
    }
    // Let go of the context: the thread that runs the core takes it (EnsureCurrent), and it is
    // usually not this one.
    core->window->DoneCurrent();
    return core.release();
}

namespace {

// Every entry point that reaches OpenGL starts here; a false return means another thread still
// holds the context, which is reported rather than left to GL errors and a crash later.
bool ContextReady(AzaharCore* core) {
    if (core->window->EnsureCurrent()) return true;
    core->error = "the 3DS core's OpenGL context is held by another thread";
    return false;
}

} // namespace

extern "C" void azahar_rs_core_free(AzaharCore* core) {
    if (!core) return;
    core->window->EnsureCurrent();
    auto& system = Core::System::GetInstance();
    if (system.IsPoweredOn()) {
        system.Shutdown();
    }
    delete core;
    g_instance = false;
}

extern "C" const char* azahar_rs_core_last_error(const AzaharCore* core) {
    return core ? core->error.c_str() : g_static_error.c_str();
}

extern "C" bool azahar_rs_core_run_frame(AzaharCore* core, bool skip_drawing) {
    if (!core->loaded) return false;
    if (!ContextReady(core)) return false;
    VideoCore::g_skip_drawing.store(skip_drawing, std::memory_order_relaxed);
    auto& system = Core::System::GetInstance();
    core->window->Touch(g_input);
    while (!core->window->TakeSubmitted()) {
        const auto status = system.RunLoop();
        if (status != Core::System::ResultStatus::Success) {
            core->error = std::string(StatusName(status)) + ": " + system.GetStatusDetails();
            core->loaded = false;
            return false;
        }
    }
    if (!skip_drawing) {
        core->window->Capture(core->top, core->bottom);
    }
    return true;
}

extern "C" const uint32_t* azahar_rs_core_get_pixels(const AzaharCore* core, uint32_t screen) {
    return screen == 0 ? core->top.data() : screen == 1 ? core->bottom.data() : nullptr;
}

extern "C" void azahar_rs_core_set_input(AzaharCore* core, const AzaharRsInput* input) {
    (void)core;
    g_input = *input;
}

extern "C" bool azahar_rs_core_state_pending(const AzaharCore* core) {
    (void)core;
    auto& system = Core::System::GetInstance();
    return system.KernelRunning() && system.Kernel().AreAsyncOperationsPending();
}

extern "C" size_t azahar_rs_core_save_state_raw(AzaharCore* core) {
    if (!core->loaded || azahar_rs_core_state_pending(core) || !ContextReady(core)) return 0;
    try {
        Core::System::GetInstance().SaveStateRaw(core->state);
    } catch (const std::exception& e) {
        core->error = std::string("save state: ") + e.what();
        return 0;
    }
    return core->state.size();
}

extern "C" const uint8_t* azahar_rs_core_state_data(const AzaharCore* core) {
    return core->state.data();
}

// The raw state written straight into `buffer`, whose first `valid_len` bytes are a previous
// raw state or anything else; when they are a recent state of this core, only the RAM pages
// written since are copied (Core::System::SaveStateRawInto). Returns the state's size, which is
// more than `capacity` when the rest did not fit (grow the buffer keeping its contents and call
// again), and 0 when no state can be taken right now.
extern "C" size_t azahar_rs_core_save_state_raw_into(AzaharCore* core, uint8_t* buffer, size_t capacity,
                                                    size_t valid_len) {
    if (!core->loaded || azahar_rs_core_state_pending(core) || !ContextReady(core)) return 0;
    try {
        return Core::System::GetInstance().SaveStateRawInto(buffer, capacity, valid_len);
    } catch (const std::exception& e) {
        core->error = std::string("save state: ") + e.what();
        return 0;
    }
}

extern "C" bool azahar_rs_core_load_state_raw(AzaharCore* core, const uint8_t* data, size_t len) {
    if (!core->loaded || !ContextReady(core)) return false;
    try {
        if (!Core::System::GetInstance().LoadStateRaw(std::span<const uint8_t>(data, len))) return false;
    } catch (const std::exception& e) {
        core->error = std::string("load state: ") + e.what();
        return false;
    }
    core->window->TakeSubmitted();
    return true;
}

namespace {

bool RangeMapped(Core::System& system, uint32_t address, size_t len) {
    auto process = system.Kernel().GetCurrentProcess();
    if (!process) return false;
    auto& memory = system.Memory();
    const uint64_t end = static_cast<uint64_t>(address) + len;
    if (end > 0x1'0000'0000ull) return false;
    for (uint64_t page = address & ~0xFFFull; page < end; page += 0x1000) {
        if (!memory.IsValidVirtualAddress(*process, static_cast<u32>(page))) return false;
    }
    return true;
}

} // namespace

extern "C" bool azahar_rs_core_read_memory(const AzaharCore* core, uint32_t address, uint8_t* out, size_t len) {
    if (!core->loaded || len == 0) return false;
    auto& system = Core::System::GetInstance();
    if (!system.KernelRunning() || !RangeMapped(system, address, len)) return false;
    system.Memory().ReadBlock(*system.Kernel().GetCurrentProcess(), address, out, len);
    return true;
}

extern "C" bool azahar_rs_core_write_memory(AzaharCore* core, uint32_t address, const uint8_t* data, size_t len) {
    if (!core->loaded || len == 0) return false;
    auto& system = Core::System::GetInstance();
    if (!system.KernelRunning() || !RangeMapped(system, address, len)) return false;
    system.Memory().WriteBlock(*system.Kernel().GetCurrentProcess(), address, data, len);
    system.InvalidateCacheRange(address, len);
    return true;
}

extern "C" bool azahar_rs_core_region(const AzaharCore* core, uint32_t index, AzaharRsRegion* out) {
    if (!core->loaded) return false;
    auto& system = Core::System::GetInstance();
    if (!system.KernelRunning()) return false;
    auto process = system.Kernel().GetCurrentProcess();
    if (!process) return false;
    uint32_t i = 0;
    for (const auto& [addr, vma] : process->vm_manager.vma_map) {
        if (vma.type != Kernel::VMAType::BackingMemory || vma.size == 0 || !vma.backing_memory) continue;
        uint32_t kind = 0;
        if (vma.base >= Memory::HEAP_VADDR && vma.base < Memory::HEAP_VADDR_END) kind = 1;
        else if ((vma.base >= Memory::LINEAR_HEAP_VADDR && vma.base < Memory::LINEAR_HEAP_VADDR_END) ||
                 (vma.base >= Memory::NEW_LINEAR_HEAP_VADDR && vma.base < Memory::NEW_LINEAR_HEAP_VADDR_END)) kind = 2;
        else continue;
        if (i++ != index) continue;
        out->virtual_address = vma.base;
        out->length = vma.size;
        out->kind = kind;
        out->data = vma.backing_memory.GetPtr();
        return true;
    }
    return false;
}

extern "C" bool azahar_rs_core_reset(AzaharCore* core) {
    if (!ContextReady(core)) return false;
    auto& system = Core::System::GetInstance();
    if (system.IsPoweredOn()) {
        system.Shutdown();
    }
    return Load(*core);
}
