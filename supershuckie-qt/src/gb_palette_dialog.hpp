#ifndef __SUPERSHUCKIE_GB_PALETTE_DIALOG_HPP__
#define __SUPERSHUCKIE_GB_PALETTE_DIALOG_HPP__

#include <QDialog>
#include <QIcon>
#include <cstddef>
#include <cstdint>
#include <vector>
#include <supershuckie/supershuckie.h>

class QCheckBox;
class QListWidget;
class QListWidgetItem;
class QPushButton;

namespace SuperShuckie64 {

class MainWindow;

struct GBColorPreset {
    QString name;
    SuperShuckieGBCustomColors colors;
};

/**
 * Settings › Game Boy › Custom colors…: the twelve colors a Game Boy game is drawn with (four
 * shades each of the background palette and object palettes 0 and 1), and named presets of them
 * that Settings › Game Boy › Color presets switches to in one step. Edits show in the game at
 * once; Cancel puts the previous colors and presets back.
 */
class GBPaletteDialog: public QDialog {
    Q_OBJECT
    friend MainWindow;

public:
    GBPaletteDialog(MainWindow *main_window);

    static const std::size_t PALETTES = 3;
    static const std::size_t SHADES = 4;

    static std::vector<GBColorPreset> load_presets(const SuperShuckieFrontendRaw *frontend);
    static void save_presets(SuperShuckieFrontendRaw *frontend, const std::vector<GBColorPreset> &presets);

    /** Whether the twelve colors are the same (whether they are on is not compared). */
    static bool same_colors(const SuperShuckieGBCustomColors &a, const SuperShuckieGBCustomColors &b);

    /** The colors as a small picture: a row per palette, a column per shade. */
    static QIcon preview_icon(const SuperShuckieGBCustomColors &colors);

private:
    MainWindow *main_window;
    SuperShuckieGBCustomColors original = {};
    SuperShuckieGBCustomColors current = {};

    QListWidget *presets;
    QPushButton *add_preset;
    QPushButton *update_preset;
    QPushButton *rename_preset;
    QPushButton *remove_preset;
    std::vector<GBColorPreset> original_presets;

    QCheckBox *enabled;
    QPushButton *swatches[PALETTES][SHADES];
    QPushButton *use_current;

    std::uint32_t &color(std::size_t palette, std::size_t shade);
    void refresh_swatch(std::size_t palette, std::size_t shade);
    void refresh_swatches();
    void pick_color(std::size_t palette, std::size_t shade);
    void apply();
    void colors_edited();

    void set_preset(QListWidgetItem *item, const GBColorPreset &preset);
    GBColorPreset preset_at(const QListWidgetItem *item) const;
    std::vector<GBColorPreset> current_presets() const;
    bool presets_changed() const;
    QListWidgetItem *selected_preset() const;
    void select_matching_preset();

    void accept() override;
    void reject() override;

private slots:
    void on_enabled_toggled(bool on);
    void on_use_current();
    void on_reset_to_grays();
    void on_preset_selected();
    void on_preset_double_clicked(QListWidgetItem *item);
    void on_add_preset();
    void on_update_preset();
    void on_rename_preset();
    void on_remove_preset();
    void refresh_preset_buttons();
};

}

#endif
