#ifndef __SUPERSHUCKIE_PEER_WINDOW_HPP__
#define __SUPERSHUCKIE_PEER_WINDOW_HPP__

#include <QJsonObject>
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

    /** Lay the screens out (from the peer_change_video_mode callback). */
    void set_layout(unsigned screen_count, const SuperShuckieScreenData *screen_data, unsigned scale);

    /** Refresh the status strip from the participant's JSON (see play_together.h). */
    void update(const QJsonObject &participant);

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
    ScreenCanvas *screen;
    QLabel *name_label;
    QLabel *sync_label;
    QLabel *time_label;
    QLabel *fps_label;
    std::unique_ptr<AudioOutput> audio;
    bool laid_out = false;
};

}

#endif
