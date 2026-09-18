#ifndef __SUPERSHUCKIE_RENDER_VIEW_HPP__
#define __SUPERSHUCKIE_RENDER_VIEW_HPP__

#include <QWidget>
#include "screen_canvas.hpp"

struct SuperShuckieScreenData;

namespace SuperShuckie64 {

class MainWindow;

/** The player's own game: a ScreenCanvas that also takes the keyboard, drops and the touch screen. */
class GameRenderWidget: public ScreenCanvas {
    friend MainWindow;
public:
    /** Lay the screens out as the main window's NDS orientation and screen-swap settings say. */
    void set_dimensions(unsigned screen_count, const SuperShuckieScreenData *screen_data, unsigned scale) noexcept;

private:
    GameRenderWidget(MainWindow *window, QWidget *parent);
    MainWindow *main_window;

    void keyPressEvent(QKeyEvent *event) override;
    void keyReleaseEvent(QKeyEvent *event) override;
    
    void dragEnterEvent(QDragEnterEvent *event) override;
    void dragMoveEvent(QDragMoveEvent *event) override;
    void dropEvent(QDropEvent *event) override;

    void mousePressEvent(QMouseEvent *event) override;
    void mouseReleaseEvent(QMouseEvent *event) override;
    void mouseDoubleClickEvent(QMouseEvent *event) override;
    void mouseMoveEvent(QMouseEvent *event) override;
};

}

#endif
