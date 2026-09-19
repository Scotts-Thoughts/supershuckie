#ifndef __SUPERSHUCKIE_PLAY_TOGETHER_CONTROLLER_HPP__
#define __SUPERSHUCKIE_PLAY_TOGETHER_CONTROLLER_HPP__

#include <QJsonObject>
#include <QObject>
#include <QPoint>
#include <QSet>
#include <cstdint>
#include <map>

#include <supershuckie/supershuckie.h>

class QLabel;
class QMessageBox;

namespace SuperShuckie64 {

class MainWindow;
class PeerWindow;
class PlayTogetherDialog;

/**
 * Owns everything Play Together shows: the session dialog, one window per other player, the
 * status-bar label, and the "locate this ROM" prompts. Ticked from the main window's tick.
 */
class PlayTogetherController: public QObject {
    Q_OBJECT
public:
    explicit PlayTogetherController(MainWindow *main_window);
    ~PlayTogetherController();

    MainWindow *main_window() const noexcept { return this->main; }

    /** The session state as supershuckie_frontend_play_together_state_json describes it. */
    QJsonObject read_state() const;
    bool is_active() const;
    bool is_host() const;

    /** Called every tick; cheap unless something changed. */
    void tick();

    /** Menu entry points. */
    void open_dialog();
    void leave();
    void reset_all();
    void show_windows();
    void set_scale(std::uint8_t scale);
    void unlink();

    /** Whether the local player's link cable is in (or going in). */
    bool is_link_cable_plugged() const;

    /**
     * Ask before an action that ends the session (opening another ROM, closing it, quitting).
     * Returns true when there is no session or the user agreed.
     */
    bool confirm_leave(const char *because);

    /** Ask the user for the ROM file `peer` is playing. */
    void locate_rom_for(std::uint16_t peer);

    /** Remember every window's position (at exit and when leaving). */
    void save_windows();

    /**
     * Where to put `window` so it does not sit on top of the main window or another player's
     * window: `wanted` if that spot is free, else cascaded down-right from it. With no `wanted`
     * the cascade starts beside the main window.
     */
    QPoint place_window(const PeerWindow *window, const QPoint *wanted = nullptr) const;

    /** Callback thunks (see SuperShuckieFrontendCallbacks). */
    static void on_peer_refresh_screens(void *user_data, std::uint16_t peer, std::size_t screen_count, const uint32_t *const *pixels);
    static void on_peer_change_video_mode(void *user_data, std::uint16_t peer, std::size_t screen_count, const SuperShuckieScreenData *screen_data, std::uint8_t scaling);

private:
    MainWindow *main;
    PlayTogetherDialog *dialog = nullptr;
    std::map<std::uint16_t, PeerWindow *> windows;
    std::uint64_t last_generation = 0;
    int strip_countdown = 0;
    QSet<std::uint16_t> prompted_for_rom;
    QLabel *status_label;
    QJsonObject last_state;
    bool was_active = false;
    /** The non-modal prompt for an incoming link request, while one is up, and its nonce. */
    QMessageBox *link_prompt = nullptr;
    std::uint32_t link_prompt_nonce = 0;
    QString last_link_reason;

    void apply_state(const QJsonObject &state, bool roster_changed);
    void apply_link_state(const QJsonObject &link, bool active);
    void close_link_prompt();
    QJsonObject participant(std::uint16_t peer) const;
};

}

#endif
