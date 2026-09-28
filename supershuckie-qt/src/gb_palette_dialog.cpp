#include "gb_palette_dialog.hpp"
#include "main_window.hpp"
#include "ask_for_text_dialog.hpp"

#include <QCheckBox>
#include <QColorDialog>
#include <QDialogButtonBox>
#include <QGridLayout>
#include <QGroupBox>
#include <QHBoxLayout>
#include <QLabel>
#include <QListWidget>
#include <QMessageBox>
#include <QPainter>
#include <QPixmap>
#include <QPushButton>
#include <QSignalBlocker>
#include <QVBoxLayout>

using namespace SuperShuckie64;

static const char *const PALETTE_NAMES[GBPaletteDialog::PALETTES] = { "Background", "Objects 0", "Objects 1" };
static const char *const SHADE_NAMES[GBPaletteDialog::SHADES] = { "White", "Light", "Dark", "Black" };
static const std::uint32_t GRAYS[GBPaletteDialog::SHADES] = { 0xFFFFFF, 0xAAAAAA, 0x555555, 0x000000 };

static const int PRESET_NAME_ROLE = Qt::UserRole;
static const int PRESET_COLORS_ROLE = Qt::UserRole + 1;

// Each color's square in a preset's preview picture.
static const int PREVIEW_CELL = 8;

static QString hex(std::uint32_t color) {
    return QString("#%1").arg(color & 0xFFFFFF, 6, 16, QChar('0')).toUpper();
}

static const std::uint32_t *palette_of(const SuperShuckieGBCustomColors &colors, std::size_t palette) {
    switch(palette) {
        case 0: return colors.background;
        case 1: return colors.objects_0;
        default: return colors.objects_1;
    }
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
    for(auto *button : { this->add_preset, this->update_preset, this->rename_preset, this->remove_preset }) {
        // Return stays with the dialog's own buttons.
        button->setAutoDefault(false);
        preset_buttons->addWidget(button);
    }
    preset_buttons->addStretch();
    presets_layout->addLayout(preset_buttons);
    layout->addWidget(presets_box);

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
            swatch->setToolTip(QString("%1, %2: click to pick a color").arg(PALETTE_NAMES[palette], SHADE_NAMES[shade]));
            connect(swatch, &QPushButton::clicked, this, [this, palette, shade]() { this->pick_color(palette, shade); });
            this->swatches[palette][shade] = swatch;
            grid->addWidget(swatch, static_cast<int>(shade) + 1, static_cast<int>(palette) + 1);
        }
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
    QPixmap pixmap(16, 16);
    pixmap.fill(QColor(QRgb(value & 0xFFFFFF)));
    auto *swatch = this->swatches[palette][shade];
    swatch->setIcon(QIcon(pixmap));
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
