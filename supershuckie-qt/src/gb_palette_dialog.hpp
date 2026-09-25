#ifndef __SUPERSHUCKIE_GB_PALETTE_DIALOG_HPP__
#define __SUPERSHUCKIE_GB_PALETTE_DIALOG_HPP__

#include <QDialog>
#include <cstddef>
#include <cstdint>
#include <supershuckie/supershuckie.h>

class QCheckBox;
class QPushButton;

namespace SuperShuckie64 {

class MainWindow;

/**
 * Settings › Game Boy › Custom colors…: the twelve colors a Game Boy game is drawn with (four
 * shades each of the background palette and object palettes 0 and 1). Edits show in the game at
 * once; Cancel puts the previous colors back.
 */
class GBPaletteDialog: public QDialog {
    Q_OBJECT
    friend MainWindow;

public:
    GBPaletteDialog(MainWindow *main_window);

    static const std::size_t PALETTES = 3;
    static const std::size_t SHADES = 4;

private:
    MainWindow *main_window;
    SuperShuckieGBCustomColors original = {};
    SuperShuckieGBCustomColors current = {};

    QCheckBox *enabled;
    QPushButton *swatches[PALETTES][SHADES];
    QPushButton *use_current;

    std::uint32_t &color(std::size_t palette, std::size_t shade);
    void refresh_swatch(std::size_t palette, std::size_t shade);
    void refresh_swatches();
    void pick_color(std::size_t palette, std::size_t shade);
    void apply();

    void accept() override;
    void reject() override;

private slots:
    void on_enabled_toggled(bool on);
    void on_use_current();
    void on_reset_to_grays();
};

}

#endif
