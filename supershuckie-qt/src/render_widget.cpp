#include "render_widget.hpp"
#include "main_window.hpp"
#include <QGraphicsPixmapItem>
#include <QKeyEvent>
#include <QMimeData>
#include <QImage>
#include <QPainter>

using namespace SuperShuckie64;

GameRenderWidget::GameRenderWidget(MainWindow *window, QWidget *parent): ScreenCanvas(parent), main_window(window) {
    this->setFocusPolicy(Qt::ClickFocus);
}

void GameRenderWidget::set_dimensions(unsigned screen_count, const SuperShuckieScreenData *screen_data, unsigned scale) noexcept {
    bool horizontal_nds = this->main_window->horizontal_nds->isChecked();
    bool swap_screens = this->main_window->frontend != nullptr &&
        supershuckie_frontend_get_swap_nds_screens(this->main_window->frontend);
    this->set_layout(screen_count, screen_data, scale, horizontal_nds, swap_screens);
}

void GameRenderWidget::keyPressEvent(QKeyEvent *event) {
    QWidget::keyPressEvent(event);
    
    if(this->main_window->frontend != nullptr) {
        // Replay controls
        int key = event->key();
        bool auto_repeat = event->isAutoRepeat();
        bool is_paused = supershuckie_frontend_is_paused(this->main_window->frontend);

        // Only while the replay is driving the game: once it is stopped the keyboard is the
        // user's game input again (the timeline keeps its own buttons).
        if(
            this->main_window->keyboard_replay_controls->isChecked() && 
            supershuckie_frontend_get_replay_state(this->main_window->frontend) == SuperShuckieReplayState::SuperShuckieReplayState__Playback &&
            !supershuckie_frontend_is_replay_playback_stopped(this->main_window->frontend)
        ) {
            auto *playback_action = this->main_window->playback_action_for(event);
            if(playback_action == this->main_window->playback_toggle_pause) {
                if(!auto_repeat) {
                    supershuckie_frontend_set_paused(
                        this->main_window->frontend,
                        !supershuckie_frontend_is_paused(this->main_window->frontend)
                    );
                }
                return;
            }
            if(playback_action == this->main_window->playback_skip_back) {
                supershuckie_frontend_advance_playback_frames(
                    this->main_window->frontend,
                    -240
                );
                return;
            }
            if(playback_action == this->main_window->playback_skip_forward) {
                supershuckie_frontend_advance_playback_frames(
                    this->main_window->frontend,
                    240
                );
                return;
            }
            if(playback_action == this->main_window->playback_step_back) {
                if(is_paused) {
                    supershuckie_frontend_advance_playback_frames(
                        this->main_window->frontend,
                        -1
                    );
                }
                return;
            }
            if(playback_action == this->main_window->playback_step_forward) {
                if(is_paused) {
                    supershuckie_frontend_advance_playback_frames(
                        this->main_window->frontend,
                        1
                    );
                }
                return;
            }
            if(key == Qt::Key_Up || key == Qt::Key_Down) {
                // up/down arrow keys do not do anything yet
                return;
            }
        }

        if(!auto_repeat) {
            supershuckie_frontend_key_press(this->main_window->frontend, key, true);
        }
    }
}

void GameRenderWidget::keyReleaseEvent(QKeyEvent *event) {
    QWidget::keyReleaseEvent(event);

    if(this->main_window->frontend != nullptr && !event->isAutoRepeat()) {
        supershuckie_frontend_key_press(this->main_window->frontend, event->key(), false);
    }
}


template<typename T> static std::optional<std::filesystem::path> validate_event(T *event) {
    auto *d = event->mimeData();
    if(d->hasUrls()) {
        auto urls = d->urls();
        if(urls.length() == 1) {
            auto path = std::filesystem::path(urls[0].toLocalFile().toStdString());
            return path;
        }
    }
    return std::nullopt;
}

void GameRenderWidget::dragEnterEvent(QDragEnterEvent *event) {
    if(validate_event(event)) {
        event->accept();
    }
}

void GameRenderWidget::dragMoveEvent(QDragMoveEvent *event) {
    if(validate_event(event)) {
        event->accept();
    }
}

void GameRenderWidget::dropEvent(QDropEvent *event) {
    auto path = validate_event(event);
    if(path) {
        this->main_window->load_rom(*path);
    }
}

void GameRenderWidget::mousePressEvent(QMouseEvent *event) {
    auto pos = event->position();
    int x = pos.x() / this->current_scale;
    int y = pos.y() / this->current_scale;

    if(this->screens.size() == 2) {
        auto &screen = this->screens[1];

        // The screen's offset applies on both axes: side by side it is the x offset, stacked it
        // is the y offset plus the centring of a narrower screen (the 3DS's bottom one).
        x -= screen.x;
        y -= screen.y;

        // >= : x == screen.width (one pixel past the right/bottom edge) is out of bounds.
        if(x < 0 || x >= static_cast<int>(screen.width) || y < 0 || y >= static_cast<int>(screen.height)) {
            return;
        }
        supershuckie_frontend_set_touch(this->main_window->frontend, true, x, y);
    }
}

void GameRenderWidget::mouseDoubleClickEvent(QMouseEvent *event) {
    this->mousePressEvent(event);
}

void GameRenderWidget::mouseReleaseEvent(QMouseEvent *) {
    supershuckie_frontend_set_touch(this->main_window->frontend, false, 0, 0);
}

void GameRenderWidget::mouseMoveEvent(QMouseEvent *event) {
    this->mousePressEvent(event);
}