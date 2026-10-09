#ifndef __SUPERSHUCKIE_SCREEN_COLOR_PICKER_HPP__
#define __SUPERSHUCKIE_SCREEN_COLOR_PICKER_HPP__

#include <QColor>
#include <optional>

class QWidget;

namespace SuperShuckie64 {

/**
 * An eyedropper: takes a picture of every screen as it is now (every program's windows, not only
 * Super Shuckie's), lets the user click a pixel of it with a magnifier showing the pixels around
 * the pointer, and returns that pixel's color. Escape or a right click cancels.
 *
 * `window` (the dialog asking) is made see-through while the picture is taken so it does not
 * cover what is under it.
 */
std::optional<QColor> pick_screen_color(QWidget *window);

}

#endif
