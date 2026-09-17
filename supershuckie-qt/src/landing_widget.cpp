#include "landing_widget.hpp"
#include "main_window.hpp"
#include "ask_for_text_dialog.hpp"
#include "error.hpp"

#include <QVBoxLayout>
#include <QHBoxLayout>
#include <QPushButton>
#include <QGridLayout>
#include <QScrollArea>
#include <QToolButton>
#include <QLabel>
#include <QMenu>
#include <QFileDialog>
#include <QFileInfo>
#include <QDir>
#include <QDateTime>
#include <QImage>
#include <QImageReader>
#include <QPainter>
#include <QMimeData>
#include <QDragEnterEvent>
#include <QDragMoveEvent>
#include <QDropEvent>
#include <QResizeEvent>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QFontMetrics>
#include <QScrollBar>
#include <QStyle>
#include <QDrag>
#include <QMouseEvent>
#include <QApplication>

#include <filesystem>

using namespace SuperShuckie64;

static const char *FAVORITE_ROMS = "qt__favorite_roms";
static const char *LANDING_HINT_DISMISSED = "qt__landing_hint_dismissed";

// Mime type of an in-progress tile reorder; the data is the source tile's index as decimal text.
static const char *TILE_MIME = "application/x-supershuckie-favorite-tile";

LandingWidget::LandingWidget(MainWindow *window, QWidget *parent): QWidget(parent), main_window(window) {
    this->setAcceptDrops(true);

    auto *layout = new QVBoxLayout(this);
    layout->setContentsMargins(16, 12, 16, 8);
    layout->setSpacing(8);

    this->scroll_area = new QScrollArea(this);
    this->scroll_area->setWidgetResizable(true);
    this->scroll_area->setFrameStyle(0);
    this->scroll_area->setHorizontalScrollBarPolicy(Qt::ScrollBarAlwaysOff);
    this->scroll_area->setVerticalScrollBarPolicy(Qt::ScrollBarAsNeeded);
    this->scroll_area->viewport()->setAutoFillBackground(false);

    this->tile_container = new QWidget(this->scroll_area);
    this->tile_container->setAutoFillBackground(false);
    this->tile_grid = new QGridLayout(this->tile_container);
    this->tile_grid->setContentsMargins(0, 0, 0, 0);
    this->tile_grid->setHorizontalSpacing(TILE_SPACING);
    this->tile_grid->setVerticalSpacing(TILE_SPACING);
    this->tile_grid->setAlignment(Qt::AlignTop | Qt::AlignLeft);
    this->scroll_area->setWidget(this->tile_container);
    layout->addWidget(this->scroll_area, 1);

    // One-time help text; "Dismiss" hides it for good (see do_dismiss_hint()).
    this->hint_row = new QWidget(this);
    auto *hint_layout = new QHBoxLayout(this->hint_row);
    hint_layout->setContentsMargins(0, 0, 0, 0);
    hint_layout->setSpacing(8);
    this->hint = new QLabel(
        "Click a game to play it. Drag games to reorder them. Right-click a game to give it a picture, "
        "rename it, or remove it. Drop a ROM anywhere here to open it, or drop a picture on a game to "
        "use it as that game's icon.",
        this->hint_row
    );
    this->hint->setWordWrap(true);
    this->hint->setEnabled(false); // draws in the palette's disabled (dimmer) text colour
    hint_layout->addWidget(this->hint, 1);
    auto *dismiss = new QPushButton("Dismiss", this->hint_row);
    dismiss->setToolTip("Hide this help text permanently");
    dismiss->setFocusPolicy(Qt::NoFocus);
    hint_layout->addWidget(dismiss, 0, Qt::AlignBottom);
    connect(dismiss, SIGNAL(clicked()), this, SLOT(do_dismiss_hint()));
    layout->addWidget(this->hint_row);
    this->hint_row->hide();

    this->reload();
}

void LandingWidget::reload() {
    bool dismissed = false;
    if(this->main_window->frontend != nullptr) {
        const char *setting = supershuckie_frontend_get_custom_setting(this->main_window->frontend, LANDING_HINT_DISMISSED);
        dismissed = setting != nullptr && setting[0] == '1';
    }
    this->hint_row->setVisible(this->main_window->frontend != nullptr && !dismissed);

    this->load_favorites();
    this->rebuild_tiles();
}

void LandingWidget::do_dismiss_hint() {
    this->hint_row->hide();
    if(this->main_window->frontend != nullptr) {
        supershuckie_frontend_set_custom_setting(this->main_window->frontend, LANDING_HINT_DISMISSED, "1");
        supershuckie_frontend_write_settings(this->main_window->frontend);
    }
}

QString LandingWidget::icons_dir() const {
    return this->main_window->app_dir + "/favorite-icons";
}

void LandingWidget::load_favorites() {
    this->favorites.clear();

    if(this->main_window->frontend == nullptr) {
        return;
    }

    const char *setting = supershuckie_frontend_get_custom_setting(this->main_window->frontend, FAVORITE_ROMS);
    if(setting == nullptr) {
        return;
    }

    // The FFI pointer is only valid until the next API call, so parse it right away.
    auto document = QJsonDocument::fromJson(QByteArray(setting));
    if(!document.isArray()) {
        return;
    }

    for(const auto &value : document.array()) {
        auto object = value.toObject();
        Favorite favorite;
        favorite.path = object.value("path").toString();
        favorite.name = object.value("name").toString();
        favorite.image = object.value("image").toString();
        if(favorite.path.isEmpty()) {
            continue;
        }
        if(favorite.name.isEmpty()) {
            favorite.name = QFileInfo(favorite.path).completeBaseName();
        }
        this->favorites.emplace_back(std::move(favorite));
    }
}

void LandingWidget::save_favorites() {
    if(this->main_window->frontend == nullptr) {
        return;
    }

    QJsonArray array;
    for(const auto &favorite : this->favorites) {
        QJsonObject object;
        object["path"] = favorite.path;
        object["name"] = favorite.name;
        object["image"] = favorite.image;
        array.append(object);
    }

    if(array.isEmpty()) {
        supershuckie_frontend_set_custom_setting(this->main_window->frontend, FAVORITE_ROMS, nullptr);
    }
    else {
        auto json = QJsonDocument(array).toJson(QJsonDocument::Compact);
        supershuckie_frontend_set_custom_setting(this->main_window->frontend, FAVORITE_ROMS, json.constData());
    }

    // Persist immediately so a crash later on doesn't lose the edit.
    supershuckie_frontend_write_settings(this->main_window->frontend);
}

bool LandingWidget::looks_like_image(const QString &path) {
    auto suffix = QFileInfo(path).suffix().toLower().toUtf8();
    for(const auto &format : QImageReader::supportedImageFormats()) {
        if(format.toLower() == suffix) {
            return true;
        }
    }
    return false;
}

QPixmap LandingWidget::generated_icon(const Favorite &favorite) {
    // A rounded "cartridge" tinted by console, labelled with the file type, so games without a
    // picture are still told apart at a glance.
    auto suffix = QFileInfo(favorite.path).suffix().toLower();
    QColor base;
    QString label;
    if(suffix == "gb") {
        base = QColor(0x6A, 0x8E, 0x3A);
        label = "GB";
    }
    else if(suffix == "gbc") {
        base = QColor(0x7B, 0x4F, 0xB5);
        label = "GBC";
    }
    else if(suffix == "gba") {
        base = QColor(0x3D, 0x5A, 0xB8);
        label = "GBA";
    }
    else if(suffix == "nds") {
        base = QColor(0x4A, 0x4A, 0x52);
        label = "NDS";
    }
    else {
        base = QColor(0x80, 0x80, 0x80);
        label = "ROM";
    }

    QPixmap pixmap(ICON_SIZE, ICON_SIZE);
    pixmap.fill(Qt::transparent);

    QPainter painter(&pixmap);
    painter.setRenderHint(QPainter::Antialiasing);

    QRectF body(4, 8, ICON_SIZE - 8, ICON_SIZE - 12);
    painter.setPen(QPen(base.darker(140), 2));
    painter.setBrush(base);
    painter.drawRoundedRect(body, 10, 10);

    // A lighter label area, like a cartridge sticker
    QRectF sticker(body.adjusted(10, 12, -10, -22));
    painter.setPen(Qt::NoPen);
    painter.setBrush(base.lighter(150));
    painter.drawRoundedRect(sticker, 6, 6);

    QFont font = painter.font();
    font.setBold(true);
    font.setPixelSize(22);
    painter.setFont(font);
    painter.setPen(QColor(0xFF, 0xFF, 0xFF));
    painter.drawText(sticker, Qt::AlignCenter, label);

    // A first-letter monogram under the sticker
    font.setPixelSize(13);
    painter.setFont(font);
    painter.setPen(QColor(0xFF, 0xFF, 0xFF, 0xCC));
    QString monogram = favorite.name.trimmed().left(1).toUpper();
    painter.drawText(QRectF(body.left(), sticker.bottom(), body.width(), body.bottom() - sticker.bottom()), Qt::AlignCenter, monogram);
    painter.end();

    return pixmap;
}

QPixmap LandingWidget::icon_for(const Favorite &favorite) const {
    if(!favorite.image.isEmpty()) {
        QPixmap custom(this->icons_dir() + "/" + favorite.image);
        if(!custom.isNull()) {
            return custom;
        }
    }
    return generated_icon(favorite);
}

void LandingWidget::rebuild_tiles() {
    // This runs from inside a tile's own clicked/context-menu signal (e.g. "Remove"), so the old
    // buttons must outlive the emission: hide them now, free them once control is back in the
    // event loop.
    for(auto *tile : this->tiles) {
        this->tile_grid->removeWidget(tile);
        tile->hide();
        tile->deleteLater();
    }
    this->tiles.clear();
    if(this->add_tile != nullptr) {
        this->tile_grid->removeWidget(this->add_tile);
        this->add_tile->hide();
        this->add_tile->deleteLater();
        this->add_tile = nullptr;
    }

    QFontMetrics metrics(this->font());

    for(std::size_t i = 0; i < this->favorites.size(); i++) {
        const auto &favorite = this->favorites[i];

        auto *tile = new QToolButton(this->tile_container);
        tile->setToolButtonStyle(Qt::ToolButtonTextUnderIcon);
        tile->setIconSize(QSize(ICON_SIZE, ICON_SIZE));
        tile->setFixedSize(TILE_WIDTH, TILE_HEIGHT);
        tile->setIcon(QIcon(this->icon_for(favorite)));
        tile->setText(metrics.elidedText(favorite.name, Qt::ElideRight, TILE_WIDTH - 12));
        tile->setToolTip(favorite.name + "\n" + QDir::toNativeSeparators(favorite.path));
        tile->setCursor(Qt::PointingHandCursor);
        tile->setContextMenuPolicy(Qt::CustomContextMenu);
        tile->setFocusPolicy(Qt::NoFocus);

        tile->setProperty("favorite_index", static_cast<uint>(i));
        tile->installEventFilter(this); // click-drag to reorder

        connect(tile, &QToolButton::clicked, this, [this, i]() { this->open_favorite(i); });
        connect(tile, &QToolButton::customContextMenuRequested, this, [this, i, tile](const QPoint &pos) {
            this->show_tile_menu(i, tile->mapToGlobal(pos));
        });

        this->tiles.push_back(tile);
    }

    this->add_tile = new QToolButton(this->tile_container);
    this->add_tile->setToolButtonStyle(Qt::ToolButtonTextUnderIcon);
    this->add_tile->setIconSize(QSize(ICON_SIZE, ICON_SIZE));
    this->add_tile->setFixedSize(TILE_WIDTH, TILE_HEIGHT);
    this->add_tile->setText("Add ROM…");
    this->add_tile->setToolTip("Add a frequently played ROM to this screen");
    this->add_tile->setCursor(Qt::PointingHandCursor);
    this->add_tile->setFocusPolicy(Qt::NoFocus);
    {
        // A big "+" in a dashed box
        QPixmap pixmap(ICON_SIZE, ICON_SIZE);
        pixmap.fill(Qt::transparent);
        QPainter painter(&pixmap);
        painter.setRenderHint(QPainter::Antialiasing);
        QPen pen(this->palette().color(QPalette::Disabled, QPalette::Text), 2, Qt::DashLine);
        painter.setPen(pen);
        painter.setBrush(Qt::NoBrush);
        painter.drawRoundedRect(QRectF(4, 8, ICON_SIZE - 8, ICON_SIZE - 12), 10, 10);
        pen.setStyle(Qt::SolidLine);
        pen.setWidth(4);
        painter.setPen(pen);
        int cx = ICON_SIZE / 2;
        int cy = ICON_SIZE / 2 + 2;
        painter.drawLine(cx - 16, cy, cx + 16, cy);
        painter.drawLine(cx, cy - 16, cx, cy + 16);
        painter.end();
        this->add_tile->setIcon(QIcon(pixmap));
    }
    connect(this->add_tile, SIGNAL(clicked()), this, SLOT(do_add_rom()));

    this->relayout_tiles();
}

int LandingWidget::columns() const {
    // Derived from our own width rather than the scroll viewport's: this runs from resizeEvent(),
    // before the layout has handed the scroll area its new size. Always leave room for the
    // vertical scroll bar so the column count doesn't flap as it appears and disappears.
    auto margins = this->layout()->contentsMargins();
    int available = this->width() - margins.left() - margins.right() - this->style()->pixelMetric(QStyle::PM_ScrollBarExtent);
    int count = (available + TILE_SPACING) / (TILE_WIDTH + TILE_SPACING);
    return count < 1 ? 1 : count;
}

void LandingWidget::relayout_tiles() {
    int cols = this->columns();

    // Pull everything out of the grid, then re-add it in reading order with the new column count.
    while(this->tile_grid->count() > 0) {
        delete this->tile_grid->takeAt(0);
    }

    int index = 0;
    for(auto *tile : this->tiles) {
        this->tile_grid->addWidget(tile, index / cols, index % cols);
        index++;
    }
    this->tile_grid->addWidget(this->add_tile, index / cols, index % cols);
}

void LandingWidget::resizeEvent(QResizeEvent *event) {
    QWidget::resizeEvent(event);
    this->relayout_tiles();
}

void LandingWidget::add_rom(const QString &path) {
    QFileInfo info(path);
    if(!info.isFile()) {
        this->main_window->show_error("Can't add ROM", "\"%s\" is not a file.", QDir::toNativeSeparators(path).toUtf8().constData());
        return;
    }

    // Adding a game that's already here just moves it to the front rather than duplicating it.
    QString absolute = info.absoluteFilePath();
    for(std::size_t i = 0; i < this->favorites.size(); i++) {
        if(QFileInfo(this->favorites[i].path).absoluteFilePath() == absolute) {
            auto existing = this->favorites[i];
            this->favorites.erase(this->favorites.begin() + i);
            this->favorites.insert(this->favorites.begin(), existing);
            this->save_favorites();
            this->rebuild_tiles();
            return;
        }
    }

    Favorite favorite;
    favorite.path = absolute;
    favorite.name = info.completeBaseName();
    this->favorites.emplace_back(std::move(favorite));
    this->save_favorites();
    this->rebuild_tiles();
}

void LandingWidget::do_add_rom() {
    QFileDialog rom_opener(this->main_window);
    rom_opener.setFileMode(QFileDialog::FileMode::ExistingFiles);
    rom_opener.setNameFilters(QStringList({
        "All compatible ROM files (*.gb *.gbc *.gba *.nds)",
        "GB/GBC ROM dumps (*.gb *.gbc)",
        "GBA ROM dumps (*.gba)",
        "NDS ROM files (*.nds)",
        "Any files (*)"
    }));
    rom_opener.setWindowTitle("Select ROMs to add to the start screen");

    // exec() runs a nested event loop; keep the 1 ms ticker from re-entering tick() underneath it.
    this->main_window->stop_timer();
    rom_opener.exec();
    this->main_window->start_timer();

    for(const auto &file : rom_opener.selectedFiles()) {
        this->add_rom(file);
    }
}

void LandingWidget::open_favorite(std::size_t index) {
    if(index >= this->favorites.size()) {
        return;
    }
    // Copy the path first: loading a ROM swaps this widget out and may rebuild the tiles.
    QString path = this->favorites[index].path;
    this->main_window->load_rom(std::filesystem::path(path.toStdU16String()));
}

void LandingWidget::set_image(std::size_t index, const QString &source) {
    if(index >= this->favorites.size()) {
        return;
    }

    QImage image(source);
    if(image.isNull()) {
        this->main_window->show_error("Can't use picture", "\"%s\" could not be read as an image.", QDir::toNativeSeparators(source).toUtf8().constData());
        return;
    }

    // Keep our own scaled-down copy so the original can move or be deleted without breaking the
    // icon, and so huge photos don't have to be decoded every time the screen is shown.
    if(image.width() > STORED_ICON_SIZE || image.height() > STORED_ICON_SIZE) {
        image = image.scaled(STORED_ICON_SIZE, STORED_ICON_SIZE, Qt::KeepAspectRatio, Qt::SmoothTransformation);
    }

    QDir dir(this->icons_dir());
    if(!dir.exists() && !QDir().mkpath(dir.absolutePath())) {
        this->main_window->show_error("Can't use picture", "Failed to create \"%s\".", QDir::toNativeSeparators(dir.absolutePath()).toUtf8().constData());
        return;
    }

    QString file_name = QString("%1-%2.png").arg(QDateTime::currentMSecsSinceEpoch()).arg(index);
    if(!image.save(dir.filePath(file_name), "PNG")) {
        this->main_window->show_error("Can't use picture", "Failed to write \"%s\".", QDir::toNativeSeparators(dir.filePath(file_name)).toUtf8().constData());
        return;
    }

    auto &favorite = this->favorites[index];
    if(!favorite.image.isEmpty()) {
        QFile::remove(dir.filePath(favorite.image));
    }
    favorite.image = file_name;

    this->save_favorites();
    this->rebuild_tiles();
}

void LandingWidget::clear_image(std::size_t index) {
    if(index >= this->favorites.size()) {
        return;
    }
    auto &favorite = this->favorites[index];
    if(favorite.image.isEmpty()) {
        return;
    }
    QFile::remove(QDir(this->icons_dir()).filePath(favorite.image));
    favorite.image.clear();
    this->save_favorites();
    this->rebuild_tiles();
}

void LandingWidget::remove_favorite(std::size_t index) {
    if(index >= this->favorites.size()) {
        return;
    }
    const auto &favorite = this->favorites[index];
    if(!favorite.image.isEmpty()) {
        QFile::remove(QDir(this->icons_dir()).filePath(favorite.image));
    }
    this->favorites.erase(this->favorites.begin() + index);
    this->save_favorites();
    this->rebuild_tiles();
}

void LandingWidget::rename_favorite(std::size_t index) {
    if(index >= this->favorites.size()) {
        return;
    }
    auto name = AskForTextDialog::ask(this->main_window, "Rename game", "Enter a new name for this game.", "", this->favorites[index].name);
    if(!name.has_value() || index >= this->favorites.size()) {
        return;
    }
    QString new_name = QString::fromStdString(*name).trimmed();
    if(new_name.isEmpty()) {
        return;
    }
    this->favorites[index].name = new_name;
    this->save_favorites();
    this->rebuild_tiles();
}

void LandingWidget::move_favorite(std::size_t index, int delta) {
    if(index >= this->favorites.size()) {
        return;
    }
    long long target = static_cast<long long>(index) + delta;
    if(target < 0 || target >= static_cast<long long>(this->favorites.size())) {
        return;
    }
    std::swap(this->favorites[index], this->favorites[static_cast<std::size_t>(target)]);
    this->save_favorites();
    this->rebuild_tiles();
}

void LandingWidget::show_tile_menu(std::size_t index, const QPoint &global_pos) {
    if(index >= this->favorites.size()) {
        return;
    }

    QMenu menu(this);

    auto *play = menu.addAction("Play");
    menu.setDefaultAction(play);
    menu.addSeparator();
    auto *set_picture = menu.addAction("Set picture…");
    auto *clear_picture = menu.addAction("Use default picture");
    clear_picture->setEnabled(!this->favorites[index].image.isEmpty());
    auto *rename = menu.addAction("Rename…");
    menu.addSeparator();
    auto *move_up = menu.addAction("Move earlier");
    move_up->setEnabled(index > 0);
    auto *move_down = menu.addAction("Move later");
    move_down->setEnabled(index + 1 < this->favorites.size());
    menu.addSeparator();
    auto *remove = menu.addAction("Remove from this screen");

    auto *chosen = menu.exec(global_pos);
    if(chosen == nullptr) {
        return;
    }

    if(chosen == play) {
        this->open_favorite(index);
    }
    else if(chosen == set_picture) {
        QStringList patterns;
        for(const auto &format : QImageReader::supportedImageFormats()) {
            patterns.append("*." + QString::fromLatin1(format));
        }
        QFileDialog picker(this->main_window);
        picker.setFileMode(QFileDialog::FileMode::ExistingFile);
        picker.setNameFilters(QStringList({
            "Images (" + patterns.join(' ') + ")",
            "Any files (*)"
        }));
        picker.setWindowTitle("Select a picture for this game");

        this->main_window->stop_timer();
        picker.exec();
        this->main_window->start_timer();

        auto files = picker.selectedFiles();
        if(files.size() == 1) {
            this->set_image(index, files[0]);
        }
    }
    else if(chosen == clear_picture) {
        this->clear_image(index);
    }
    else if(chosen == rename) {
        this->rename_favorite(index);
    }
    else if(chosen == move_up) {
        this->move_favorite(index, -1);
    }
    else if(chosen == move_down) {
        this->move_favorite(index, 1);
    }
    else if(chosen == remove) {
        this->remove_favorite(index);
    }
}

int LandingWidget::tile_index_at(const QPoint &pos) const {
    for(std::size_t i = 0; i < this->tiles.size(); i++) {
        auto *tile = this->tiles[i];
        if(!tile->isVisible()) {
            continue;
        }
        QRect rect(tile->mapTo(this, QPoint(0, 0)), tile->size());
        if(rect.contains(pos)) {
            return static_cast<int>(i);
        }
    }
    return -1;
}

int LandingWidget::drop_slot_at(const QPoint &pos) const {
    // The tile under the cursor, else the first tile that starts after the cursor in reading
    // order, else the end of the list.
    int direct = this->tile_index_at(pos);
    if(direct >= 0) {
        return direct;
    }
    for(std::size_t i = 0; i < this->tiles.size(); i++) {
        QRect rect(this->tiles[i]->mapTo(this, QPoint(0, 0)), this->tiles[i]->size());
        if(pos.y() < rect.top() || (pos.y() <= rect.bottom() && pos.x() < rect.left())) {
            return static_cast<int>(i);
        }
    }
    return static_cast<int>(this->tiles.size()) - 1;
}

bool LandingWidget::eventFilter(QObject *watched, QEvent *event) {
    auto *tile = qobject_cast<QToolButton *>(watched);
    if(tile != nullptr && tile != this->add_tile) {
        if(event->type() == QEvent::MouseButtonPress) {
            auto *mouse = static_cast<QMouseEvent *>(event);
            if(mouse->button() == Qt::LeftButton) {
                this->drag_press_pos = mouse->position().toPoint();
                this->drag_press_active = true;
            }
        }
        else if(event->type() == QEvent::MouseButtonRelease) {
            this->drag_press_active = false;
        }
        else if(event->type() == QEvent::MouseMove && this->drag_press_active) {
            auto *mouse = static_cast<QMouseEvent *>(event);
            if((mouse->buttons() & Qt::LeftButton) &&
                (mouse->position().toPoint() - this->drag_press_pos).manhattanLength() >= QApplication::startDragDistance()) {
                this->drag_press_active = false;
                bool ok = false;
                uint index = tile->property("favorite_index").toUInt(&ok);
                if(ok) {
                    this->start_tile_drag(index);
                }
                return true;
            }
        }
    }
    return QWidget::eventFilter(watched, event);
}

void LandingWidget::start_tile_drag(std::size_t index) {
    if(index >= this->tiles.size()) {
        return;
    }
    auto *tile = this->tiles[index];

    auto *mime = new QMimeData();
    mime->setData(TILE_MIME, QByteArray::number(static_cast<qulonglong>(index)));

    // Owned by the tile, freed by exec(); the tile outlives it because rebuild_tiles() only ever
    // deleteLater()s, which can't run until this nested loop has ended.
    auto *drag = new QDrag(tile);
    drag->setMimeData(mime);
    drag->setPixmap(tile->grab());
    drag->setHotSpot(this->drag_press_pos);
    drag->exec(Qt::MoveAction);

    // The button never sees the release that ended the drag; don't leave it drawn pressed.
    tile->setDown(false);
}

template<typename T> static std::optional<QString> single_dropped_file(T *event) {
    auto *data = event->mimeData();
    if(data->hasUrls()) {
        auto urls = data->urls();
        if(urls.length() == 1 && urls[0].isLocalFile()) {
            return urls[0].toLocalFile();
        }
    }
    return std::nullopt;
}

void LandingWidget::dragEnterEvent(QDragEnterEvent *event) {
    if(event->mimeData()->hasFormat(TILE_MIME) || single_dropped_file(event)) {
        event->acceptProposedAction();
    }
}

void LandingWidget::dragMoveEvent(QDragMoveEvent *event) {
    if(event->mimeData()->hasFormat(TILE_MIME) || single_dropped_file(event)) {
        event->acceptProposedAction();
    }
}

void LandingWidget::dropEvent(QDropEvent *event) {
    if(event->mimeData()->hasFormat(TILE_MIME)) {
        event->acceptProposedAction();
        bool ok = false;
        qulonglong from = event->mimeData()->data(TILE_MIME).toULongLong(&ok);
        int to = this->drop_slot_at(event->position().toPoint());
        if(!ok || from >= this->favorites.size() || to < 0 || static_cast<std::size_t>(to) == from) {
            return;
        }
        auto moved = this->favorites[static_cast<std::size_t>(from)];
        this->favorites.erase(this->favorites.begin() + static_cast<std::ptrdiff_t>(from));
        this->favorites.insert(this->favorites.begin() + to, moved);
        this->save_favorites();
        this->rebuild_tiles();
        return;
    }

    auto file = single_dropped_file(event);
    if(!file) {
        return;
    }
    event->acceptProposedAction();

    // A picture dropped on a game becomes its icon; anything else is a ROM to open, matching what
    // dropping on the game view does.
    if(looks_like_image(*file)) {
        int index = this->tile_index_at(event->position().toPoint());
        if(index >= 0) {
            this->set_image(static_cast<std::size_t>(index), *file);
        }
        return;
    }

    this->main_window->load_rom(std::filesystem::path(file->toStdU16String()));
}
