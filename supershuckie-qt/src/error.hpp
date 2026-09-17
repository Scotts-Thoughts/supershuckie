#ifndef __SUPERSHUCKIE_ERROR_HPP__
#define __SUPERSHUCKIE_ERROR_HPP__

#include <QMessageBox>
#include <cstring>

// `parent` keeps the dialog on top of (and modal to) a specific window instead of floating
// parentless; callers that already have a QWidget* on hand should prefer DISPLAY_ERROR_DIALOG_P.
#define DISPLAY_ERROR_DIALOG_P(parent, title, ...) { \
    QMessageBox qmb(parent); \
    qmb.setWindowTitle(title); \
    qmb.setIcon(QMessageBox::Icon::Critical); \
    char ____________error_fmt[1024]; \
    std::snprintf(____________error_fmt, sizeof(____________error_fmt), __VA_ARGS__); \
    qmb.setText(____________error_fmt); \
    qmb.exec(); \
}

#define DISPLAY_ERROR_DIALOG(title, ...) DISPLAY_ERROR_DIALOG_P(nullptr, title, __VA_ARGS__)

#endif
