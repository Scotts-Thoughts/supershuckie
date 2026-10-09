#ifndef __SUPERSHUCKIE_REPLAY_COMPARE_CONTROLLER_HPP__
#define __SUPERSHUCKIE_REPLAY_COMPARE_CONTROLLER_HPP__

#include <QJsonObject>
#include <QObject>
#include <QPoint>
#include <QWidget>
#include <cstdint>
#include <map>
#include <string>

#include <supershuckie/supershuckie.h>

class QLabel;

namespace SuperShuckie64 {

class MainWindow;
class ScreenCanvas;
class ReplayCompareController;

/**
 * A replay compared with the loaded one: its screens plus a status strip (name and ROM, whether
 * it is in sync, its timer and counters). Takes no keyboard input. Closing it stops comparing
 * that replay.
 */
class CompareWindow: public QWidget {
    Q_OBJECT
public:
    CompareWindow(ReplayCompareController *controller, std::uint16_t id, const QJsonObject &replay);

    std::uint16_t compare_id() const noexcept { return this->id; }
    ScreenCanvas *canvas() const noexcept { return this->screen; }

    /** Geometry key for this replay's window (by name, so it comes back where it was). */
    QString settings_key() const;

    /** Lay the screens out (from the compare_change_video_mode callback). */
    void set_layout(unsigned screen_count, const SuperShuckieScreenData *screen_data, unsigned scale);

    /** Refresh the status strip from the replay's and the comparison's JSON (see replay_compare.h). */
    void update(const QJsonObject &replay, const QJsonObject &state);

    void save_geometry();
    void restore_geometry();

    /** The window is going because the replay was removed (not closed by the user). */
    void forget() noexcept { this->removed = true; }

protected:
    void closeEvent(QCloseEvent *event) override;
    void contextMenuEvent(QContextMenuEvent *event) override;

private:
    ReplayCompareController *controller;
    std::uint16_t id;
    QString name;
    QString rom_name;
    ScreenCanvas *screen;
    QLabel *name_label;
    QLabel *sync_label;
    QLabel *time_label;
    QLabel *fps_label;
    bool laid_out = false;
    bool removed = false;
};

/**
 * Owns the compared replays' windows and the Replays menu's "Compare…" flows (choosing the
 * replay, locating its ROM, confirming a mismatch). Ticked from the main window's tick.
 */
class ReplayCompareController: public QObject {
    Q_OBJECT
public:
    explicit ReplayCompareController(MainWindow *main_window);
    ~ReplayCompareController();

    MainWindow *main_window() const noexcept { return this->main; }

    /** Called every tick; cheap unless something changed. */
    void tick();

    /** Menu entry points: one of the ROM's replays, any replay file, stop comparing all. */
    void add_from_list();
    void add_from_file();
    void clear();

    /** Remember every window's position (at exit). */
    void save_windows();

    /**
     * Where to put `window` so it does not sit on top of the main window or another compared
     * replay's window (see PlayTogetherController::place_window).
     */
    QPoint place_window(const CompareWindow *window, const QPoint *wanted = nullptr) const;

    /** Callback thunks (see SuperShuckieFrontendCallbacks). */
    static void on_compare_refresh_screens(void *user_data, std::uint16_t id, std::size_t screen_count, const uint32_t *const *pixels);
    static void on_compare_change_video_mode(void *user_data, std::uint16_t id, std::size_t screen_count, const SuperShuckieScreenData *screen_data, std::uint8_t scaling);

private:
    MainWindow *main;
    std::map<std::uint16_t, CompareWindow *> windows;
    std::uint64_t last_generation = 0;
    int strip_countdown = 0;
    QJsonObject last_state;

    QJsonObject read_state() const;
    QJsonObject replay(std::uint16_t id) const;
    void apply_state(bool roster_changed);

    /**
     * Add the replay (a name of the current ROM's replays, or a file path), asking for its ROM
     * and confirming a mismatch as needed.
     */
    void add(const std::string &replay, bool by_name);
};

}

#endif
