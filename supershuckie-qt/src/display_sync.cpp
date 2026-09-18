#include "display_sync.hpp"

#include <QGuiApplication>
#include <QScreen>
#include <chrono>
#include <thread>

#ifdef _WIN32
#include <windows.h>
#include <dwmapi.h>
#endif

DisplaySyncThread::DisplaySyncThread(QObject *parent) : QThread(parent) {}

DisplaySyncThread::~DisplaySyncThread() {
    this->stop();
}

void DisplaySyncThread::stop() {
    this->stopping.store(true, std::memory_order_relaxed);
    if(this->isRunning()) {
        this->wait();
    }
}

void DisplaySyncThread::acknowledge() noexcept {
    this->outstanding.store(false, std::memory_order_release);
}

bool DisplaySyncThread::wait_for_vblank() {
#ifdef _WIN32
    // Blocks until the desktop compositor's next present, i.e. the next refresh of the display.
    return SUCCEEDED(DwmFlush());
#else
    return false;
#endif
}

void DisplaySyncThread::run() {
    // Fallback when the compositor cannot be waited on: sleep one refresh period. This does not
    // lock to the display, but still presents at a steady rate.
    auto fallback_period = std::chrono::microseconds(16667);
    if(auto *screen = QGuiApplication::primaryScreen()) {
        double hz = screen->refreshRate();
        if(hz > 1.0) {
            fallback_period = std::chrono::microseconds(static_cast<long long>(1000000.0 / hz));
        }
    }

    while(!this->stopping.load(std::memory_order_relaxed)) {
        if(!wait_for_vblank()) {
            std::this_thread::sleep_for(fallback_period);
        }
        if(this->stopping.load(std::memory_order_relaxed)) {
            break;
        }
        // Skip this refresh if the GUI thread has not yet presented the previous one.
        bool expected = false;
        if(this->outstanding.compare_exchange_strong(expected, true, std::memory_order_acq_rel)) {
            emit this->vblank();
        }
    }
}
