#ifndef SUPERSHUCKIE_DISPLAY_SYNC_HPP
#define SUPERSHUCKIE_DISPLAY_SYNC_HPP

#include <QThread>
#include <atomic>

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

    /** The GUI thread calls this once it has handled a `vblank`, allowing the next one. */
    void acknowledge() noexcept;

signals:
    void vblank();

protected:
    void run() override;

private:
    std::atomic<bool> stopping { false };
    std::atomic<bool> outstanding { false };

    /** Block until the display's next refresh. Returns false if that is not possible here. */
    static bool wait_for_vblank();
};

#endif
