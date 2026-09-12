#include <stdio.h>

#include <SDL3/SDL.h>
#include <QApplication>

#ifdef _WIN32
#include <QStyleFactory>
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

#include "main_window.hpp"
#include "theme.hpp"

int main(int argc, char **argv) {
#ifdef _WIN32
    // An emulator running at 4x while another window (OBS, a browser) has focus is exactly
    // what Windows 11 likes to demote to efficiency cores and lower clocks; opt the process out.
    // The core and 3D render threads additionally opt themselves out and raise their priority.
    {
        PROCESS_POWER_THROTTLING_STATE state{};
        state.Version = PROCESS_POWER_THROTTLING_CURRENT_VERSION;
        state.ControlMask = PROCESS_POWER_THROTTLING_EXECUTION_SPEED;
        state.StateMask = 0;
        SetProcessInformation(GetCurrentProcess(), ProcessPowerThrottling, &state, sizeof(state));
    }
#endif

    // ~10.7 ms device period at 48 kHz; the frontend's own audio ring is the real latency knob.
    SDL_SetHint(SDL_HINT_AUDIO_DEVICE_SAMPLE_FRAMES, "512");
    SDL_Init(SDL_INIT_EVENTS | SDL_INIT_GAMEPAD | SDL_INIT_VIDEO | SDL_INIT_AUDIO);

    QCoreApplication::setOrganizationName("SnowyMouse");
    QCoreApplication::setApplicationName("SuperShuckie");

    QApplication app(argc, argv);

    SixShooter::Theme theme;

    SuperShuckie64::MainWindow window;
    window.show();

    if(argc == 2) {
        window.load_rom(argv[1]);
    }

    int result = app.exec();
    SDL_Quit();

    return result;
}
