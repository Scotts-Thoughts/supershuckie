#include <QWidget>

#include <cstdint>

namespace SuperShuckie64 {

class MainWindow;

/**
 * The replay timeline: a play/pause button, a stop/resume button, a "back to the resume point"
 * button and a seekable progress bar.
 *
 * Shown while a replay is loaded. With the replay playing, the first button pauses/resumes the
 * emulator and the second stops the replay (the replay stays loaded, the game runs on live under
 * the user's control); the third is disabled. With the replay stopped, the first button is always
 * a pause toggle (highlighted while paused), the second resumes playback from where it was
 * stopped or last seeked to (the resume point), and the third puts the game back at the resume
 * point while leaving the user in control; clicking the bar seeks and leaves the user in control
 * at that frame (which becomes the resume point).
 */
class ReplayPlaybackControls: public QWidget {
    Q_OBJECT
    friend MainWindow;
public:
    ReplayPlaybackControls(MainWindow *main_window, QWidget *parent);
private:
    MainWindow *main_window;

    void paintEvent(QPaintEvent *event) override;
    void mouseMoveEvent(QMouseEvent *event) override;
    void mousePressEvent(QMouseEvent *event) override;
    void mouseReleaseEvent(QMouseEvent *event) override;
    bool event(QEvent *event) override;

    QRectF playback_bar_bounds();
    double progress_on_bar(int x);
    std::uint32_t progress_to_frame(double progress);

    void draw_play_icon(QPainter &painter, int x);
    void draw_pause_icon(QPainter &painter, int x);
    void draw_stop_icon(QPainter &painter, int x);
    void draw_back_to_resume_point_icon(QPainter &painter, int x);

    void toggle_stopped();

    void tick();

    bool is_paused = false;
    /** The loaded replay is stopped (see the class comment); mirrors the frontend. */
    bool is_stopped = false;
    bool is_clicking_on_bar = false;

    double playback_progress = 0.0;
};
}
