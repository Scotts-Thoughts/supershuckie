#ifndef SUPERSHUCKIE_DISPLAY_SYNC_HPP
#define SUPERSHUCKIE_DISPLAY_SYNC_HPP

#include <QThread>
#include <atomic>
#include <cstdint>

/**
 * Wakes once per display refresh and asks the main window to present the newest emulated frame.
 *
 * Without this the main window hands frames to the screen the moment they arrive, and the
 * emulator's 60.0024 frames per second drift against the monitor's refresh: whenever the two
 * clocks line up badly, frames alternately show twice and get skipped for a stretch. Waiting for
 * the compositor's vertical blank (DwmFlush on Windows; a refresh-period sleep elsewhere) and
 * presenting once per refresh keeps the cadence even.
 *
 * The `vblank` signal is emitted from this thread; connect it queued to a slot on the GUI thread.
 * At most one signal is outstanding at a time, so a slow GUI thread drops wakeups instead of
 * accumulating a backlog of stale ones.
 */
class DisplaySyncThread : public QThread {
    Q_OBJECT

public:
    explicit DisplaySyncThread(QObject *parent = nullptr);
    ~DisplaySyncThread() override;

    /** Ask the thread to finish and wait for it (at most about one refresh). */
    void stop();

    /**
     * The GUI thread calls this once it has handled a `vblank`, allowing the next one. Returns
     * how many refreshes passed since the last call: 1 normally, more if the GUI thread was too
     * busy to be woken for some.
     */
    std::uint32_t acknowledge() noexcept;

    /** Microseconds since the most recent refresh this thread woke for. */
    std::int64_t microseconds_since_vblank() const noexcept;

    /** Length of one refresh in microseconds, as reported by the primary screen. */
    std::int64_t refresh_period_microseconds() const noexcept;

signals:
    void vblank();

protected:
    void run() override;

private:
    std::atomic<bool> stopping { false };
    std::atomic<bool> outstanding { false };
    std::atomic<std::uint64_t> refresh_count { 0 };
    std::atomic<std::int64_t> last_vblank_microseconds { 0 };
    std::atomic<std::int64_t> period_microseconds { 16667 };
    std::uint64_t acknowledged_refresh_count = 0; // GUI thread only

    /** Block until the display's next refresh. Returns false if that is not possible here. */
    static bool wait_for_vblank();
};

#endif
