#ifndef __SUPERSHUCKIE_SCREEN_CANVAS_HPP__
#define __SUPERSHUCKIE_SCREEN_CANVAS_HPP__

#include <QGraphicsView>
#include <QPixmap>
#include <cstdint>
#include <vector>

class QGraphicsScene;
class QGraphicsPixmapItem;
struct SuperShuckieScreenData;

namespace SuperShuckie64 {

/**
 * Composites an emulator's screens (one or two) at an integer scale. Input handling is the
 * subclass's business: the main game view takes the keyboard and the touch screen, a window
 * showing another player's game takes nothing.
 */
class ScreenCanvas: public QGraphicsView {
public:
    explicit ScreenCanvas(QWidget *parent);

    struct ScreenData {
        unsigned width;
        unsigned height;
        QPixmap pixmap;
        QGraphicsPixmapItem *pixmap_item = nullptr;
        unsigned x = 0;
        unsigned y = 0;
    };

    /**
     * Rebuild the scene for `screen_count` screens of the given geometry at `scale`. Two screens
     * go side by side when `horizontal`, else one above the other; `swap` shows the second
     * screen first (both orientations). The widget's fixed size follows.
     */
    void set_layout(unsigned screen_count, const SuperShuckieScreenData *screen_data, unsigned scale, bool horizontal, bool swap) noexcept;

    /** Upload fresh pixels (`pixels[i]` is screen `i`'s 0xAARRGGBB buffer). */
    void refresh_screen(unsigned screen_count, const uint32_t *const *pixels);

    /**
     * Composite the current frame into a native-resolution image (screens positioned exactly as
     * displayed). Returns a null QImage if no frame is available.
     */
    QImage capture() const;

    unsigned scale() const noexcept { return this->current_scale; }
    unsigned native_width() const noexcept { return this->total_width; }
    unsigned native_height() const noexcept { return this->total_height; }
    const std::vector<ScreenData> &screen_list() const noexcept { return this->screens; }

protected:
    std::vector<ScreenData> screens;
    unsigned total_width = 1, total_height = 1, current_scale = 1;

private:
    QGraphicsScene *scene = nullptr;
};

}

#endif
