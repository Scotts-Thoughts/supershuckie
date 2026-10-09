#ifndef __SUPERSHUCKIE_GB_PALETTE_DIALOG_HPP__
#define __SUPERSHUCKIE_GB_PALETTE_DIALOG_HPP__

#include <QDialog>
#include <QIcon>
#include <cstddef>
#include <cstdint>
#include <optional>
#include <vector>
#include <supershuckie/supershuckie.h>

class QCheckBox;
class QListWidget;
class QLabel;
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
 *
 * The eyedropper picks a color from anywhere on the screen into the "picked color", which is then
 * dragged onto a color, set on one with its right-click menu, or set on a whole row (a shade of
 * all three palettes) with that row's Set row button. Presets can be exported to a file and
 * imported from one, to pass them between people.
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

    /** The presets as an export file's contents. */
    static QByteArray export_presets(const std::vector<GBColorPreset> &presets);

    /**
     * The presets in an export file's contents; `skipped` counts presets in it that are not
     * readable. Fails (with a reason in `error`) on a file that is not an export file.
     */
    static std::optional<std::vector<GBColorPreset>> import_presets(const QByteArray &contents, std::size_t &skipped, QString &error);

private:
    MainWindow *main_window;
    SuperShuckieGBCustomColors original = {};
    SuperShuckieGBCustomColors current = {};

    QListWidget *presets;
    QPushButton *add_preset;
    QPushButton *update_preset;
    QPushButton *rename_preset;
    QPushButton *remove_preset;
    QPushButton *export_button;
    std::vector<GBColorPreset> original_presets;

    QCheckBox *enabled;
    QPushButton *swatches[PALETTES][SHADES];
    QPushButton *set_row[SHADES];
    QPushButton *use_current;

    std::optional<std::uint32_t> picked;
    QPushButton *picked_well;

    std::uint32_t &color(std::size_t palette, std::size_t shade);
    void refresh_swatch(std::size_t palette, std::size_t shade);
    void refresh_swatches();
    void pick_color(std::size_t palette, std::size_t shade);
    void set_picked(std::uint32_t color);
    void refresh_picked();
    void use_picked(std::size_t palette, std::size_t shade);
    void use_picked_for_row(std::size_t shade);
    void eyedropper_into(std::size_t palette, std::size_t shade);
    void swatch_menu(std::size_t palette, std::size_t shade, const QPoint &pos);
    bool eventFilter(QObject *watched, QEvent *event) override;
    std::optional<std::pair<std::size_t, std::size_t>> swatch_of(const QObject *object) const;
    QPoint drag_start;
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
    void on_swap_light_dark();
    void on_eyedropper();
    void on_choose_picked();
    void on_preset_selected();
    void on_preset_double_clicked(QListWidgetItem *item);
    void on_add_preset();
    void on_update_preset();
    void on_rename_preset();
    void on_remove_preset();
    void on_import_presets();
    void on_export_presets();
    void refresh_preset_buttons();
};

}

#endif
