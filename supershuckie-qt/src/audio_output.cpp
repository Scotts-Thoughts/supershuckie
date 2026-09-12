#include "audio_output.hpp"

#include <cstring>

namespace SuperShuckie64 {

AudioOutput::AudioOutput(SuperShuckieAudioOutputRaw *ring) : ring(ring) {
    // Big enough for any request SDL makes with the 512-frame device period main.cpp asks for
    // (it can ask for several periods at once after a stall); grown in the callback if not.
    this->scratch.resize(4096 * 2);
}

AudioOutput::~AudioOutput() {
    this->close();
    supershuckie_audio_output_release(this->ring);
}

bool AudioOutput::open() {
    if(this->stream != nullptr) {
        return true;
    }

    SDL_AudioSpec spec = {};
    spec.format = SDL_AUDIO_S16;
    spec.channels = 2;
    spec.freq = static_cast<int>(supershuckie_audio_output_sample_rate());

    this->stream = SDL_OpenAudioDeviceStream(SDL_AUDIO_DEVICE_DEFAULT_PLAYBACK, &spec, &AudioOutput::get_callback, this);
    if(this->stream == nullptr) {
        this->error = SDL_GetError();
        return false;
    }

    SDL_SetAudioStreamGain(this->stream, this->gain);
    SDL_SetAudioStreamFrequencyRatio(this->stream, this->frequency_ratio);

    if(!SDL_ResumeAudioStreamDevice(this->stream)) {
        this->error = SDL_GetError();
        SDL_DestroyAudioStream(this->stream);
        this->stream = nullptr;
        return false;
    }

    this->error.clear();
    return true;
}

void AudioOutput::close() {
    if(this->stream == nullptr) {
        return;
    }
    // Also closes the device this stream was opened with.
    SDL_DestroyAudioStream(this->stream);
    this->stream = nullptr;
}

void AudioOutput::set_gain(float gain) {
    this->gain = gain;
    if(this->stream != nullptr) {
        SDL_SetAudioStreamGain(this->stream, gain);
    }
}

void AudioOutput::set_frequency_ratio(float ratio) {
    this->frequency_ratio = ratio;
    if(this->stream != nullptr) {
        SDL_SetAudioStreamFrequencyRatio(this->stream, ratio);
    }
}

void AudioOutput::clear() {
    supershuckie_audio_output_clear(this->ring);
    if(this->stream != nullptr) {
        SDL_ClearAudioStream(this->stream);
    }
}

float AudioOutput::emulation_speed() const noexcept {
    return supershuckie_audio_output_speed(this->ring);
}

bool AudioOutput::fast_forward_scales_pitch() const noexcept {
    return supershuckie_audio_output_fast_forward_scales_pitch(this->ring);
}

// Runs on SDL's audio thread whenever the device wants more. Only the ring is touched here: no
// Qt, no frontend, and no allocation on the usual path.
void SDLCALL AudioOutput::get_callback(void *userdata, SDL_AudioStream *stream, int additional_amount, int) {
    auto *self = static_cast<AudioOutput *>(userdata);
    if(additional_amount <= 0) {
        return;
    }

    std::size_t frames = static_cast<std::size_t>(additional_amount) / (sizeof(std::int16_t) * 2);
    if(frames == 0) {
        return;
    }
    if(self->scratch.size() < frames * 2) {
        self->scratch.resize(frames * 2);
    }

    std::size_t got = supershuckie_audio_output_read(self->ring, self->scratch.data(), frames);
    if(got < frames) {
        // Underrun (paused, muted-while-sped-up, or the emulator fell behind): pad with silence.
        std::memset(self->scratch.data() + got * 2, 0, (frames - got) * 2 * sizeof(std::int16_t));
    }

    SDL_PutAudioStreamData(stream, self->scratch.data(), static_cast<int>(frames * 2 * sizeof(std::int16_t)));
}

}
