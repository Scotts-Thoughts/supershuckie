#include "screen_canvas.hpp"

#include <QGraphicsPixmapItem>
#include <QGraphicsScene>
#include <QImage>
#include <QPainter>
#include <supershuckie/supershuckie.h>

using namespace SuperShuckie64;

ScreenCanvas::ScreenCanvas(QWidget *parent): QGraphicsView(parent) {
    this->setFrameStyle(0);
    this->setHorizontalScrollBarPolicy(Qt::ScrollBarPolicy::ScrollBarAlwaysOff);
    this->setVerticalScrollBarPolicy(Qt::ScrollBarPolicy::ScrollBarAlwaysOff);
    this->setSizePolicy(QSizePolicy::Policy::Fixed, QSizePolicy::Policy::Fixed);
}

void ScreenCanvas::set_layout(unsigned screen_count, const SuperShuckieScreenData *screen_data, unsigned scale, bool horizontal, bool swap) noexcept {
    if(scale == 0) {
        scale = 1;
    }

    this->current_scale = scale;
    this->setTransform(QTransform::fromScale(scale, scale));

    delete this->scene;
    this->scene = nullptr;

    this->screens.clear();

    this->scene = new QGraphicsScene(this);

    this->total_width = 0;
    this->total_height = 0;

    // Create the screen objects in data order so screens[i] stays bound to pixels[i] in
    // refresh_screen(), and so touch input keeps targeting the bottom screen (screens[1]).
    for(unsigned i = 0; i < screen_count; i++) {
        ScreenData screen;
        screen.width = screen_data[i].width;
        screen.height = screen_data[i].height;
        screen.pixmap_item = this->scene->addPixmap(screen.pixmap);
        this->screens.emplace_back(screen);
    }

    // Assign on-screen positions in visual order. When swapping, place the two screens in
    // reverse so the bottom screen comes first; this works for both orientations and the
    // stored x/y offsets keep touch mapping correct.
    for(unsigned visual = 0; visual < screen_count; visual++) {
        unsigned i = (swap && screen_count == 2) ? (screen_count - 1 - visual) : visual;
        auto &screen = this->screens[i];

        if(horizontal) {
            screen.x = this->total_width;
            this->total_width += screen.width;
            this->total_height = this->total_height > screen.height ? this->total_height : screen.height;
        }
        else {
            screen.y = this->total_height;
            this->total_height += screen.height;
            this->total_width = this->total_width > screen.width ? this->total_width : screen.width;
        }

        screen.pixmap_item->setOffset(screen.x, screen.y);
    }

    this->setFixedSize(this->total_width * scale, this->total_height * scale);
    this->setScene(this->scene);

    // Under set_manual_present() the view does not repaint for scene changes by itself.
    this->viewport()->update();
}

void ScreenCanvas::set_manual_present(bool manual) {
    this->setViewportUpdateMode(manual ? QGraphicsView::NoViewportUpdate : QGraphicsView::MinimalViewportUpdate);
    this->viewport()->update();
}

void ScreenCanvas::present_now() {
    this->viewport()->repaint();
}

QImage ScreenCanvas::capture() const {
    if(this->screens.empty() || this->total_width == 0 || this->total_height == 0) {
        return QImage();
    }

    // Paint each screen's current pixmap at its on-screen offset. The offsets already encode the
    // vertical/horizontal arrangement and the screen-swap setting, so the result matches the view.
    QImage image(this->total_width, this->total_height, QImage::Format_ARGB32);
    image.fill(Qt::black);

    QPainter painter(&image);
    for(const auto &screen : this->screens) {
        painter.drawPixmap(static_cast<int>(screen.x), static_cast<int>(screen.y), screen.pixmap);
    }
    painter.end();

    return image;
}

void ScreenCanvas::refresh_screen(unsigned screen_count, const uint32_t *const *pixels) {
    for(unsigned i = 0; i < this->screens.size() && i < screen_count; i++) {
        auto &screen = this->screens[i];
        screen.pixmap.convertFromImage(QImage(reinterpret_cast<const uchar *>(pixels[i]), screen.width, screen.height, QImage::Format::Format_ARGB32));
        screen.pixmap_item->setPixmap(screen.pixmap);
    }
}
