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

static std::int64_t now_microseconds() noexcept {
    return std::chrono::duration_cast<std::chrono::microseconds>(std::chrono::steady_clock::now().time_since_epoch()).count();
}

std::uint32_t DisplaySyncThread::acknowledge() noexcept {
    auto count = this->refresh_count.load(std::memory_order_relaxed);
    auto passed = count - this->acknowledged_refresh_count;
    this->acknowledged_refresh_count = count;
    this->outstanding.store(false, std::memory_order_release);
    return passed > UINT32_MAX ? UINT32_MAX : static_cast<std::uint32_t>(passed);
}

std::int64_t DisplaySyncThread::microseconds_since_vblank() const noexcept {
    return now_microseconds() - this->last_vblank_microseconds.load(std::memory_order_relaxed);
}

std::int64_t DisplaySyncThread::refresh_period_microseconds() const noexcept {
    return this->period_microseconds.load(std::memory_order_relaxed);
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
    this->period_microseconds.store(fallback_period.count(), std::memory_order_relaxed);

    while(!this->stopping.load(std::memory_order_relaxed)) {
        if(!wait_for_vblank()) {
            std::this_thread::sleep_for(fallback_period);
        }
        if(this->stopping.load(std::memory_order_relaxed)) {
            break;
        }
        this->last_vblank_microseconds.store(now_microseconds(), std::memory_order_relaxed);
        this->refresh_count.fetch_add(1, std::memory_order_relaxed);

        // Skip this refresh if the GUI thread has not yet presented the previous one.
        bool expected = false;
        if(this->outstanding.compare_exchange_strong(expected, true, std::memory_order_acq_rel)) {
            emit this->vblank();
        }
    }
}
