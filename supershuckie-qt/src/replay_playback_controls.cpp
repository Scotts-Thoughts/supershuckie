#include <QPainter>
#include <QMouseEvent>
#include <QHelpEvent>
#include <QToolTip>
#include <QGuiApplication>
#include <QStyleHints>

#include "replay_playback_controls.hpp"
#include "main_window.hpp"

using namespace SuperShuckie64;

#define PLAYBACK_HEIGHT 24

#define PAUSE_BUTTON_THICKNESS 4
#define BUTTON_PADDING_HORIZ 12
#define BUTTON_PADDING_VERT 4
#define BUTTON_ICON_WIDTH 12
#define BUTTON_FULL_WIDTH (BUTTON_PADDING_HORIZ*2 + BUTTON_ICON_WIDTH)

// Left to right: play/pause, stop/resume, back to the resume point, then the bar.
#define PAUSE_BUTTON_X 0
#define STOP_BUTTON_X BUTTON_FULL_WIDTH
#define RESUME_POINT_BUTTON_X (BUTTON_FULL_WIDTH * 2)
#define BUTTONS_FULL_WIDTH (BUTTON_FULL_WIDTH * 3)

#define STOP_ICON_SIZE 12
#define RESUME_POINT_BAR_THICKNESS 3
#define HIGHLIGHT_INSET 3
#define HIGHLIGHT_RADIUS 4

#define BAR_THICKNESS 4
#define BAR_PADDING BUTTON_PADDING_HORIZ

#define INDICATOR_RADIUS 4

ReplayPlaybackControls::ReplayPlaybackControls(MainWindow *main_window, QWidget *parent): QWidget(parent), main_window(main_window) {
    this->setFixedHeight(PLAYBACK_HEIGHT);
    this->setMinimumWidth(PLAYBACK_HEIGHT);
    this->setFocusPolicy(Qt::NoFocus);
}

void ReplayPlaybackControls::draw_play_icon(QPainter &painter, int x) {
    const QPointF play_button[3] = {
        QPointF(
            x + BUTTON_PADDING_HORIZ, BUTTON_PADDING_VERT
        ),
        QPointF(
            x + BUTTON_PADDING_HORIZ, PLAYBACK_HEIGHT - BUTTON_PADDING_VERT
        ),
        QPointF(
            x + BUTTON_PADDING_HORIZ + BUTTON_ICON_WIDTH, PLAYBACK_HEIGHT / 2.0
        ),
    };
    painter.drawPolygon(play_button, 3);
}

void ReplayPlaybackControls::draw_pause_icon(QPainter &painter, int x) {
    const QRectF pause_button[2] = {
        QRectF(
            x + BUTTON_PADDING_HORIZ, BUTTON_PADDING_VERT,
            PAUSE_BUTTON_THICKNESS, PLAYBACK_HEIGHT - BUTTON_PADDING_VERT * 2.0
        ),
        QRectF(
            x + BUTTON_FULL_WIDTH - PAUSE_BUTTON_THICKNESS - BUTTON_PADDING_HORIZ, BUTTON_PADDING_VERT,
            PAUSE_BUTTON_THICKNESS, PLAYBACK_HEIGHT - BUTTON_PADDING_VERT * 2.0
        ),
    };
    painter.drawRects(pause_button, 2);
}

void ReplayPlaybackControls::draw_stop_icon(QPainter &painter, int x) {
    painter.drawRect(QRectF(
        x + BUTTON_PADDING_HORIZ, PLAYBACK_HEIGHT / 2.0 - STOP_ICON_SIZE / 2.0,
        STOP_ICON_SIZE, STOP_ICON_SIZE
    ));
}

// A bar with a triangle pointing back at it ("skip to the marker").
void ReplayPlaybackControls::draw_back_to_resume_point_icon(QPainter &painter, int x) {
    painter.drawRect(QRectF(
        x + BUTTON_PADDING_HORIZ, BUTTON_PADDING_VERT,
        RESUME_POINT_BAR_THICKNESS, PLAYBACK_HEIGHT - BUTTON_PADDING_VERT * 2.0
    ));
    const QPointF triangle[3] = {
        QPointF(
            x + BUTTON_PADDING_HORIZ + BUTTON_ICON_WIDTH, BUTTON_PADDING_VERT
        ),
        QPointF(
            x + BUTTON_PADDING_HORIZ + BUTTON_ICON_WIDTH, PLAYBACK_HEIGHT - BUTTON_PADDING_VERT
        ),
        QPointF(
            x + BUTTON_PADDING_HORIZ + RESUME_POINT_BAR_THICKNESS + 1, PLAYBACK_HEIGHT / 2.0
        ),
    };
    painter.drawPolygon(triangle, 3);
}

void ReplayPlaybackControls::paintEvent(QPaintEvent *event) {
    auto palette = QGuiApplication::palette();

    QPainter painter(this);
    QRect fill_rectangle = QRect(0, 0, this->width(), PLAYBACK_HEIGHT);
    painter.setRenderHint(QPainter::Antialiasing);

    painter.fillRect(fill_rectangle, palette.window().color());
    painter.setBrush(palette.windowText());

    if(this->is_stopped) {
        // The game is the user's: the first button only pauses/unpauses it (lit while paused, as
        // the icon itself never changes) and the second resumes the replay.
        if(this->is_paused) {
            QColor highlight = palette.accent().color();
            QColor background = highlight;
            background.setAlpha(72);

            auto pen = painter.pen();
            painter.setPen(Qt::NoPen);
            painter.setBrush(background);
            painter.drawRoundedRect(
                QRectF(PAUSE_BUTTON_X + HIGHLIGHT_INSET, HIGHLIGHT_INSET, BUTTON_FULL_WIDTH - HIGHLIGHT_INSET * 2, PLAYBACK_HEIGHT - HIGHLIGHT_INSET * 2),
                HIGHLIGHT_RADIUS, HIGHLIGHT_RADIUS
            );
            painter.setPen(pen);
            painter.setBrush(highlight);
        }
        this->draw_pause_icon(painter, PAUSE_BUTTON_X);

        painter.setBrush(palette.windowText());
        this->draw_play_icon(painter, STOP_BUTTON_X);
        this->draw_back_to_resume_point_icon(painter, RESUME_POINT_BUTTON_X);
    }
    else {
        if(this->is_paused) {
            this->draw_play_icon(painter, PAUSE_BUTTON_X);
        }
        else {
            this->draw_pause_icon(painter, PAUSE_BUTTON_X);
        }
        this->draw_stop_icon(painter, STOP_BUTTON_X);

        // There is no resume point to go back to until the replay is stopped.
        painter.setBrush(palette.color(QPalette::Disabled, QPalette::WindowText));
        this->draw_back_to_resume_point_icon(painter, RESUME_POINT_BUTTON_X);
        painter.setBrush(palette.windowText());
    }

    auto bounds = this->playback_bar_bounds();

    QColor progress_color = palette.accent().color();
    QColor remaining_color = QColor(64, 64, 64);

    QPointF center_point(bounds.x(), bounds.y() + BAR_THICKNESS / 2.0);

    if(this->playback_progress <= 0.0) {
        painter.fillRect(bounds, remaining_color);
    }
    else if(this->playback_progress >= 1.0) {
        painter.fillRect(bounds, progress_color);
        center_point.setX(bounds.x() + bounds.width());
    }
    else {
        auto elapsed_bounds = bounds;
        auto remaining_bounds = bounds;

        int elapsed_width = static_cast<int>(elapsed_bounds.width() * this->playback_progress);
        int remaining_width = remaining_bounds.width() - elapsed_width;
        center_point.setX(elapsed_bounds.x() + elapsed_width);

        elapsed_bounds.setWidth(elapsed_width);
        remaining_bounds.setX(remaining_bounds.x() + elapsed_width);

        painter.fillRect(elapsed_bounds, progress_color);
        painter.fillRect(remaining_bounds, palette.mid().color());
    }

    painter.drawEllipse(center_point, INDICATOR_RADIUS, INDICATOR_RADIUS);

    QWidget::paintEvent(event);
}

QRectF ReplayPlaybackControls::playback_bar_bounds() {
    float x = BUTTONS_FULL_WIDTH + BAR_PADDING - BUTTON_PADDING_HORIZ;
    return QRectF(
        x, PLAYBACK_HEIGHT / 2.0 - BAR_THICKNESS / 2.0,
        this->width() - BAR_PADDING - x, BAR_THICKNESS
    );
}

void ReplayPlaybackControls::tick() {
    // TODO: check if dimensions have changed?

    bool needs_repaint = false;
    auto *frontend = this->main_window->frontend;

    if(supershuckie_frontend_is_paused(frontend) != this->is_paused) {
        this->is_paused = !this->is_paused;
        needs_repaint = true;
    }

    // Stopping can also happen from inside the frontend ("Stop playback on input").
    if(supershuckie_frontend_is_replay_playback_stopped(frontend) != this->is_stopped) {
        this->is_stopped = !this->is_stopped;
        needs_repaint = true;
    }

    // get_replay_playback_time does not write its out-params when no replay is loaded; default to
    // 0 so calculated_progress below falls into its total_frames == 0 case.
    std::uint32_t total_frames = 0;
    supershuckie_frontend_get_replay_playback_time(frontend, &total_frames, nullptr);

    // The replay's own position: while it is stopped, that is where playback resumes from (the
    // frame counter itself keeps counting the live play).
    std::uint32_t replay_frame = supershuckie_frontend_get_replay_frame(frontend);

    double calculated_progress = total_frames == 0 ? 1.0 : static_cast<double>(replay_frame) / static_cast<double>(total_frames);
    if(!this->is_clicking_on_bar && this->playback_progress != calculated_progress) {
        this->playback_progress = calculated_progress;
        needs_repaint = true;
    }

    if(needs_repaint) {
        this->repaint();
    }
}

// The buttons are the Replays menu's stop/resume/go-back actions (which is what makes them
// rebindable), so they go through the same slots.
void ReplayPlaybackControls::toggle_stopped() {
    if(supershuckie_frontend_is_replay_playback_stopped(this->main_window->frontend)) {
        this->main_window->do_resume_playback();
    }
    else {
        this->main_window->do_stop_playback();
    }
}

void ReplayPlaybackControls::mousePressEvent(QMouseEvent *event) {
    int x = event->position().x();

    if(x < STOP_BUTTON_X) {
        supershuckie_frontend_set_paused(this->main_window->frontend, !supershuckie_frontend_is_paused(this->main_window->frontend));
        return;
    }

    if(x < RESUME_POINT_BUTTON_X) {
        this->toggle_stopped();
        return;
    }

    if(x < BUTTONS_FULL_WIDTH) {
        // Back to where playback would resume from, staying in control (a no-op while playing).
        this->main_window->do_go_to_resume_point();
        return;
    }

    auto progress_requested = this->progress_on_bar(x);
    if(progress_requested < 0.0 || progress_requested > 1.0) {
        return;
    }

    this->playback_progress = progress_requested;

    supershuckie_frontend_set_playback_frozen(this->main_window->frontend, true);
    supershuckie_frontend_set_playback_frame(this->main_window->frontend, this->progress_to_frame(progress_requested));
    this->is_clicking_on_bar = true;
    this->repaint();
}

void ReplayPlaybackControls::mouseReleaseEvent(QMouseEvent *event) {
    if(this->is_clicking_on_bar) {
        supershuckie_frontend_set_playback_frozen(this->main_window->frontend, false);
        this->is_clicking_on_bar = false;
    }
}

bool ReplayPlaybackControls::event(QEvent *event) {
    if(event->type() != QEvent::ToolTip) {
        return QWidget::event(event);
    }

    auto *help_event = static_cast<QHelpEvent *>(event);
    int x = help_event->pos().x();
    QString text;

    if(x < STOP_BUTTON_X) {
        text = this->is_paused ? "Unpause" : "Pause";
    }
    else if(x < RESUME_POINT_BUTTON_X) {
        if(this->is_stopped) {
            text = QString("Resume the replay from frame %1").arg(supershuckie_frontend_get_replay_frame(this->main_window->frontend));
        }
        else {
            text = "Stop the replay and take control (the replay stays loaded)";
        }
    }
    else if(x < BUTTONS_FULL_WIDTH) {
        if(this->is_stopped) {
            text = QString("Go back to frame %1 (where the replay would resume) and keep control").arg(supershuckie_frontend_get_replay_frame(this->main_window->frontend));
        }
        else {
            text = "Go back to the resume point (once the replay is stopped)";
        }
    }
    else if(this->is_stopped) {
        text = "Go to a frame of the replay and keep control from there";
    }

    if(text.isEmpty()) {
        QToolTip::hideText();
    }
    else {
        QToolTip::showText(help_event->globalPos(), text, this);
    }
    return true;
}

double ReplayPlaybackControls::progress_on_bar(int x) {
    auto bounds = this->playback_bar_bounds();
    return static_cast<double>(x - bounds.x()) / static_cast<double>(bounds.width());
}

std::uint32_t ReplayPlaybackControls::progress_to_frame(double progress) {
    if(progress <= 0.0) {
        return 0;
    }

    // Same as tick(): the out-param is only written while a replay is loaded.
    std::uint32_t total_frames = 0;
    supershuckie_frontend_get_replay_playback_time(this->main_window->frontend, &total_frames, nullptr);

    if(progress >= 1.0) {
        return total_frames;
    }

    return static_cast<double>(total_frames * progress + 0.5);
}

void ReplayPlaybackControls::mouseMoveEvent(QMouseEvent *event) {
    if(!this->is_clicking_on_bar) {
        return;
    }

    double progress = this->progress_on_bar(event->position().x());
    this->playback_progress = progress;

    supershuckie_frontend_set_playback_frame(
        this->main_window->frontend,
        this->progress_to_frame(progress)
    );

    this->repaint();
}
