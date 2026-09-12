#ifndef __SUPERSHUCKIE_AUDIO_OUTPUT_HPP__
#define __SUPERSHUCKIE_AUDIO_OUTPUT_HPP__

#include <cstdint>
#include <string>
#include <vector>
#include <SDL3/SDL.h>
#include <supershuckie/supershuckie.h>

namespace SuperShuckie64 {

/**
 * The SDL side of audio playback: a device stream that pulls from the frontend's audio ring on
 * SDL's audio thread, so a busy or blocked GUI thread cannot cause dropouts.
 *
 * Nothing here touches the frontend object; the ring handle is the only shared state, and it
 * outlives every emulator core.
 */
class AudioOutput {
public:
    /** Takes ownership of a handle from supershuckie_frontend_retain_audio_output(). */
    explicit AudioOutput(SuperShuckieAudioOutputRaw *ring);
    ~AudioOutput();

    AudioOutput(const AudioOutput &) = delete;
    AudioOutput &operator=(const AudioOutput &) = delete;

    /** Open the default playback device. On failure, returns false and last_error() says why. */
    bool open();
    void close();
    bool is_open() const noexcept { return this->stream != nullptr; }
    const std::string &last_error() const noexcept { return this->error; }

    /** 0.0 (silent) to 1.0. */
    void set_gain(float gain);

    /** Playback rate multiplier (1.0 = normal); used to keep up with sped-up emulation. */
    void set_frequency_ratio(float ratio);

    /** Drop whatever is queued in the device stream and the ring. */
    void clear();

    /** Current emulation speed multiplier, as the emulator last published it. */
    float emulation_speed() const noexcept;

    /** Whether sped-up playback needs the frequency ratio raised to keep up (see the C header). */
    bool fast_forward_scales_pitch() const noexcept;

private:
    SuperShuckieAudioOutputRaw *ring;
    SDL_AudioStream *stream = nullptr;
    std::vector<std::int16_t> scratch;
    std::string error;
    float gain = 1.0f;
    float frequency_ratio = 1.0f;

    static void SDLCALL get_callback(void *userdata, SDL_AudioStream *stream, int additional_amount, int total_amount);
};

}

#endif
