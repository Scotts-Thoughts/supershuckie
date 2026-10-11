#define JIT_ENABLED 1

#include "melonDS/src/NDS.h"
#include "melonDS/src/Platform.h"
#include "melonDS/src/WifiAP.h"
#include <cstdint>
#include <cstdlib>
#include <cstring>
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
    // Whether the state last loaded carried the SHUK section (see append_extra_section).
    bool loaded_state_had_extras = false;
};

// ---- State melonDS save states leave out ------------------------------------------------------
//
// melonDS save states do not carry the firmware flash, which games write to: every Nintendo
// Wi-Fi Connection game this was checked with (Pokémon HeartGold, Platinum, Black/White and
// Black 2/White 2) stores a WFC ID there a moment into every boot that finds none. A console reset
// inside a replay then boots on whatever firmware the playing console happens to have, and the
// game takes a different path from the recording's when that differs. Nor do they carry the
// divider and square root units' operands and results, which a game can read back after a load,
// the state of the access point melonDS simulates (whose beacons a game with the wireless on
// receives), or the CPUs' IRQ lines (recomputed from IME/IE/IF on load below).
//
// Every state made here therefore ends with one more section, SHUK, carrying them: of the
// firmware, its Wi-Fi and user settings area (what melonDS itself hands Platform::WriteFirmware).
// melonDS finds sections by name, so its loader (and an older build of this interface) skips it;
// a state without it (made before) loads as before and leaves all of that alone.
//
// SHUK payload, little endian; a later version may only append:
//   u32 version (1), u32 firmware offset, u32 firmware length, firmware bytes,
//   u32 DivNumerator[2], DivDenominator[2], DivQuotient[2], DivRemainder[2], SqrtVal[2], SqrtRes,
//   access point: u64 USCounter, u16 SeqNo, u8 BeaconDue, u8 0, u32 PacketLen, u32 RXNum,
//   u32 ClientStatus, u8 PacketBuffer[2048]
static constexpr char EXTRA_SECTION_MAGIC[4] = {'S', 'H', 'U', 'K'};
static constexpr u32 EXTRA_SECTION_VERSION = 1;
static constexpr std::size_t STATE_HEADER_SIZE = 0x10;
static constexpr std::size_t SECTION_HEADER_SIZE = 0x10;
static constexpr std::size_t EXTRA_REGISTER_WORDS = 11;
static constexpr std::size_t ACCESS_POINT_PACKET_SIZE = 2048;
static constexpr std::size_t ACCESS_POINT_STATE_SIZE = 8 + 2 + 1 + 1 + 4 + 4 + 4 + ACCESS_POINT_PACKET_SIZE;

// The divider and square root registers are protected members of NDS; pointers to them formed in
// a derived class reach them without changing melonDS.
struct NDSHiddenRegisters : NDS {
    static u32 *div_numerator(NDS &nds) { return nds.*(&NDSHiddenRegisters::DivNumerator); }
    static u32 *div_denominator(NDS &nds) { return nds.*(&NDSHiddenRegisters::DivDenominator); }
    static u32 *div_quotient(NDS &nds) { return nds.*(&NDSHiddenRegisters::DivQuotient); }
    static u32 *div_remainder(NDS &nds) { return nds.*(&NDSHiddenRegisters::DivRemainder); }
    static u32 *sqrt_val(NDS &nds) { return nds.*(&NDSHiddenRegisters::SqrtVal); }
    static u32 &sqrt_res(NDS &nds) { return nds.*(&NDSHiddenRegisters::SqrtRes); }
};

// The registers in SHUK order.
static void hidden_registers(NDS &nds, u32 *(&words)[EXTRA_REGISTER_WORDS]) {
    u32 *div_numerator = NDSHiddenRegisters::div_numerator(nds);
    u32 *div_denominator = NDSHiddenRegisters::div_denominator(nds);
    u32 *div_quotient = NDSHiddenRegisters::div_quotient(nds);
    u32 *div_remainder = NDSHiddenRegisters::div_remainder(nds);
    u32 *sqrt_val = NDSHiddenRegisters::sqrt_val(nds);
    u32 *const all[EXTRA_REGISTER_WORDS] = {
        &div_numerator[0], &div_numerator[1], &div_denominator[0], &div_denominator[1],
        &div_quotient[0], &div_quotient[1], &div_remainder[0], &div_remainder[1],
        &sqrt_val[0], &sqrt_val[1], &NDSHiddenRegisters::sqrt_res(nds),
    };
    for (std::size_t i = 0; i < EXTRA_REGISTER_WORDS; i++) {
        words[i] = all[i];
    }
}

// The access point's state is private to WifiAP (and Wifi's pointer to it, to Wifi). An explicit
// template instantiation may name private members, so one hands out pointers to them.
template <typename Tag, typename Tag::type Member>
struct PrivateMember {
    friend typename Tag::type member_pointer(Tag) { return Member; }
};
#define SUPERSHUCKIE_PRIVATE_MEMBER(tag, member_type, member) \
    struct tag {                                              \
        using type = member_type;                             \
        friend type member_pointer(tag);                      \
    };                                                        \
    template struct PrivateMember<tag, &member>;
SUPERSHUCKIE_PRIVATE_MEMBER(WifiAccessPointMember, class WifiAP *Wifi::*, Wifi::WifiAP)
SUPERSHUCKIE_PRIVATE_MEMBER(AccessPointUSCounter, u64 WifiAP::*, WifiAP::USCounter)
SUPERSHUCKIE_PRIVATE_MEMBER(AccessPointSeqNo, u16 WifiAP::*, WifiAP::SeqNo)
SUPERSHUCKIE_PRIVATE_MEMBER(AccessPointBeaconDue, bool WifiAP::*, WifiAP::BeaconDue)
SUPERSHUCKIE_PRIVATE_MEMBER(AccessPointPacketBuffer, u8 (WifiAP::*)[ACCESS_POINT_PACKET_SIZE], WifiAP::PacketBuffer)
SUPERSHUCKIE_PRIVATE_MEMBER(AccessPointPacketLen, int WifiAP::*, WifiAP::PacketLen)
SUPERSHUCKIE_PRIVATE_MEMBER(AccessPointRXNum, int WifiAP::*, WifiAP::RXNum)
SUPERSHUCKIE_PRIVATE_MEMBER(AccessPointClientStatus, int WifiAP::*, WifiAP::ClientStatus)
#undef SUPERSHUCKIE_PRIVATE_MEMBER

static WifiAP &access_point(NDS &nds) {
    return *(nds.Wifi.*member_pointer(WifiAccessPointMember{}));
}

static u32 read_u32(const u8 *p) {
    u32 v;
    std::memcpy(&v, p, sizeof(v));
    return v;
}

static void write_u32(u8 *p, u32 v) {
    std::memcpy(p, &v, sizeof(v));
}

static void save_access_point(WifiAP &ap, u8 *out) {
    const u64 us_counter = ap.*member_pointer(AccessPointUSCounter{});
    const u16 seq_no = ap.*member_pointer(AccessPointSeqNo{});
    std::memcpy(out, &us_counter, 8);
    std::memcpy(out + 8, &seq_no, 2);
    out[10] = (ap.*member_pointer(AccessPointBeaconDue{})) ? 1 : 0;
    out[11] = 0;
    write_u32(out + 12, static_cast<u32>(ap.*member_pointer(AccessPointPacketLen{})));
    write_u32(out + 16, static_cast<u32>(ap.*member_pointer(AccessPointRXNum{})));
    write_u32(out + 20, static_cast<u32>(ap.*member_pointer(AccessPointClientStatus{})));
    std::memcpy(out + 24, ap.*member_pointer(AccessPointPacketBuffer{}), ACCESS_POINT_PACKET_SIZE);
}

static void load_access_point(WifiAP &ap, const u8 *in) {
    std::memcpy(&(ap.*member_pointer(AccessPointUSCounter{})), in, 8);
    std::memcpy(&(ap.*member_pointer(AccessPointSeqNo{})), in + 8, 2);
    ap.*member_pointer(AccessPointBeaconDue{}) = in[10] != 0;
    const u32 packet_len = read_u32(in + 12);
    ap.*member_pointer(AccessPointPacketLen{}) = static_cast<int>(packet_len <= ACCESS_POINT_PACKET_SIZE ? packet_len : 0);
    ap.*member_pointer(AccessPointRXNum{}) = static_cast<int>(read_u32(in + 16));
    ap.*member_pointer(AccessPointClientStatus{}) = static_cast<int>(read_u32(in + 20));
    std::memcpy(ap.*member_pointer(AccessPointPacketBuffer{}), in + 24, ACCESS_POINT_PACKET_SIZE);
}

// The firmware area SHUK carries: the Wi-Fi access points through the end of the image. False
// when the image's header points somewhere unusable.
static bool firmware_settings_area(const Firmware &firmware, u32 &offset, u32 &length) {
    offset = firmware.GetWifiAccessPointOffset();
    if (offset < STATE_HEADER_SIZE || offset >= firmware.Length()) {
        return false;
    }
    length = firmware.Length() - offset;
    return true;
}

// Append SHUK to the finished state in data[0, length) (room for `capacity` bytes) and fix up the
// state's total length; returns the new length, or 0 when it does not fit.
static std::size_t append_extra_section(NDS &nds, u8 *data, std::size_t length, std::size_t capacity) {
    const Firmware &firmware = nds.SPI.GetFirmware();
    u32 fw_offset = 0, fw_length = 0;
    if (!firmware_settings_area(firmware, fw_offset, fw_length)) {
        fw_offset = 0;
        fw_length = 0;
    }
    const std::size_t section = SECTION_HEADER_SIZE + 12 + fw_length + EXTRA_REGISTER_WORDS * 4 + ACCESS_POINT_STATE_SIZE;
    if (length < STATE_HEADER_SIZE || length + section > capacity || length + section > UINT32_MAX) {
        return 0;
    }

    u8 *out = data + length;
    std::memcpy(out, EXTRA_SECTION_MAGIC, 4);
    write_u32(out + 4, static_cast<u32>(section));
    std::memset(out + 8, 0, 8);
    out += SECTION_HEADER_SIZE;
    write_u32(out, EXTRA_SECTION_VERSION);
    write_u32(out + 4, fw_offset);
    write_u32(out + 8, fw_length);
    out += 12;
    std::memcpy(out, firmware.Buffer() + fw_offset, fw_length);
    out += fw_length;
    u32 *words[EXTRA_REGISTER_WORDS];
    hidden_registers(nds, words);
    for (u32 *word : words) {
        write_u32(out, *word);
        out += 4;
    }
    save_access_point(access_point(nds), out);

    write_u32(data + 0x08, static_cast<u32>(length + section));
    return length + section;
}

// The payload of section `magic` of the melonDS state in data[0, size), or nullptr.
static const u8 *find_state_section(const u8 *data, std::size_t size, const char (&magic)[4], std::size_t &payload_length) {
    std::size_t offset = STATE_HEADER_SIZE;
    while (offset + SECTION_HEADER_SIZE <= size) {
        const std::size_t length = read_u32(data + offset + 4);
        if (length < SECTION_HEADER_SIZE || length > size - offset) {
            return nullptr;
        }
        if (std::memcmp(data + offset, magic, 4) == 0) {
            payload_length = length - SECTION_HEADER_SIZE;
            return data + offset + SECTION_HEADER_SIZE;
        }
        offset += length;
    }
    return nullptr;
}

// A well-formed SHUK payload.
struct ExtraSection {
    u32 fw_offset = 0;
    u32 fw_length = 0;
    const u8 *firmware = nullptr;
    const u8 *registers = nullptr;
    const u8 *access_point = nullptr;
};

static bool parse_extra_section(const u8 *data, std::size_t size, ExtraSection &out) {
    std::size_t length = 0;
    const u8 *payload = find_state_section(data, size, EXTRA_SECTION_MAGIC, length);
    if (payload == nullptr || length < 12 || read_u32(payload) < 1) {
        return false;
    }
    const std::size_t fw_length = read_u32(payload + 8);
    if (fw_length > length - 12 || length - 12 - fw_length < EXTRA_REGISTER_WORDS * 4 + ACCESS_POINT_STATE_SIZE) {
        return false;
    }
    out.fw_offset = read_u32(payload + 4);
    out.fw_length = static_cast<u32>(fw_length);
    out.firmware = payload + 12;
    out.registers = out.firmware + fw_length;
    out.access_point = out.registers + EXTRA_REGISTER_WORDS * 4;
    return true;
}

// Whether `section`'s firmware bytes are exactly this firmware image's settings area.
static bool firmware_layout_matches(const Firmware &firmware, const ExtraSection &section) {
    u32 offset = 0, length = 0;
    return firmware_settings_area(firmware, offset, length) && offset == section.fw_offset && length == section.fw_length;
}

// After melonDS loaded the state in data[0, size): put back what SHUK carries and recompute the
// IRQ lines (melonDS only updates them when IME/IE/IF change, so a load otherwise leaves the
// loading console's own). Returns whether the state had SHUK.
static bool finish_load(NDS &nds, const u8 *data, std::size_t size) {
    nds.UpdateIRQ(0);
    nds.UpdateIRQ(1);

    ExtraSection section;
    if (!parse_extra_section(data, size, section)) {
        return false;
    }
    u32 *words[EXTRA_REGISTER_WORDS];
    hidden_registers(nds, words);
    for (std::size_t i = 0; i < EXTRA_REGISTER_WORDS; i++) {
        *words[i] = read_u32(section.registers + 4 * i);
    }
    load_access_point(access_point(nds), section.access_point);
    Firmware &firmware = nds.SPI.GetFirmware();
    if (firmware_layout_matches(firmware, section)) {
        std::memcpy(firmware.Buffer() + section.fw_offset, section.firmware, section.fw_length);
    }
    return true;
}

// Where a SHUK section that ends the state in data[0, size) starts, or 0.
static std::size_t trailing_extra_section(const u8 *data, std::size_t size) {
    std::size_t offset = STATE_HEADER_SIZE;
    while (offset + SECTION_HEADER_SIZE <= size) {
        const std::size_t length = read_u32(data + offset + 4);
        if (length < SECTION_HEADER_SIZE || length > size - offset) {
            return 0;
        }
        if (std::memcmp(data + offset, EXTRA_SECTION_MAGIC, 4) == 0 && offset + length == size) {
            return offset;
        }
        offset += length;
    }
    return 0;
}

// Load the state in data[0, size) into the console, leaving data as it was. False, with the
// console untouched, when melonDS rejects the state's header.
//
// melonDS writes into a state it loads: Savestate::Finish stores the offset it has read up to in
// the header's length field, as it does when saving. For a state of melonDS's own that is the
// length the field already holds; for one ending in SHUK, which melonDS does not read, it is where
// SHUK starts, and the buffer would no longer load (melonDS checks the field against the buffer's
// length): the replay player's keyframe buffer, which it folds the next keyframes onto, or a save
// state loaded again after its preview frame. So the field is put back after every load. The first
// build with SHUK did not do that and recorded such states as they were left (a state loaded while
// recording), so a field saying exactly where a trailing SHUK starts is taken as the whole length.
static bool load_state(NDS &nds, void *data, std::size_t size) {
    u8 *bytes = static_cast<u8 *>(data);
    if (size < STATE_HEADER_SIZE || size > UINT32_MAX) {
        return false;
    }
    u8 length_field[4];
    std::memcpy(length_field, bytes + 0x08, 4);
    const std::size_t extra = trailing_extra_section(bytes, size);
    if (extra != 0 && read_u32(length_field) == extra) {
        write_u32(bytes + 0x08, static_cast<u32>(size));
    }

    bool loaded = false;
    {
        Savestate state(data, static_cast<u32>(size), false);
        // Past a rejected header, NDS::DoSavestate carries on with nothing read (its config check
        // compares an uninitialized word) and leaves the console half loaded: the 3D render list
        // full of null polygons, among others, which the render thread then draws.
        if (!state.Error) {
            loaded = nds.DoSavestate(&state) && !state.Error;
        }
    }
    std::memcpy(bytes + 0x08, length_field, 4);
    return loaded;
}

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

    if(!success || state.Error) {
        return 0;
    }

    return append_extra_section(*core->nds, static_cast<u8 *>(data), state.Length(), data_size);
}

extern "C" bool melonds_rs_core_load_save_state(MelonDSCoreHolder *core, void *data, std::size_t data_size) {
    core->discarded = {};
    if (!load_state(*core->nds, data, data_size)) {
        return false;
    }
    core->loaded_state_had_extras = finish_load(*core->nds, static_cast<const u8 *>(data), data_size);
    return true;
}

// Load a state whose polygon and vertex RAM may be a stale copy (see DiscardedGeometry): nothing
// is drawn from the polygons it restores. The flag is consulted inside DoSavestate, before the
// render thread is restarted on the loaded render list.
extern "C" bool melonds_rs_core_load_save_state_discarding_geometry(MelonDSCoreHolder *core, void *data, std::size_t data_size) {
    GPU3D &gpu3d = core->nds->GPU.GPU3D;
    gpu3d.DiscardGeometryOnLoad = true;
    bool loaded = load_state(*core->nds, data, data_size);
    gpu3d.DiscardGeometryOnLoad = false;

    DiscardedGeometry &d = core->discarded;
    d = {};
    if (loaded) {
        core->loaded_state_had_extras = finish_load(*core->nds, static_cast<const u8 *>(data), data_size);
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

// ---- Replays recorded before states carried SHUK ----------------------------------------------

// A WFC ID as the games store it (access points 1 and 2, bytes 0xF5..0xFD): the one HeartGold
// wrote on melonDS's default firmware. Any valid ID serves; games only keep a copy of it in RAM.
static constexpr u8 STANDARD_WFC_ID[9] = {0x48, 0xC9, 0x37, 0xF3, 0xCE, 0x9E, 0x49, 0x5B, 0x9A};
static constexpr std::size_t WFC_ID_OFFSET = 0xF5;

// Frames into a console's life by which every game checked has written its WFC ID to firmware
// that had none (Black 2/White 2 at 32, Black/White at 36, Platinum at 131, HeartGold at 186).
static constexpr u32 WFC_ID_WRITTEN_BY_FRAME = 600;

// The WFC ID area of an access point block: bytes 0xF0..0xFD (the ID and what follows it, up to
// the checksum).
static constexpr std::size_t WFC_ID_AREA = 0xF0;
static constexpr std::size_t WFC_ID_AREA_LENGTH = 0xFE - WFC_ID_AREA;

static bool has_wfc_id(const Firmware &firmware) {
    const auto &access_points = firmware.GetAccessPoints();
    for (std::size_t i = 0; i < 2; i++) {
        for (std::size_t b = WFC_ID_AREA; b < WFC_ID_AREA + WFC_ID_AREA_LENGTH; b++) {
            if (access_points[i].Bytes[b] != 0) {
                return true;
            }
        }
    }
    return false;
}

// The WFC ID area of a copy of a firmware access point block holding an ID in the console's main
// RAM (checksum and all, as read from the firmware: Black/White and Black 2/White 2 keep the
// blocks they read), or nullptr.
static const u8 *find_wfc_id_in_ram(const NDS &nds) {
    const u8 *ram = nds.MainRAM;
    const std::size_t size = static_cast<std::size_t>(nds.MainRAMMask) + 1;
    for (std::size_t at = 0; at + sizeof(Firmware::WifiAccessPoint) <= size; at += 4) {
        const u8 *block = ram + at;
        // ConnectionConfigured, Status (normal or not configured)
        if (block[0xEF] != 0x01 || (block[0xE7] != 0x00 && block[0xE7] != 0xFF)) {
            continue;
        }
        bool any = false;
        for (std::size_t b = WFC_ID_AREA; b < WFC_ID_AREA + WFC_ID_AREA_LENGTH; b++) {
            any |= block[b] != 0;
        }
        if (any && CRC16(block, 0xFE, 0x0000) == static_cast<u16>(block[0xFE] | (block[0xFF] << 8))) {
            return block + WFC_ID_AREA;
        }
    }
    return nullptr;
}

// Write a WFC ID area the way the games write their IDs: into access points 1 and 2 (with their
// checksums), clearing the block after access point 3.
static void write_wfc_id(Firmware &firmware, const u8 *id_area) {
    auto &access_points = firmware.GetAccessPoints();
    for (std::size_t i = 0; i < 2; i++) {
        std::memcpy(access_points[i].Bytes + WFC_ID_AREA, id_area, WFC_ID_AREA_LENGTH);
        access_points[i].UpdateChecksum();
    }
    std::memset(firmware.GetWifiAccessPointPosition() + 3 * sizeof(Firmware::WifiAccessPoint), 0, 0x100);
}

// Replay playback, right after loading one of the replay's keyframes. A keyframe recorded before
// states carried SHUK says nothing about the firmware of the console that recorded it. One taken
// well after the game booted was taken on firmware that held a WFC ID by then (the game writes one
// a moment into every boot that finds none), so give this console's firmware one too: otherwise
// the replay's next console reset makes the game write one here, which the recording's did not,
// and playback parts ways with it. melonDS's frame count survives resets, so the keyframe's own
// says how long the recording console had been running. When the game keeps a copy of the
// firmware's block in RAM, that is the recording's very ID, and the firmware gets it even if it
// had another one; otherwise any ID serves (the games only keep it to copy it around).
//
// Returns whether the firmware was changed.
extern "C" bool melonds_rs_core_adopt_replay_keyframe(MelonDSCoreHolder *core) {
    NDS &nds = *core->nds;
    if (core->loaded_state_had_extras) {
        return false;
    }
    Firmware &firmware = nds.SPI.GetFirmware();
    u32 offset = 0, length = 0;
    if (!firmware_settings_area(firmware, offset, length) || length < 0x400) {
        return false;
    }

    const u8 *recorded = find_wfc_id_in_ram(nds);
    if (recorded == nullptr && nds.NumFrames < WFC_ID_WRITTEN_BY_FRAME) {
        return false;
    }
    u8 id_area[WFC_ID_AREA_LENGTH] = {};
    if (recorded != nullptr) {
        std::memcpy(id_area, recorded, WFC_ID_AREA_LENGTH);
    }
    else if (has_wfc_id(firmware)) {
        return false;
    }
    else {
        std::memcpy(id_area + (WFC_ID_OFFSET - WFC_ID_AREA), STANDARD_WFC_ID, sizeof(STANDARD_WFC_ID));
    }

    const auto &access_points = firmware.GetAccessPoints();
    if (std::memcmp(access_points[0].Bytes + WFC_ID_AREA, id_area, WFC_ID_AREA_LENGTH) == 0
        && std::memcmp(access_points[1].Bytes + WFC_ID_AREA, id_area, WFC_ID_AREA_LENGTH) == 0) {
        return false;
    }
    write_wfc_id(firmware, id_area);
    return true;
}

// Bits of melonds_rs_core_check_replay_keyframe's result.
enum ReplayKeyframeCheck : std::uint32_t {
    // The keyframe carries SHUK and its firmware area differed from the console's, which now has
    // the keyframe's.
    REPLAY_KEYFRAME_FIRMWARE_SYNCED = 1,
    // The firmware chip's interface state differs from the keyframe's (see below).
    REPLAY_KEYFRAME_FIRMWARE_ACCESS_DIFFERS = 2,
};

// SPFW payload (FirmwareMem::DoSavestate): Hold (4), CurCmd (1), DataPos (4), Data (1),
// StatusReg (1), Addr (4).
static constexpr char SPFW_MAGIC[4] = {'S', 'P', 'F', 'W'};
static constexpr std::size_t SPFW_LENGTH = 15;
static constexpr std::size_t SPFW_DATA_OFFSET = 9;

// Replay playback reached one of the replay's keyframes, the state in data[0, size), by playing
// rather than by loading it. Brings the console's firmware in line with a keyframe that carries
// it, and compares the firmware chip's interface state (the command in progress and the address it
// reached; not the last byte transferred, which can be part of a WFC ID, and those may differ
// harmlessly) with the keyframe's. That state only changes when the game uses the firmware, so a
// difference means the game used it differently from the recording since they last matched: a
// boot found a WFC ID where the recording's wrote one, or the other way round. Nothing short of
// loading the keyframe makes up for that; the caller does. Returns ReplayKeyframeCheck bits.
extern "C" std::uint32_t melonds_rs_core_check_replay_keyframe(MelonDSCoreHolder *core, const u8 *data, std::size_t size) {
    NDS &nds = *core->nds;
    std::uint32_t result = 0;

    ExtraSection section;
    Firmware &firmware = nds.SPI.GetFirmware();
    if (parse_extra_section(data, size, section) && firmware_layout_matches(firmware, section)) {
        u8 *area = firmware.Buffer() + section.fw_offset;
        if (std::memcmp(area, section.firmware, section.fw_length) != 0) {
            std::memcpy(area, section.firmware, section.fw_length);
            result |= REPLAY_KEYFRAME_FIRMWARE_SYNCED;
        }
    }

    std::size_t recorded_length = 0;
    const u8 *recorded = find_state_section(data, size, SPFW_MAGIC, recorded_length);
    u8 buffer[STATE_HEADER_SIZE + SECTION_HEADER_SIZE + 2 * SPFW_LENGTH];
    Savestate live_state(buffer, sizeof(buffer), true);
    nds.SPI.GetFirmwareMem()->DoSavestate(&live_state);
    live_state.Finish();
    std::size_t live_length = 0;
    const u8 *live = live_state.Error ? nullptr : find_state_section(buffer, live_state.Length(), SPFW_MAGIC, live_length);
    if (recorded != nullptr && live != nullptr && recorded_length == SPFW_LENGTH && live_length == SPFW_LENGTH) {
        for (std::size_t i = 0; i < SPFW_LENGTH; i++) {
            if (i != SPFW_DATA_OFFSET && recorded[i] != live[i]) {
                result |= REPLAY_KEYFRAME_FIRMWARE_ACCESS_DIFFERS;
                break;
            }
        }
    }
    return result;
}

// Diagnostics (desync hunting): CPU-visible state that melonDS save states do not carry, read
// without side effects. Writes up to `max` values to `out` and returns how many there are; the
// order is documented on the Rust side (melonds_rs::Core::debug_unsaved_state).
extern "C" std::size_t melonds_rs_core_debug_unsaved_state(MelonDSCoreHolder *core, std::uint32_t *out, std::size_t max) {
    NDS &nds = *core->nds;
    const std::uint32_t values[] = {
        nds.ARM9.IRQ, nds.ARM7.IRQ, nds.ARM9.Halted, nds.ARM7.Halted,
        nds.ARM9IORead32(0x04000208), nds.ARM9IORead32(0x04000210), nds.ARM9IORead32(0x04000214),
        nds.ARM7IORead32(0x04000208), nds.ARM7IORead32(0x04000210), nds.ARM7IORead32(0x04000214),
        nds.ARM9IORead32(0x04000280),
        nds.ARM9IORead32(0x04000290), nds.ARM9IORead32(0x04000294),
        nds.ARM9IORead32(0x04000298), nds.ARM9IORead32(0x0400029C),
        nds.ARM9IORead32(0x040002A0), nds.ARM9IORead32(0x040002A4),
        nds.ARM9IORead32(0x040002A8), nds.ARM9IORead32(0x040002AC),
        nds.ARM9IORead32(0x040002B0), nds.ARM9IORead32(0x040002B4),
        nds.ARM9IORead32(0x040002B8), nds.ARM9IORead32(0x040002BC),
        nds.KeyInput,
    };
    const std::size_t count = sizeof(values) / sizeof(values[0]);
    for (std::size_t i = 0; i < count && i < max; i++) {
        out[i] = values[i];
    }
    return count;
}

// Diagnostics: the firmware flash image, which games can write to and save states do not carry.
extern "C" const u8 *melonds_rs_core_debug_firmware(MelonDSCoreHolder *core, std::size_t *length) {
    const Firmware &firmware = core->nds->SPI.GetFirmware();
    *length = firmware.Length();
    return firmware.Buffer();
}

// Diagnostics: overwrite bytes of the firmware flash image (bounds-checked).
extern "C" void melonds_rs_core_debug_write_firmware(MelonDSCoreHolder *core, std::size_t offset, const u8 *data, std::size_t length) {
    Firmware &firmware = core->nds->SPI.GetFirmware();
    for (std::size_t i = 0; i < length && offset + i < firmware.Length(); i++) {
        firmware.Buffer()[offset + i] = data[i];
    }
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
