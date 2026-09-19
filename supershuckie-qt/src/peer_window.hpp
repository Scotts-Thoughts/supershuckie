#ifndef __SUPERSHUCKIE_PEER_WINDOW_HPP__
#define __SUPERSHUCKIE_PEER_WINDOW_HPP__

#include <QJsonObject>
#include <QColor>
#include <QWidget>
#include <cstdint>
#include <memory>

class QLabel;
struct SuperShuckieScreenData;

namespace SuperShuckie64 {

class PlayTogetherController;
class ScreenCanvas;
class AudioOutput;

/**
 * Another player's game in a Play Together session: their screens plus a status strip (name and
 * ROM, how far behind, timer and counters, frame rate). Takes no keyboard input. Closing hides
 * it; the controller destroys it when the player leaves.
 */
class PeerWindow: public QWidget {
    Q_OBJECT
public:
    PeerWindow(PlayTogetherController *controller, std::uint16_t peer_id, const QJsonObject &participant);
    ~PeerWindow();

    std::uint16_t peer_id() const noexcept { return this->id; }
    ScreenCanvas *canvas() const noexcept { return this->screen; }

    /** Geometry key for this player's window (by name, so it survives a rejoin). */
    QString settings_key() const;

    /** The player's colour (invalid when the session gave none). */
    QColor color() const noexcept { return this->player_color; }

    /** Black or white, whichever reads on `background`. */
    static QColor contrasting_text(const QColor &background);

    /** Lay the screens out (from the peer_change_video_mode callback). */
    void set_layout(unsigned screen_count, const SuperShuckieScreenData *screen_data, unsigned scale);

    /**
     * Refresh the status strip from the participant's JSON and the session's "link" object (see
     * play_together.h).
     */
    void update(const QJsonObject &participant, const QJsonObject &link);

    /** Whether a link cable can be plugged into this player's game right now (from the last update). */
    bool can_link() const noexcept { return this->linkable; }

    /** Whether the local player's link cable is in (or going into) this player's game. */
    bool is_link_peer() const noexcept { return this->link_peer; }

    /** Hear this player's game (opens an audio device on their ring) or stop. */
    bool set_audio(bool enabled, std::string *error);
    bool audio_on() const noexcept;

    void save_geometry();
    void restore_geometry();

protected:
    void closeEvent(QCloseEvent *event) override;
    void contextMenuEvent(QContextMenuEvent *event) override;

private:
    PlayTogetherController *controller;
    std::uint16_t id;
    QString name;
    QString rom_name;
    QColor player_color;
    ScreenCanvas *screen;
    QLabel *name_label;
    QLabel *sync_label;
    QLabel *time_label;
    QLabel *fps_label;
    std::unique_ptr<AudioOutput> audio;
    bool laid_out = false;
    bool linkable = false;
    bool link_peer = false;
};

}

#endif
