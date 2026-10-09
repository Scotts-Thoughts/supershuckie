#include "gb_palette_dialog.hpp"
#include "main_window.hpp"
#include "ask_for_text_dialog.hpp"
#include "screen_color_picker.hpp"

#include <QApplication>
#include <QCheckBox>
#include <QColorDialog>
#include <QDialogButtonBox>
#include <QDrag>
#include <QDragEnterEvent>
#include <QDropEvent>
#include <QFile>
#include <QFileDialog>
#include <QGridLayout>
#include <QGroupBox>
#include <QHBoxLayout>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QLabel>
#include <QListWidget>
#include <QMenu>
#include <QMessageBox>
#include <QMimeData>
#include <QMouseEvent>
#include <QPainter>
#include <QPixmap>
#include <QPushButton>
#include <QSignalBlocker>
#include <QVBoxLayout>
#include <utility>

using namespace SuperShuckie64;

static const char *const PALETTE_NAMES[GBPaletteDialog::PALETTES] = { "Background", "Objects 0", "Objects 1" };
static const char *const SHADE_NAMES[GBPaletteDialog::SHADES] = { "White", "Light", "Dark", "Black" };
// The same names as in the settings file, so a preset reads the same in both.
static const char *const PALETTE_KEYS[GBPaletteDialog::PALETTES] = { "background", "objects_0", "objects_1" };
static const std::uint32_t GRAYS[GBPaletteDialog::SHADES] = { 0xFFFFFF, 0xAAAAAA, 0x555555, 0x000000 };

static const int PRESET_NAME_ROLE = Qt::UserRole;
static const int PRESET_COLORS_ROLE = Qt::UserRole + 1;

// Each color's square in a preset's preview picture.
static const int PREVIEW_CELL = 8;

// What an export file says it is.
static const char *const EXPORT_FORMAT = "supershuckie-gb-color-presets";
static const int EXPORT_VERSION = 1;

static QString hex(std::uint32_t color) {
    return QString("#%1").arg(color & 0xFFFFFF, 6, 16, QChar('0')).toUpper();
}

static std::optional<std::uint32_t> parse_hex(QString text) {
    text = text.trimmed();
    if(text.startsWith('#')) {
        text = text.mid(1);
    }
    if(text.size() != 6) {
        return std::nullopt;
    }
    bool ok = false;
    auto value = text.toUInt(&ok, 16);
    if(!ok) {
        return std::nullopt;
    }
    return value;
}

static QPixmap color_pixmap(std::uint32_t color, int size = 16) {
    QPixmap pixmap(size, size);
    pixmap.fill(QColor(QRgb(color & 0xFFFFFF)));
    return pixmap;
}

static const std::uint32_t *palette_of(const SuperShuckieGBCustomColors &colors, std::size_t palette) {
    switch(palette) {
        case 0: return colors.background;
        case 1: return colors.objects_0;
        default: return colors.objects_1;
    }
}

static std::uint32_t *palette_of(SuperShuckieGBCustomColors &colors, std::size_t palette) {
    return const_cast<std::uint32_t *>(palette_of(static_cast<const SuperShuckieGBCustomColors &>(colors), palette));
}

std::vector<GBColorPreset> GBPaletteDialog::load_presets(const SuperShuckieFrontendRaw *frontend) {
    std::vector<GBColorPreset> presets;
    auto count = supershuckie_frontend_get_gb_color_preset_count(frontend);
    for(std::size_t i = 0; i < count; i++) {
        GBColorPreset preset = {};
        const char *name = supershuckie_frontend_get_gb_color_preset(frontend, i, &preset.colors);
        preset.name = QString::fromUtf8(name);
        presets.push_back(std::move(preset));
    }
    return presets;
}

void GBPaletteDialog::save_presets(SuperShuckieFrontendRaw *frontend, const std::vector<GBColorPreset> &presets) {
    std::vector<QByteArray> names;
    std::vector<SuperShuckieGBCustomColors> colors;
    for(const auto &preset : presets) {
        names.push_back(preset.name.toUtf8());
        colors.push_back(preset.colors);
    }

    std::vector<const char *> name_pointers;
    for(const auto &name : names) {
        name_pointers.push_back(name.constData());
    }

    supershuckie_frontend_set_gb_color_presets(frontend, name_pointers.data(), colors.data(), presets.size());
    supershuckie_frontend_write_settings(frontend);
}

QByteArray GBPaletteDialog::export_presets(const std::vector<GBColorPreset> &presets) {
    QJsonArray list;
    for(const auto &preset : presets) {
        QJsonObject object;
        object["name"] = preset.name;
        for(std::size_t palette = 0; palette < PALETTES; palette++) {
            QJsonArray shades;
            for(std::size_t shade = 0; shade < SHADES; shade++) {
                shades.append(hex(palette_of(preset.colors, palette)[shade]));
            }
            object[PALETTE_KEYS[palette]] = shades;
        }
        list.append(object);
    }

    QJsonObject root;
    root["format"] = EXPORT_FORMAT;
    root["version"] = EXPORT_VERSION;
    root["presets"] = list;
    return QJsonDocument(root).toJson(QJsonDocument::Indented);
}

std::optional<std::vector<GBColorPreset>> GBPaletteDialog::import_presets(const QByteArray &contents, std::size_t &skipped, QString &error) {
    skipped = 0;
    QJsonParseError parse_error = {};
    auto document = QJsonDocument::fromJson(contents, &parse_error);
    if(document.isNull()) {
        error = QString("This is not a color presets file (%1).").arg(parse_error.errorString());
        return std::nullopt;
    }

    // An export file, a settings file passed along as it is, or just a list of presets.
    QJsonArray list;
    auto root = document.object();
    if(document.isArray()) {
        list = document.array();
    }
    else if(root.value("presets").isArray()) {
        list = root.value("presets").toArray();
    }
    else if(root.value("game_boy_settings").toObject().value("color_presets").isArray()) {
        list = root.value("game_boy_settings").toObject().value("color_presets").toArray();
    }
    else if(root.value("color_presets").isArray()) {
        list = root.value("color_presets").toArray();
    }
    else if(root.contains("name") && root.contains(PALETTE_KEYS[0])) {
        list.append(root);
    }
    else {
        error = "This file has no Game Boy color presets in it.";
        return std::nullopt;
    }

    std::vector<GBColorPreset> presets;
    for(const auto &value : list) {
        auto object = value.toObject();
        GBColorPreset preset = {};
        preset.name = object.value("name").toString().trimmed();
        preset.colors.enabled = true;
        bool readable = !preset.name.isEmpty();
        for(std::size_t palette = 0; palette < PALETTES && readable; palette++) {
            auto shades = object.value(PALETTE_KEYS[palette]).toArray();
            if(shades.size() != static_cast<qsizetype>(SHADES)) {
                readable = false;
                break;
            }
            for(std::size_t shade = 0; shade < SHADES; shade++) {
                auto color = parse_hex(shades.at(static_cast<qsizetype>(shade)).toString());
                if(!color.has_value()) {
                    readable = false;
                    break;
                }
                palette_of(preset.colors, palette)[shade] = *color;
            }
        }
        if(readable) {
            presets.push_back(std::move(preset));
        }
        else {
            skipped++;
        }
    }
    return presets;
}

/**
 * Asks which of `presets` to export or import, with all of them ticked to begin with; nothing
 * means the user cancelled.
 */
static std::optional<std::vector<std::size_t>> choose_presets(QWidget *parent, const QString &title, const QString &message, const QString &ok_text, const std::vector<GBColorPreset> &presets) {
    QDialog dialog(parent);
    dialog.setWindowTitle(title);
    auto *layout = new QVBoxLayout(&dialog);
    auto *label = new QLabel(message, &dialog);
    label->setWordWrap(true);
    layout->addWidget(label);

    auto *list = new QListWidget(&dialog);
    list->setObjectName("gb-color-presets-choose");
    list->setIconSize(QSize(static_cast<int>(GBPaletteDialog::SHADES) * PREVIEW_CELL, static_cast<int>(GBPaletteDialog::PALETTES) * PREVIEW_CELL));
    for(const auto &preset : presets) {
        auto *item = new QListWidgetItem(GBPaletteDialog::preview_icon(preset.colors), preset.name, list);
        item->setFlags((item->flags() | Qt::ItemIsUserCheckable) & ~Qt::ItemIsSelectable);
        item->setCheckState(Qt::Checked);
    }
    layout->addWidget(list);

    auto *buttons = new QDialogButtonBox(QDialogButtonBox::Ok | QDialogButtonBox::Cancel, &dialog);
    auto *ok = buttons->button(QDialogButtonBox::Ok);
    ok->setText(ok_text);
    auto *all = buttons->addButton("Select all", QDialogButtonBox::ResetRole);
    auto *none = buttons->addButton("Select none", QDialogButtonBox::ResetRole);
    auto set_all = [list](Qt::CheckState state) {
        for(int i = 0; i < list->count(); i++) {
            list->item(i)->setCheckState(state);
        }
    };
    QObject::connect(all, &QPushButton::clicked, &dialog, [set_all]() { set_all(Qt::Checked); });
    QObject::connect(none, &QPushButton::clicked, &dialog, [set_all]() { set_all(Qt::Unchecked); });
    // At least one has to be ticked.
    QObject::connect(list, &QListWidget::itemChanged, &dialog, [list, ok]() {
        bool any = false;
        for(int i = 0; i < list->count(); i++) {
            any = any || list->item(i)->checkState() == Qt::Checked;
        }
        ok->setEnabled(any);
    });
    QObject::connect(buttons, &QDialogButtonBox::accepted, &dialog, &QDialog::accept);
    QObject::connect(buttons, &QDialogButtonBox::rejected, &dialog, &QDialog::reject);
    layout->addWidget(buttons);

    if(dialog.exec() != QDialog::Accepted) {
        return std::nullopt;
    }
    std::vector<std::size_t> chosen;
    for(int i = 0; i < list->count(); i++) {
        if(list->item(i)->checkState() == Qt::Checked) {
            chosen.push_back(static_cast<std::size_t>(i));
        }
    }
    return chosen;
}

bool GBPaletteDialog::same_colors(const SuperShuckieGBCustomColors &a, const SuperShuckieGBCustomColors &b) {
    for(std::size_t palette = 0; palette < PALETTES; palette++) {
        for(std::size_t shade = 0; shade < SHADES; shade++) {
            if((palette_of(a, palette)[shade] & 0xFFFFFF) != (palette_of(b, palette)[shade] & 0xFFFFFF)) {
                return false;
            }
        }
    }
    return true;
}

QIcon GBPaletteDialog::preview_icon(const SuperShuckieGBCustomColors &colors) {
    QPixmap pixmap(static_cast<int>(SHADES) * PREVIEW_CELL, static_cast<int>(PALETTES) * PREVIEW_CELL);
    QPainter painter(&pixmap);
    for(std::size_t palette = 0; palette < PALETTES; palette++) {
        for(std::size_t shade = 0; shade < SHADES; shade++) {
            auto color = QColor(QRgb(palette_of(colors, palette)[shade] & 0xFFFFFF));
            painter.fillRect(static_cast<int>(shade) * PREVIEW_CELL, static_cast<int>(palette) * PREVIEW_CELL, PREVIEW_CELL, PREVIEW_CELL, color);
        }
    }
    painter.end();
    return QIcon(pixmap);
}

GBPaletteDialog::GBPaletteDialog(MainWindow *main_window): QDialog(main_window), main_window(main_window) {
    this->setWindowTitle("Custom Game Boy colors");
    supershuckie_frontend_get_gb_custom_colors(main_window->frontend, &this->original);
    this->current = this->original;

    auto *layout = new QVBoxLayout(this);

    auto *description = new QLabel(
        "A game running on a Game Boy, or on a Game Boy Color in Game Boy mode (the boot ROM's colors "
        "that Pokémon Red and Blue get), is drawn with these colors instead of its own. Game Boy Color "
        "games and Super Game Boy colors are not affected. Changes show as the game draws its next frame.",
        this
    );
    description->setWordWrap(true);
    layout->addWidget(description);

    auto *presets_box = new QGroupBox("Presets", this);
    presets_box->setToolTip("Settings › Game Boy › Color presets switches to these in one step");
    auto *presets_layout = new QHBoxLayout(presets_box);
    this->presets = new QListWidget(presets_box);
    this->presets->setObjectName("gb-color-presets");
    this->presets->setIconSize(QSize(static_cast<int>(SHADES) * PREVIEW_CELL, static_cast<int>(PALETTES) * PREVIEW_CELL));
    // The order here is the order of Settings › Game Boy › Color presets.
    this->presets->setDragDropMode(QAbstractItemView::InternalMove);
    this->presets->setToolTip("Click a preset to use its colors, or double-click it to use them and close. Drag to reorder.");
    presets_layout->addWidget(this->presets);

    auto *preset_buttons = new QVBoxLayout();
    this->add_preset = new QPushButton("Save as…", presets_box);
    this->add_preset->setObjectName("gb-color-preset-add");
    this->add_preset->setToolTip("Save the colors below as a new preset");
    this->update_preset = new QPushButton("Update", presets_box);
    this->update_preset->setObjectName("gb-color-preset-update");
    this->update_preset->setToolTip("Change the selected preset to the colors below");
    this->rename_preset = new QPushButton("Rename…", presets_box);
    this->rename_preset->setObjectName("gb-color-preset-rename");
    this->remove_preset = new QPushButton("Remove", presets_box);
    this->remove_preset->setObjectName("gb-color-preset-remove");
    auto *import_button = new QPushButton("Import…", presets_box);
    import_button->setObjectName("gb-color-preset-import");
    import_button->setToolTip("Add presets from a file someone exported");
    this->export_button = new QPushButton("Export…", presets_box);
    this->export_button->setObjectName("gb-color-preset-export");
    this->export_button->setToolTip("Save some or all of the presets to a file to give to someone");
    for(auto *button : { this->add_preset, this->update_preset, this->rename_preset, this->remove_preset }) {
        // Return stays with the dialog's own buttons.
        button->setAutoDefault(false);
        preset_buttons->addWidget(button);
    }
    preset_buttons->addStretch();
    for(auto *button : { import_button, this->export_button }) {
        button->setAutoDefault(false);
        preset_buttons->addWidget(button);
    }
    presets_layout->addLayout(preset_buttons);
    layout->addWidget(presets_box);

    auto *picker = new QHBoxLayout();
    auto *eyedropper = new QPushButton("Eyedropper…", this);
    eyedropper->setObjectName("gb-colors-eyedropper");
    eyedropper->setToolTip("Pick a color from anywhere on the screen, even outside Super Shuckie");
    eyedropper->setAutoDefault(false);
    connect(eyedropper, SIGNAL(clicked()), this, SLOT(on_eyedropper()));
    picker->addWidget(eyedropper);

    this->picked_well = new QPushButton(this);
    this->picked_well->setObjectName("gb-colors-picked");
    this->picked_well->setToolTip("The picked color: drag it onto a color below to use it there, or click to choose it yourself");
    this->picked_well->setAutoDefault(false);
    this->picked_well->installEventFilter(this);
    connect(this->picked_well, SIGNAL(clicked()), this, SLOT(on_choose_picked()));
    picker->addWidget(this->picked_well);

    auto *picker_hint = new QLabel("Drag it onto a color or right-click a color to use it; Set row uses it for a whole row.", this);
    picker_hint->setWordWrap(true);
    picker->addWidget(picker_hint, 1);
    layout->addLayout(picker);

    auto *grid = new QGridLayout();
    for(std::size_t palette = 0; palette < PALETTES; palette++) {
        auto *label = new QLabel(PALETTE_NAMES[palette], this);
        label->setAlignment(Qt::AlignCenter);
        grid->addWidget(label, 0, static_cast<int>(palette) + 1);
    }
    for(std::size_t shade = 0; shade < SHADES; shade++) {
        grid->addWidget(new QLabel(SHADE_NAMES[shade], this), static_cast<int>(shade) + 1, 0);
        for(std::size_t palette = 0; palette < PALETTES; palette++) {
            auto *swatch = new QPushButton(this);
            swatch->setObjectName(QString("gb-color-%1-%2").arg(palette).arg(shade));
            swatch->setAutoDefault(false);
            swatch->setToolTip(QString("%1, %2: click to choose a color, right-click for more, or drop the picked color here").arg(PALETTE_NAMES[palette], SHADE_NAMES[shade]));
            swatch->setContextMenuPolicy(Qt::CustomContextMenu);
            swatch->setAcceptDrops(true);
            swatch->installEventFilter(this);
            connect(swatch, &QPushButton::clicked, this, [this, palette, shade]() { this->pick_color(palette, shade); });
            connect(swatch, &QWidget::customContextMenuRequested, this, [this, palette, shade](const QPoint &pos) { this->swatch_menu(palette, shade, pos); });
            this->swatches[palette][shade] = swatch;
            grid->addWidget(swatch, static_cast<int>(shade) + 1, static_cast<int>(palette) + 1);
        }

        auto *set_row = new QPushButton("Set row", this);
        set_row->setObjectName(QString("gb-colors-set-row-%1").arg(shade));
        set_row->setToolTip(QString("Set %1 in all three palettes to the picked color").arg(SHADE_NAMES[shade]));
        set_row->setAutoDefault(false);
        set_row->setAcceptDrops(true);
        set_row->installEventFilter(this);
        connect(set_row, &QPushButton::clicked, this, [this, shade]() { this->use_picked_for_row(shade); });
        this->set_row[shade] = set_row;
        grid->addWidget(set_row, static_cast<int>(shade) + 1, static_cast<int>(PALETTES) + 1);
    }
    layout->addLayout(grid);

    auto *tools = new QHBoxLayout();
    this->use_current = new QPushButton("Use the game's colors", this);
    this->use_current->setObjectName("gb-colors-use-current");
    this->use_current->setToolTip("Start from the colors the running game is drawn with by its own palettes");
    this->use_current->setAutoDefault(false);
    connect(this->use_current, SIGNAL(clicked()), this, SLOT(on_use_current()));
    tools->addWidget(this->use_current);

    auto *grays = new QPushButton("Reset to grays", this);
    grays->setObjectName("gb-colors-reset");
    grays->setAutoDefault(false);
    connect(grays, SIGNAL(clicked()), this, SLOT(on_reset_to_grays()));
    tools->addWidget(grays);

    auto *swap = new QPushButton("Swap Light and Dark", this);
    swap->setObjectName("gb-colors-swap-light-dark");
    swap->setToolTip("Swap the Light and Dark rows of all three palettes");
    swap->setAutoDefault(false);
    connect(swap, SIGNAL(clicked()), this, SLOT(on_swap_light_dark()));
    tools->addWidget(swap);
    tools->addStretch();
    layout->addLayout(tools);

    this->enabled = new QCheckBox("Use custom colors", this);
    this->enabled->setObjectName("gb-colors-enabled");
    this->enabled->setChecked(this->current.enabled);
    connect(this->enabled, SIGNAL(toggled(bool)), this, SLOT(on_enabled_toggled(bool)));
    layout->addWidget(this->enabled);

    auto *buttons = new QDialogButtonBox(QDialogButtonBox::Ok | QDialogButtonBox::Cancel, this);
    connect(buttons, SIGNAL(accepted()), this, SLOT(accept()));
    connect(buttons, SIGNAL(rejected()), this, SLOT(reject()));
    layout->addWidget(buttons);

    // Only a running Game Boy game (or a Game Boy game on a Game Boy Color) has colors to copy.
    SuperShuckieGBCustomColors probe = {};
    this->use_current->setEnabled(supershuckie_frontend_get_gb_palettes(main_window->frontend, &probe));
    this->refresh_swatches();
    this->refresh_picked();

    this->original_presets = GBPaletteDialog::load_presets(main_window->frontend);
    for(const auto &preset : this->original_presets) {
        this->set_preset(new QListWidgetItem(this->presets), preset);
    }
    this->select_matching_preset();

    connect(this->presets, SIGNAL(itemSelectionChanged()), this, SLOT(on_preset_selected()));
    // Clicking the selected preset again puts back its colors after editing them.
    connect(this->presets, SIGNAL(itemClicked(QListWidgetItem *)), this, SLOT(on_preset_selected()));
    connect(this->presets, SIGNAL(itemDoubleClicked(QListWidgetItem *)), this, SLOT(on_preset_double_clicked(QListWidgetItem *)));
    connect(this->add_preset, SIGNAL(clicked()), this, SLOT(on_add_preset()));
    connect(this->update_preset, SIGNAL(clicked()), this, SLOT(on_update_preset()));
    connect(this->rename_preset, SIGNAL(clicked()), this, SLOT(on_rename_preset()));
    connect(this->remove_preset, SIGNAL(clicked()), this, SLOT(on_remove_preset()));
    connect(import_button, SIGNAL(clicked()), this, SLOT(on_import_presets()));
    connect(this->export_button, SIGNAL(clicked()), this, SLOT(on_export_presets()));
    this->refresh_preset_buttons();
}

std::uint32_t &GBPaletteDialog::color(std::size_t palette, std::size_t shade) {
    switch(palette) {
        case 0: return this->current.background[shade];
        case 1: return this->current.objects_0[shade];
        default: return this->current.objects_1[shade];
    }
}

void GBPaletteDialog::refresh_swatch(std::size_t palette, std::size_t shade) {
    auto value = this->color(palette, shade);
    auto *swatch = this->swatches[palette][shade];
    swatch->setIcon(QIcon(color_pixmap(value)));
    swatch->setText(hex(value));
}

void GBPaletteDialog::refresh_swatches() {
    for(std::size_t palette = 0; palette < PALETTES; palette++) {
        for(std::size_t shade = 0; shade < SHADES; shade++) {
            this->refresh_swatch(palette, shade);
        }
    }
}

void GBPaletteDialog::apply() {
    supershuckie_frontend_set_gb_custom_colors(this->main_window->frontend, &this->current);
}

void GBPaletteDialog::colors_edited() {
    this->refresh_swatches();
    this->apply();
    this->refresh_preset_buttons();
}

void GBPaletteDialog::pick_color(std::size_t palette, std::size_t shade) {
    auto &value = this->color(palette, shade);
    auto picked = QColorDialog::getColor(QColor(QRgb(value & 0xFFFFFF)), this, QString("%1, %2").arg(PALETTE_NAMES[palette], SHADE_NAMES[shade]));
    if(!picked.isValid()) {
        return;
    }
    value = static_cast<std::uint32_t>(picked.rgb() & 0xFFFFFF);
    this->colors_edited();
}

void GBPaletteDialog::set_picked(std::uint32_t color) {
    this->picked = color & 0xFFFFFF;
    this->refresh_picked();
}

void GBPaletteDialog::refresh_picked() {
    if(this->picked.has_value()) {
        this->picked_well->setIcon(QIcon(color_pixmap(*this->picked)));
        this->picked_well->setText(hex(*this->picked));
    }
    else {
        this->picked_well->setIcon(QIcon());
        this->picked_well->setText("No color picked");
    }
    for(auto *button : this->set_row) {
        button->setEnabled(this->picked.has_value());
    }
}

void GBPaletteDialog::use_picked(std::size_t palette, std::size_t shade) {
    if(!this->picked.has_value()) {
        return;
    }
    this->color(palette, shade) = *this->picked;
    this->colors_edited();
}

void GBPaletteDialog::use_picked_for_row(std::size_t shade) {
    if(!this->picked.has_value()) {
        return;
    }
    for(std::size_t palette = 0; palette < PALETTES; palette++) {
        this->color(palette, shade) = *this->picked;
    }
    this->colors_edited();
}

void GBPaletteDialog::on_eyedropper() {
    auto picked = pick_screen_color(this);
    if(picked.has_value()) {
        this->set_picked(static_cast<std::uint32_t>(picked->rgb() & 0xFFFFFF));
    }
}

void GBPaletteDialog::eyedropper_into(std::size_t palette, std::size_t shade) {
    auto picked = pick_screen_color(this);
    if(!picked.has_value()) {
        return;
    }
    this->set_picked(static_cast<std::uint32_t>(picked->rgb() & 0xFFFFFF));
    this->use_picked(palette, shade);
}

void GBPaletteDialog::on_choose_picked() {
    auto start = this->picked.has_value() ? QColor(QRgb(*this->picked)) : QColor(Qt::white);
    auto chosen = QColorDialog::getColor(start, this, "Picked color");
    if(chosen.isValid()) {
        this->set_picked(static_cast<std::uint32_t>(chosen.rgb() & 0xFFFFFF));
    }
}

void GBPaletteDialog::swatch_menu(std::size_t palette, std::size_t shade, const QPoint &pos) {
    QMenu menu(this);
    auto picked_text = this->picked.has_value() ? hex(*this->picked) : QString("none yet");
    auto *use = menu.addAction(QString("Use the picked color (%1)").arg(picked_text));
    use->setEnabled(this->picked.has_value());
    auto *use_row = menu.addAction(QString("Use the picked color for the whole %1 row").arg(SHADE_NAMES[shade]));
    use_row->setEnabled(this->picked.has_value());
    menu.addSeparator();
    auto *eyedropper = menu.addAction("Eyedropper…");
    auto *choose = menu.addAction("Choose color…");
    auto *keep = menu.addAction(QString("Make %1 the picked color").arg(hex(this->color(palette, shade))));

    auto *chosen = menu.exec(this->swatches[palette][shade]->mapToGlobal(pos));
    if(chosen == use) {
        this->use_picked(palette, shade);
    }
    else if(chosen == use_row) {
        this->use_picked_for_row(shade);
    }
    else if(chosen == eyedropper) {
        this->eyedropper_into(palette, shade);
    }
    else if(chosen == choose) {
        this->pick_color(palette, shade);
    }
    else if(chosen == keep) {
        this->set_picked(this->color(palette, shade));
    }
}

std::optional<std::pair<std::size_t, std::size_t>> GBPaletteDialog::swatch_of(const QObject *object) const {
    for(std::size_t palette = 0; palette < PALETTES; palette++) {
        for(std::size_t shade = 0; shade < SHADES; shade++) {
            if(this->swatches[palette][shade] == object) {
                return std::make_pair(palette, shade);
            }
        }
    }
    return std::nullopt;
}

bool GBPaletteDialog::eventFilter(QObject *watched, QEvent *event) {
    // Dragging the picked color out of its well.
    if(watched == this->picked_well && this->picked.has_value()) {
        if(event->type() == QEvent::MouseButtonPress) {
            auto *mouse = static_cast<QMouseEvent *>(event);
            if(mouse->button() == Qt::LeftButton) {
                this->drag_start = mouse->position().toPoint();
            }
        }
        else if(event->type() == QEvent::MouseMove) {
            auto *mouse = static_cast<QMouseEvent *>(event);
            if((mouse->buttons() & Qt::LeftButton) && (mouse->position().toPoint() - this->drag_start).manhattanLength() >= QApplication::startDragDistance()) {
                auto *mime = new QMimeData();
                mime->setColorData(QColor(QRgb(*this->picked)));
                mime->setText(hex(*this->picked));
                auto *drag = new QDrag(this->picked_well);
                drag->setMimeData(mime);
                drag->setPixmap(color_pixmap(*this->picked, 24));
                // The well would otherwise stay pressed down, and click when the mouse comes back.
                this->picked_well->setDown(false);
                drag->exec(Qt::CopyAction);
                return true;
            }
        }
        return QDialog::eventFilter(watched, event);
    }

    // Dropping it on a color or a row.
    std::size_t set_row_shade = SHADES;
    for(std::size_t shade = 0; shade < SHADES; shade++) {
        if(this->set_row[shade] == watched) {
            set_row_shade = shade;
        }
    }
    auto swatch = this->swatch_of(watched);
    if(!swatch.has_value() && set_row_shade == SHADES) {
        return QDialog::eventFilter(watched, event);
    }

    if(event->type() == QEvent::DragEnter) {
        auto *drag = static_cast<QDragEnterEvent *>(event);
        if(drag->mimeData()->hasColor()) {
            drag->acceptProposedAction();
            return true;
        }
    }
    else if(event->type() == QEvent::Drop) {
        auto *drop = static_cast<QDropEvent *>(event);
        if(drop->mimeData()->hasColor()) {
            auto color = qvariant_cast<QColor>(drop->mimeData()->colorData());
            this->set_picked(static_cast<std::uint32_t>(color.rgb() & 0xFFFFFF));
            if(swatch.has_value()) {
                this->use_picked(swatch->first, swatch->second);
            }
            else {
                this->use_picked_for_row(set_row_shade);
            }
            drop->acceptProposedAction();
            return true;
        }
    }
    return QDialog::eventFilter(watched, event);
}

void GBPaletteDialog::on_swap_light_dark() {
    // Light and Dark are the middle two shades.
    for(std::size_t palette = 0; palette < PALETTES; palette++) {
        std::swap(this->color(palette, 1), this->color(palette, 2));
    }
    this->colors_edited();
}

void GBPaletteDialog::on_enabled_toggled(bool on) {
    this->current.enabled = on;
    this->apply();
}

void GBPaletteDialog::on_use_current() {
    SuperShuckieGBCustomColors palettes = {};
    if(!supershuckie_frontend_get_gb_palettes(this->main_window->frontend, &palettes)) {
        this->use_current->setEnabled(false);
        return;
    }
    for(std::size_t shade = 0; shade < SHADES; shade++) {
        this->current.background[shade] = palettes.background[shade];
        this->current.objects_0[shade] = palettes.objects_0[shade];
        this->current.objects_1[shade] = palettes.objects_1[shade];
    }
    this->colors_edited();
}

void GBPaletteDialog::on_reset_to_grays() {
    for(std::size_t palette = 0; palette < PALETTES; palette++) {
        for(std::size_t shade = 0; shade < SHADES; shade++) {
            this->color(palette, shade) = GRAYS[shade];
        }
    }
    this->colors_edited();
}

void GBPaletteDialog::set_preset(QListWidgetItem *item, const GBColorPreset &preset) {
    QVariantList colors;
    for(std::size_t palette = 0; palette < PALETTES; palette++) {
        for(std::size_t shade = 0; shade < SHADES; shade++) {
            colors.append(palette_of(preset.colors, palette)[shade]);
        }
    }
    item->setData(PRESET_NAME_ROLE, preset.name);
    item->setData(PRESET_COLORS_ROLE, colors);
    item->setText(preset.name);
    item->setIcon(GBPaletteDialog::preview_icon(preset.colors));
}

GBColorPreset GBPaletteDialog::preset_at(const QListWidgetItem *item) const {
    GBColorPreset preset = {};
    preset.name = item->data(PRESET_NAME_ROLE).toString();
    preset.colors.enabled = true;

    auto colors = item->data(PRESET_COLORS_ROLE).toList();
    for(std::size_t shade = 0; shade < SHADES; shade++) {
        preset.colors.background[shade] = colors.value(static_cast<int>(0 * SHADES + shade)).toUInt();
        preset.colors.objects_0[shade] = colors.value(static_cast<int>(1 * SHADES + shade)).toUInt();
        preset.colors.objects_1[shade] = colors.value(static_cast<int>(2 * SHADES + shade)).toUInt();
    }
    return preset;
}

std::vector<GBColorPreset> GBPaletteDialog::current_presets() const {
    std::vector<GBColorPreset> presets;
    for(int i = 0; i < this->presets->count(); i++) {
        presets.push_back(this->preset_at(this->presets->item(i)));
    }
    return presets;
}

bool GBPaletteDialog::presets_changed() const {
    auto presets = this->current_presets();
    if(presets.size() != this->original_presets.size()) {
        return true;
    }
    for(std::size_t i = 0; i < presets.size(); i++) {
        if(presets[i].name != this->original_presets[i].name || !GBPaletteDialog::same_colors(presets[i].colors, this->original_presets[i].colors)) {
            return true;
        }
    }
    return false;
}

QListWidgetItem *GBPaletteDialog::selected_preset() const {
    auto selected = this->presets->selectedItems();
    return selected.isEmpty() ? nullptr : selected.first();
}

void GBPaletteDialog::select_matching_preset() {
    // Show which preset (if any) the colors are, without switching to it.
    const QSignalBlocker blocker(this->presets);
    this->presets->clearSelection();
    for(int i = 0; i < this->presets->count(); i++) {
        auto *item = this->presets->item(i);
        if(GBPaletteDialog::same_colors(this->preset_at(item).colors, this->current)) {
            this->presets->setCurrentItem(item);
            break;
        }
    }
}

void GBPaletteDialog::refresh_preset_buttons() {
    this->export_button->setEnabled(this->presets->count() > 0);
    auto *item = this->selected_preset();
    this->update_preset->setEnabled(item != nullptr && !GBPaletteDialog::same_colors(this->preset_at(item).colors, this->current));
    this->rename_preset->setEnabled(item != nullptr);
    this->remove_preset->setEnabled(item != nullptr);
}

void GBPaletteDialog::on_preset_selected() {
    auto *item = this->selected_preset();
    if(item == nullptr) {
        this->refresh_preset_buttons();
        return;
    }

    // Picking a preset means using it, so it also switches custom colors on.
    this->current = this->preset_at(item).colors;
    this->current.enabled = true;
    {
        const QSignalBlocker blocker(this->enabled);
        this->enabled->setChecked(true);
    }
    this->colors_edited();
}

void GBPaletteDialog::on_preset_double_clicked(QListWidgetItem *item) {
    this->presets->setCurrentItem(item);
    this->on_preset_selected();
    this->accept();
}

void GBPaletteDialog::on_add_preset() {
    auto suggestion = QString("Colors %1").arg(this->presets->count() + 1);
    auto answer = AskForTextDialog::ask(this->main_window, "Save color preset", "Enter a name for these colors", "", suggestion);
    if(!answer.has_value() || QString::fromStdString(*answer).trimmed().isEmpty()) {
        return;
    }

    GBColorPreset preset = {};
    preset.name = QString::fromStdString(*answer).trimmed();
    preset.colors = this->current;

    // One preset per name, so a name in the menu always means one set of colors.
    QListWidgetItem *item = nullptr;
    for(int i = 0; i < this->presets->count(); i++) {
        if(this->preset_at(this->presets->item(i)).name == preset.name) {
            item = this->presets->item(i);
            break;
        }
    }
    if(item != nullptr) {
        auto replace = QMessageBox::question(this, this->windowTitle(), QString("There is already a preset called “%1”. Replace its colors?").arg(preset.name));
        if(replace != QMessageBox::Yes) {
            return;
        }
    }
    else {
        item = new QListWidgetItem(this->presets);
    }

    this->set_preset(item, preset);
    {
        // Its colors are the ones being edited already.
        const QSignalBlocker blocker(this->presets);
        this->presets->setCurrentItem(item);
    }
    this->refresh_preset_buttons();
}

void GBPaletteDialog::on_update_preset() {
    auto *item = this->selected_preset();
    if(item == nullptr) {
        return;
    }

    auto preset = this->preset_at(item);
    preset.colors = this->current;
    this->set_preset(item, preset);
    this->refresh_preset_buttons();
}

void GBPaletteDialog::on_rename_preset() {
    auto *item = this->selected_preset();
    if(item == nullptr) {
        return;
    }

    auto preset = this->preset_at(item);
    auto answer = AskForTextDialog::ask(this->main_window, "Rename color preset", "Enter a new name for these colors", "", preset.name);
    if(!answer.has_value() || QString::fromStdString(*answer).trimmed().isEmpty()) {
        return;
    }
    auto name = QString::fromStdString(*answer).trimmed();
    for(int i = 0; i < this->presets->count(); i++) {
        if(this->presets->item(i) != item && this->preset_at(this->presets->item(i)).name == name) {
            QMessageBox::warning(this, this->windowTitle(), QString("There is already a preset called “%1”.").arg(name));
            return;
        }
    }
    preset.name = name;
    this->set_preset(item, preset);
}

void GBPaletteDialog::on_remove_preset() {
    auto *item = this->selected_preset();
    if(item == nullptr) {
        return;
    }

    {
        // Removing the selected row would otherwise select a neighbour and switch to its colors.
        const QSignalBlocker blocker(this->presets);
        delete this->presets->takeItem(this->presets->row(item));
        this->presets->clearSelection();
    }
    this->refresh_preset_buttons();
}

void GBPaletteDialog::on_export_presets() {
    auto presets = this->current_presets();
    if(presets.empty()) {
        return;
    }
    auto chosen = choose_presets(this, "Export color presets", "Choose the presets to export.", "Export…", presets);
    if(!chosen.has_value()) {
        return;
    }

    auto path = QFileDialog::getSaveFileName(this, "Export color presets", "color-presets.json", "Color presets (*.json)");
    if(path.isEmpty()) {
        return;
    }
    std::vector<GBColorPreset> exported;
    for(auto index : *chosen) {
        exported.push_back(presets[index]);
    }

    QFile file(path);
    auto contents = GBPaletteDialog::export_presets(exported);
    if(!file.open(QIODevice::WriteOnly | QIODevice::Truncate) || file.write(contents) != contents.size()) {
        QMessageBox::warning(this, "Export color presets", QString("Could not write %1: %2").arg(path, file.errorString()));
        return;
    }
    file.close();
    QMessageBox::information(this, "Export color presets", QString(exported.size() == 1 ? "Exported 1 preset." : "Exported %1 presets.").arg(exported.size()));
}

void GBPaletteDialog::on_import_presets() {
    auto path = QFileDialog::getOpenFileName(this, "Import color presets", QString(), "Color presets (*.json);;All files (*)");
    if(path.isEmpty()) {
        return;
    }

    QFile file(path);
    if(!file.open(QIODevice::ReadOnly)) {
        QMessageBox::warning(this, "Import color presets", QString("Could not read %1: %2").arg(path, file.errorString()));
        return;
    }
    std::size_t skipped = 0;
    QString error;
    auto imported = GBPaletteDialog::import_presets(file.readAll(), skipped, error);
    file.close();
    if(!imported.has_value()) {
        QMessageBox::warning(this, "Import color presets", error);
        return;
    }
    if(imported->empty()) {
        QMessageBox::warning(this, "Import color presets", skipped == 0 ? QString("This file has no presets in it.") : QString("None of the presets in this file could be read."));
        return;
    }

    if(imported->size() > 1) {
        auto chosen = choose_presets(this, "Import color presets", "Choose the presets to import.", "Import", *imported);
        if(!chosen.has_value()) {
            return;
        }
        std::vector<GBColorPreset> picked;
        for(auto index : *chosen) {
            picked.push_back((*imported)[index]);
        }
        *imported = std::move(picked);
    }

    auto find = [this](const QString &name) -> QListWidgetItem * {
        for(int i = 0; i < this->presets->count(); i++) {
            if(this->preset_at(this->presets->item(i)).name == name) {
                return this->presets->item(i);
            }
        }
        return nullptr;
    };

    // Same name and colors is a preset already here; same name, other colors needs a decision.
    QStringList clashes;
    for(const auto &preset : *imported) {
        auto *existing = find(preset.name);
        if(existing != nullptr && !GBPaletteDialog::same_colors(this->preset_at(existing).colors, preset.colors)) {
            clashes.append(preset.name);
        }
    }

    enum class Clash { Replace, KeepBoth, Skip } clash = Clash::Skip;
    if(!clashes.isEmpty()) {
        QMessageBox box(this);
        box.setIcon(QMessageBox::Question);
        box.setWindowTitle("Import color presets");
        box.setText(clashes.size() == 1
            ? QString("You already have a preset called “%1” with other colors.").arg(clashes.first())
            : QString("You already have presets called %1 with other colors.").arg("“" + clashes.join("”, “") + "”"));
        box.setInformativeText("Replace yours with the imported ones, keep both (the imported ones get a number after their names), or skip them?");
        auto *replace = box.addButton("Replace", QMessageBox::DestructiveRole);
        auto *keep_both = box.addButton("Keep both", QMessageBox::AcceptRole);
        auto *skip = box.addButton("Skip", QMessageBox::RejectRole);
        box.addButton(QMessageBox::Cancel);
        box.setDefaultButton(keep_both);
        box.exec();
        if(box.clickedButton() == replace) {
            clash = Clash::Replace;
        }
        else if(box.clickedButton() == keep_both) {
            clash = Clash::KeepBoth;
        }
        else if(box.clickedButton() != skip) {
            return;
        }
    }

    std::size_t added = 0;
    std::size_t replaced = 0;
    std::size_t already = 0;
    for(auto preset : *imported) {
        auto *existing = find(preset.name);
        if(existing != nullptr && GBPaletteDialog::same_colors(this->preset_at(existing).colors, preset.colors)) {
            already++;
            continue;
        }
        if(existing != nullptr) {
            if(clash == Clash::Skip) {
                continue;
            }
            if(clash == Clash::Replace) {
                this->set_preset(existing, preset);
                replaced++;
                continue;
            }
            auto base = preset.name;
            for(int number = 2; find(preset.name) != nullptr; number++) {
                preset.name = QString("%1 (%2)").arg(base).arg(number);
            }
        }
        this->set_preset(new QListWidgetItem(this->presets), preset);
        added++;
    }
    this->refresh_preset_buttons();

    QStringList summary;
    if(added > 0) {
        summary.append(QString(added == 1 ? "Added 1 preset." : "Added %1 presets.").arg(added));
    }
    if(replaced > 0) {
        summary.append(QString(replaced == 1 ? "Replaced 1 preset." : "Replaced %1 presets.").arg(replaced));
    }
    if(already > 0) {
        summary.append(QString(already == 1 ? "1 preset was already here." : "%1 presets were already here.").arg(already));
    }
    if(skipped > 0) {
        summary.append(QString(skipped == 1 ? "1 preset in the file could not be read." : "%1 presets in the file could not be read.").arg(skipped));
    }
    if(added + replaced > 0) {
        summary.append("They are kept when you click OK.");
    }
    else if(summary.isEmpty()) {
        summary.append("Nothing was imported.");
    }
    QMessageBox::information(this, "Import color presets", summary.join(" "));
}

void GBPaletteDialog::accept() {
    this->apply();
    if(this->presets_changed()) {
        GBPaletteDialog::save_presets(this->main_window->frontend, this->current_presets());
    }
    QDialog::accept();
}

void GBPaletteDialog::reject() {
    if(this->presets_changed()) {
        QMessageBox box(this);
        box.setIcon(QMessageBox::Question);
        box.setWindowTitle(this->windowTitle());
        box.setText("Discard your changes to the color presets?");
        auto *discard = box.addButton("Discard changes", QMessageBox::DestructiveRole);
        auto *keep = box.addButton("Keep editing", QMessageBox::RejectRole);
        box.setDefaultButton(keep);
        box.exec();
        if(box.clickedButton() != discard) {
            return;
        }
    }
    supershuckie_frontend_set_gb_custom_colors(this->main_window->frontend, &this->original);
    QDialog::reject();
}
