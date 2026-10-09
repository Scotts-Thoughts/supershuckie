#include "screen_color_picker.hpp"

#include <QCursor>
#include <QEventLoop>
#include <QGuiApplication>
#include <QImage>
#include <QKeyEvent>
#include <QMouseEvent>
#include <QPainter>
#include <QPixmap>
#include <QScreen>
#include <QTimer>
#include <QWidget>
#include <QWindow>
#include <algorithm>
#include <memory>
#include <vector>

using namespace SuperShuckie64;

// The magnifier shows this many pixels across (odd, so one of them is in the middle) ...
static const int LOUPE_PIXELS = 15;
// ... each this many times bigger.
static const int LOUPE_ZOOM = 9;
static const int LOUPE_SIZE = LOUPE_PIXELS * LOUPE_ZOOM;
static const int LOUPE_LABEL_HEIGHT = 24;
// How far the magnifier is drawn from the pointer.
static const int LOUPE_OFFSET = 24;

// Long enough for the window manager to stop drawing a window that was made see-through.
static const int HIDE_DELAY_MS = 200;

static const char *const HINT = "Click to pick a color  ·  Esc or right-click to cancel";

namespace {

struct Picking {
    QEventLoop loop;
    std::optional<QColor> result;
};

/** One screen's picture, covering that screen until a pixel is picked. */
class ScreenColorOverlay: public QWidget {
public:
    ScreenColorOverlay(QWidget *parent, QScreen *screen, Picking *picking): QWidget(parent, Qt::Window | Qt::FramelessWindowHint | Qt::WindowStaysOnTopHint | Qt::Tool), picking(picking) {
        this->shot = screen->grabWindow(0);
        this->image = this->shot.toImage();

        this->setAttribute(Qt::WA_OpaquePaintEvent);
        this->setMouseTracking(true);
        this->setCursor(Qt::CrossCursor);
        this->setFocusPolicy(Qt::StrongFocus);

        // On the right screen before it is sized, so it gets that screen's scale.
        this->winId();
        this->windowHandle()->setScreen(screen);
        this->setGeometry(screen->geometry());
    }

    void follow_cursor() {
        auto pos = this->mapFromGlobal(QCursor::pos());
        if(this->rect().contains(pos)) {
            this->cursor = pos;
            this->update();
        }
    }

protected:
    void paintEvent(QPaintEvent *) override {
        QPainter painter(this);
        painter.drawPixmap(this->rect(), this->shot);

        // Out of the pointer's way.
        QFont font = painter.font();
        font.setPointSize(11);
        painter.setFont(font);
        auto hint_size = painter.fontMetrics().size(0, HINT) + QSize(24, 12);
        QRect hint(QPoint((this->width() - hint_size.width()) / 2, 16), hint_size);
        if(this->cursor.has_value() && hint.adjusted(-LOUPE_SIZE, -LOUPE_SIZE, LOUPE_SIZE, LOUPE_SIZE).contains(*this->cursor)) {
            hint.moveBottom(this->height() - 16);
        }
        painter.setRenderHint(QPainter::Antialiasing);
        painter.setPen(Qt::NoPen);
        painter.setBrush(QColor(0, 0, 0, 190));
        painter.drawRoundedRect(hint, 6, 6);
        painter.setPen(Qt::white);
        painter.drawText(hint, Qt::AlignCenter, HINT);
        painter.setRenderHint(QPainter::Antialiasing, false);

        if(!this->cursor.has_value()) {
            return;
        }

        auto pixel = this->pixel_at(*this->cursor);
        QRect loupe(*this->cursor + QPoint(LOUPE_OFFSET, LOUPE_OFFSET), QSize(LOUPE_SIZE, LOUPE_SIZE + LOUPE_LABEL_HEIGHT));
        if(loupe.right() >= this->width()) {
            loupe.moveRight(this->cursor->x() - LOUPE_OFFSET);
        }
        if(loupe.bottom() >= this->height()) {
            loupe.moveBottom(this->cursor->y() - LOUPE_OFFSET);
        }

        // Pixels outside the picture come out black.
        QRect around(pixel - QPoint(LOUPE_PIXELS / 2, LOUPE_PIXELS / 2), QSize(LOUPE_PIXELS, LOUPE_PIXELS));
        QRect magnified(loupe.topLeft(), QSize(LOUPE_SIZE, LOUPE_SIZE));
        painter.setRenderHint(QPainter::SmoothPixmapTransform, false);
        painter.drawImage(magnified, this->image.copy(around));

        // The pixel that would be picked.
        QRect middle(magnified.topLeft() + QPoint(LOUPE_PIXELS / 2 * LOUPE_ZOOM, LOUPE_PIXELS / 2 * LOUPE_ZOOM), QSize(LOUPE_ZOOM, LOUPE_ZOOM));
        painter.setBrush(Qt::NoBrush);
        painter.setPen(Qt::black);
        painter.drawRect(middle.adjusted(-1, -1, 0, 0));
        painter.setPen(Qt::white);
        painter.drawRect(middle.adjusted(-2, -2, 1, 1));
        painter.setPen(QColor(0, 0, 0, 160));
        painter.drawRect(magnified.adjusted(0, 0, -1, -1));

        QRect label(magnified.bottomLeft() + QPoint(0, 1), QSize(LOUPE_SIZE, LOUPE_LABEL_HEIGHT - 1));
        auto color = this->color_at(*this->cursor);
        painter.fillRect(label, QColor(32, 32, 32));
        auto chip = QRect(label.topLeft() + QPoint(5, 5), QSize(label.height() - 10, label.height() - 10));
        painter.fillRect(chip, color);
        painter.setPen(Qt::white);
        painter.drawRect(chip.adjusted(0, 0, -1, -1));
        painter.drawText(label.adjusted(chip.width() + 10, 0, 0, 0), Qt::AlignVCenter | Qt::AlignLeft, color.name(QColor::HexRgb).toUpper());
    }

    void mouseMoveEvent(QMouseEvent *event) override {
        this->cursor = event->position().toPoint();
        this->update();
    }

    void leaveEvent(QEvent *) override {
        // Another screen's picture shows the magnifier now.
        this->cursor.reset();
        this->update();
    }

    void mousePressEvent(QMouseEvent *event) override {
        // Picked on release, so the release does not land on a window under the picture once it is gone.
        event->accept();
    }

    void mouseReleaseEvent(QMouseEvent *event) override {
        if(event->button() == Qt::LeftButton) {
            this->picking->result = this->color_at(event->position().toPoint());
        }
        this->picking->loop.quit();
    }

    void keyPressEvent(QKeyEvent *event) override {
        switch(event->key()) {
            case Qt::Key_Escape:
                this->picking->loop.quit();
                break;
            case Qt::Key_Return:
            case Qt::Key_Enter:
                if(this->cursor.has_value()) {
                    this->picking->result = this->color_at(*this->cursor);
                    this->picking->loop.quit();
                }
                break;
            default:
                QWidget::keyPressEvent(event);
        }
    }

private:
    Picking *picking;
    QPixmap shot;
    QImage image;
    std::optional<QPoint> cursor;

    /** The picture's pixel under `pos`, which is in this widget's (scaled) coordinates. */
    QPoint pixel_at(QPoint pos) const {
        if(this->width() <= 0 || this->height() <= 0 || this->image.isNull()) {
            return QPoint();
        }
        auto x = static_cast<int>(static_cast<qint64>(pos.x()) * this->image.width() / this->width());
        auto y = static_cast<int>(static_cast<qint64>(pos.y()) * this->image.height() / this->height());
        return QPoint(std::clamp(x, 0, this->image.width() - 1), std::clamp(y, 0, this->image.height() - 1));
    }

    QColor color_at(QPoint pos) const {
        if(this->image.isNull()) {
            return QColor(Qt::black);
        }
        return QColor(this->image.pixel(this->pixel_at(pos))).toRgb();
    }
};

}

std::optional<QColor> SuperShuckie64::pick_screen_color(QWidget *window) {
    qreal opacity = 1.0;
    if(window != nullptr) {
        opacity = window->windowOpacity();
        window->setWindowOpacity(0.0);
        QEventLoop wait;
        QTimer::singleShot(HIDE_DELAY_MS, &wait, &QEventLoop::quit);
        wait.exec(QEventLoop::ExcludeUserInputEvents);
    }

    Picking picking;
    std::vector<std::unique_ptr<ScreenColorOverlay>> overlays;
    for(auto *screen : QGuiApplication::screens()) {
        overlays.push_back(std::make_unique<ScreenColorOverlay>(window, screen, &picking));
    }

    // The pictures are taken, so the window can come back; the overlays cover it.
    if(window != nullptr) {
        window->setWindowOpacity(opacity);
    }

    ScreenColorOverlay *focused = nullptr;
    for(auto &overlay : overlays) {
        overlay->show();
        if(overlay->geometry().contains(QCursor::pos())) {
            focused = overlay.get();
        }
    }
    if(focused == nullptr && !overlays.empty()) {
        focused = overlays.front().get();
    }
    if(focused != nullptr) {
        focused->raise();
        focused->activateWindow();
        focused->setFocus();
        // Escape still cancels if a click on another screen's picture moved the focus.
        focused->grabKeyboard();
        focused->follow_cursor();
    }

    picking.loop.exec();

    if(focused != nullptr) {
        focused->releaseKeyboard();
    }
    overlays.clear();
    if(window != nullptr) {
        window->activateWindow();
    }
    return picking.result;
}
