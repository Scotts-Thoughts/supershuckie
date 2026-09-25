#include "gb_palette_dialog.hpp"
#include "main_window.hpp"

#include <QCheckBox>
#include <QColorDialog>
#include <QDialogButtonBox>
#include <QGridLayout>
#include <QHBoxLayout>
#include <QLabel>
#include <QPixmap>
#include <QPushButton>
#include <QVBoxLayout>

using namespace SuperShuckie64;

static const char *const PALETTE_NAMES[GBPaletteDialog::PALETTES] = { "Background", "Objects 0", "Objects 1" };
static const char *const SHADE_NAMES[GBPaletteDialog::SHADES] = { "White", "Light", "Dark", "Black" };
static const std::uint32_t GRAYS[GBPaletteDialog::SHADES] = { 0xFFFFFF, 0xAAAAAA, 0x555555, 0x000000 };

static QString hex(std::uint32_t color) {
    return QString("#%1").arg(color & 0xFFFFFF, 6, 16, QChar('0')).toUpper();
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

void GBPaletteDialog::pick_color(std::size_t palette, std::size_t shade) {
    auto &value = this->color(palette, shade);
    auto picked = QColorDialog::getColor(QColor(QRgb(value & 0xFFFFFF)), this, QString("%1, %2").arg(PALETTE_NAMES[palette], SHADE_NAMES[shade]));
    if(!picked.isValid()) {
        return;
    }
    value = static_cast<std::uint32_t>(picked.rgb() & 0xFFFFFF);
    this->refresh_swatch(palette, shade);
    this->apply();
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
    this->refresh_swatches();
    this->apply();
}

void GBPaletteDialog::on_reset_to_grays() {
    for(std::size_t palette = 0; palette < PALETTES; palette++) {
        for(std::size_t shade = 0; shade < SHADES; shade++) {
            this->color(palette, shade) = GRAYS[shade];
        }
    }
    this->refresh_swatches();
    this->apply();
}

void GBPaletteDialog::accept() {
    this->apply();
    QDialog::accept();
}

void GBPaletteDialog::reject() {
    supershuckie_frontend_set_gb_custom_colors(this->main_window->frontend, &this->original);
    QDialog::reject();
}
