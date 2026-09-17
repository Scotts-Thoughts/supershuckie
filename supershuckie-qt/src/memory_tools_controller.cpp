#include <QDesktopServices>
#include <algorithm>
#include <QDir>
#include <QUrl>

#include "memory_tools_controller.hpp"
#include "hex_viewer_window.hpp"
#include "ram_search_window.hpp"
#include "ram_watch_window.hpp"
#include "watch_edit_dialog.hpp"
#include <QCheckBox>
#include <QInputDialog>
#include <QJsonArray>
#include <QJsonDocument>
#include <QMenu>
#include <QMessageBox>
#include "main_window.hpp"

using namespace SuperShuckie64;

static const char *RAM_VIEWERS_OPEN = "qt__ram_viewers_open";
static const char *RAM_VIEWER_STATE_PREFIX = "qt__ram_viewer_";
static const char *RAM_REFRESH_RATE = "qt__ram_refresh_hz";
static const char *RAM_SEARCH_STATE = "qt__ram_search_window";
static const char *RAM_WATCH_STATE = "qt__ram_watch_window";

MemoryToolsController::MemoryToolsController(MainWindow *main_window): QObject(main_window), main(main_window) {
    connect(&this->timer, SIGNAL(timeout()), this, SLOT(on_timer()));
    // While every tool window is hidden, traced watches still pause emulation; this notices and
    // shows the watch window.
    connect(&this->idle_timer, SIGNAL(timeout()), this, SLOT(on_idle_timer()));
    this->idle_timer.setInterval(250);
    this->idle_timer.start();

    const char *rate = supershuckie_frontend_get_custom_setting(this->frontend(), RAM_REFRESH_RATE);
    if(rate != nullptr) {
        int hz = QString(rate).toInt();
        if(hz > 0) {
            supershuckie_frontend_memory_set_refresh_rate(this->frontend(), static_cast<std::uint8_t>(std::clamp(hz, 1, 120)));
        }
    }
    this->timer.setInterval(1000 / std::max(1, this->refresh_rate()));
    this->update_regions();
}

SuperShuckieFrontendRaw *MemoryToolsController::frontend() const noexcept {
    return this->main->frontend;
}

bool MemoryToolsController::any_window_visible() const {
    for(auto *viewer : this->viewers) {
        if(viewer != nullptr && viewer->isVisible()) {
            return true;
        }
    }
    return (this->search != nullptr && this->search->isVisible()) || (this->watch != nullptr && this->watch->isVisible());
}

void MemoryToolsController::on_idle_timer() {
    if(this->timer.isActive()) {
        return;
    }
    this->update_regions();
    if(this->watch != nullptr || supershuckie_frontend_memory_has_traces(this->frontend())) {
        if(this->watch == nullptr) {
            this->watch = new RamWatchWindow(this);
        }
        emit this->refresh();
    }
}

RamWatchWindow *MemoryToolsController::open_watch() {
    if(this->watch == nullptr) {
        this->watch = new RamWatchWindow(this);
    }
    this->watch->show();
    this->watch->raise();
    this->watch->activateWindow();
    this->visibility_changed();
    return this->watch;
}

std::uint32_t MemoryToolsController::upsert_watch(const QByteArray &json, char *error, std::size_t error_len) {
    return supershuckie_frontend_watch_upsert_json(this->frontend(), json.constData(), error, error_len);
}

void MemoryToolsController::add_watch(QWidget *parent, std::uint32_t address, std::uint32_t value_type, std::uint8_t size, bool big_endian, const QString &label) {
    auto watch = WatchEditDialog::new_watch(address, value_type, size, big_endian, label);
    auto id = WatchEditDialog::edit(this, parent, watch);
    if(id) {
        this->open_watch()->select_watch(*id);
    }
}

void MemoryToolsController::add_watches(const std::vector<std::uint32_t> &addresses, std::uint32_t value_type, std::uint8_t size, bool big_endian) {
    std::uint32_t last = 0;
    char error[512] = {};
    for(auto address : addresses) {
        auto watch = WatchEditDialog::new_watch(address, value_type, size, big_endian, this->format_address(address, true));
        auto id = this->upsert_watch(QJsonDocument(watch).toJson(QJsonDocument::Compact), error, sizeof(error));
        if(id == 0) {
            this->main_window()->show_error("Can't add watch", "%s", error);
            break;
        }
        last = id;
    }
    if(last != 0) {
        this->open_watch()->select_watch(last);
    }
}

namespace {
    const char *WATCH_TYPE_NAMES[] = { "u8", "i8", "u16", "i16", "u32", "i32", "f32", "bcd", "bytes", "text" };

    std::uint32_t watch_type_index(const QString &name) {
        for(std::uint32_t i = 0; i < 10; i++) {
            if(name == WATCH_TYPE_NAMES[i]) {
                return i;
            }
        }
        return 0;
    }
}

bool MemoryToolsController::edit_watch_value_inline(QWidget *parent, std::uint32_t id, bool value_column) {
    if(!value_column) {
        return false;
    }
    char *list = supershuckie_frontend_watch_list_json(this->frontend());
    auto array = QJsonDocument::fromJson(QByteArray(list)).array();
    supershuckie_string_free(list);
    QJsonObject watch;
    for(auto value : array) {
        if(static_cast<std::uint32_t>(value.toObject()["id"].toInteger()) == id) {
            watch = value.toObject();
        }
    }
    if(watch.isEmpty()) {
        return false;
    }

    auto format = watch["format"].toObject();
    std::uint32_t type = watch_type_index(format["type"].toString());
    auto size = static_cast<std::uint8_t>(format["size"].toInt());
    bool big_endian = format["big_endian"].toBool();
    std::size_t table = std::max<qsizetype>(0, this->table_names().indexOf(watch["table"].toString()));

    // Start from the value shown.
    SuperShuckieWatchValue values[512];
    std::size_t count = supershuckie_frontend_watch_read_values(this->frontend(), values, 512);
    QString current;
    for(std::size_t i = 0; i < count; i++) {
        if(values[i].id == id && values[i].ok) {
            current = this->format_value(table, type, size, big_endian, type >= 7 ? 0 : SuperShuckieMemoryDisplay__Decimal, values[i].value, values[i].length);
        }
    }

    bool frozen = watch["freeze"].toObject()["active"].toBool();
    bool ok = false;
    this->main->stop_timer();
    QString text = QInputDialog::getText(parent, frozen ? "Change frozen value" : "Set value", QString("%1 (%2):").arg(watch["label"].toString(), format["type"].toString()), QLineEdit::Normal, current, &ok);
    this->main->start_timer();
    if(!ok) {
        return true;
    }
    auto bytes = this->parse_value(parent, table, type, size, big_endian, text);
    if(!bytes) {
        return true;
    }
    if(bytes->size() < size) {
        bytes->append(QByteArray(size - bytes->size(), '\0'));
    }

    auto address = watch["address"].toObject();
    if(frozen) {
        this->set_frozen(parent, id, true, *bytes);
    }
    else if(!this->confirm_write(parent)) {
        return true;
    }
    else {
        std::vector<std::int32_t> offsets;
        for(auto offset : address["offsets"].toArray()) {
            offsets.push_back(offset.toInt());
        }
        bool parsed = false;
        std::uint32_t base = address["base"].toString().mid(2).toUInt(&parsed, 16);
        char error[512] = {};
        if(!supershuckie_frontend_memory_write(this->frontend(), base, offsets.data(), offsets.size(), reinterpret_cast<const std::uint8_t *>(bytes->constData()), static_cast<std::size_t>(bytes->size()), error, sizeof(error))) {
            emit this->message(QString::fromUtf8(error));
        }
    }
    return true;
}

void MemoryToolsController::add_watch_actions(QMenu *menu, QWidget *parent, const std::vector<std::uint32_t> &ids) {
    if(ids.empty()) {
        return;
    }
    menu->addSeparator();
    auto *freeze = menu->addAction(ids.size() == 1 ? "Freeze at current value" : QString("Freeze %1 watches at their current values").arg(ids.size()));
    connect(freeze, &QAction::triggered, this, [this, parent, ids]() {
        SuperShuckieWatchValue values[512];
        std::size_t count = supershuckie_frontend_watch_read_values(this->frontend(), values, 512);
        for(auto id : ids) {
            for(std::size_t i = 0; i < count; i++) {
                if(values[i].id == id && values[i].ok) {
                    if(!this->set_frozen(parent, id, true, QByteArray(reinterpret_cast<const char *>(values[i].value), values[i].length))) {
                        return;
                    }
                }
            }
        }
    });
    auto *unfreeze = menu->addAction(ids.size() == 1 ? "Unfreeze" : QString("Unfreeze %1 watches").arg(ids.size()));
    connect(unfreeze, &QAction::triggered, this, [this, parent, ids]() {
        for(auto id : ids) {
            this->set_frozen(parent, id, false);
        }
    });
    if(ids.size() == 1) {
        auto *set_value = menu->addAction("Set value…");
        auto id = ids[0];
        connect(set_value, &QAction::triggered, this, [this, parent, id]() {
            this->edit_watch_value_inline(parent, id, true);
        });
    }
}

RamSearchWindow *MemoryToolsController::open_search() {
    if(this->search == nullptr) {
        this->search = new RamSearchWindow(this);
    }
    this->search->show();
    this->search->raise();
    this->search->activateWindow();
    this->visibility_changed();
    return this->search;
}

void MemoryToolsController::search_for_value(std::uint32_t value_type, std::uint8_t size, bool big_endian, const QString &value) {
    this->open_search()->prefill(value_type, size, big_endian, value);
}

void MemoryToolsController::visibility_changed() {
    if(this->any_window_visible()) {
        if(!this->timer.isActive()) {
            this->timer.start();
        }
    }
    else {
        this->timer.stop();
    }
}

void MemoryToolsController::on_timer() {
    this->update_regions();
    this->update_frozen();
    char text[512];
    if(supershuckie_frontend_memory_edit_message(this->frontend(), text, sizeof(text))) {
        emit this->message(QString::fromUtf8(text));
    }
    emit this->refresh();
}

void MemoryToolsController::update_frozen() {
    std::size_t count = supershuckie_frontend_memory_frozen_ranges(this->frontend(), nullptr, nullptr, 0);
    std::vector<std::uint32_t> starts(count), lengths(count);
    count = supershuckie_frontend_memory_frozen_ranges(this->frontend(), starts.data(), lengths.data(), count);
    this->frozen.clear();
    for(std::size_t i = 0; i < count; i++) {
        this->frozen.emplace_back(starts[i], lengths[i]);
    }
}

bool MemoryToolsController::confirm_write(QWidget *parent) {
    char reason[512] = {};
    if(!supershuckie_frontend_memory_can_write(this->frontend(), reason, sizeof(reason))) {
        emit this->message(QString::fromUtf8(reason));
        return false;
    }
    if(!supershuckie_frontend_memory_needs_record_confirmation(this->frontend())) {
        return true;
    }
    QMessageBox box(parent);
    box.setWindowTitle("Edit memory while recording?");
    box.setIcon(QMessageBox::Question);
    box.setText("A replay is being recorded. Memory edits and freezes are written into it, and anyone watching the replay will see them happen.");
    box.setInformativeText("Continue?");
    auto *dont_ask = new QCheckBox("Don't ask again", &box);
    box.setCheckBox(dont_ask);
    box.setStandardButtons(QMessageBox::Yes | QMessageBox::Cancel);
    box.setDefaultButton(QMessageBox::Cancel);
    this->main->stop_timer();
    int answer = box.exec();
    this->main->start_timer();
    if(answer != QMessageBox::Yes) {
        return false;
    }
    supershuckie_frontend_memory_confirm_record_writes(this->frontend(), dont_ask->isChecked());
    return true;
}

bool MemoryToolsController::write(QWidget *parent, std::uint32_t address, const QByteArray &bytes) {
    if(!this->confirm_write(parent)) {
        return false;
    }
    char error[512] = {};
    if(!supershuckie_frontend_memory_write(this->frontend(), address, nullptr, 0, reinterpret_cast<const std::uint8_t *>(bytes.constData()), static_cast<std::size_t>(bytes.size()), error, sizeof(error))) {
        emit this->message(QString::fromUtf8(error));
        return false;
    }
    return true;
}

std::uint32_t MemoryToolsController::freeze(QWidget *parent, std::uint32_t address, std::uint32_t value_type, std::uint8_t size, bool big_endian, const QByteArray &bytes, const char *group) {
    if(!this->confirm_write(parent)) {
        return 0;
    }
    char error[512] = {};
    auto id = supershuckie_frontend_memory_freeze(this->frontend(), address, nullptr, 0, value_type, size, big_endian, reinterpret_cast<const std::uint8_t *>(bytes.constData()), static_cast<std::size_t>(bytes.size()), group, error, sizeof(error));
    if(id == 0) {
        emit this->message(QString::fromUtf8(error));
    }
    this->update_frozen();
    return id;
}

bool MemoryToolsController::set_frozen(QWidget *parent, std::uint32_t id, bool frozen, const QByteArray &bytes) {
    if(frozen && !this->confirm_write(parent)) {
        return false;
    }
    char error[512] = {};
    bool ok = supershuckie_frontend_watch_set_frozen(this->frontend(), id, frozen, bytes.isEmpty() ? nullptr : reinterpret_cast<const std::uint8_t *>(bytes.constData()), static_cast<std::size_t>(bytes.size()), error, sizeof(error));
    if(!ok) {
        emit this->message(QString::fromUtf8(error));
    }
    this->update_frozen();
    return ok;
}

void MemoryToolsController::unfreeze_range(std::uint32_t address, std::uint32_t length) {
    char *list = supershuckie_frontend_watch_list_json(this->frontend());
    auto array = QJsonDocument::fromJson(QByteArray(list)).array();
    supershuckie_string_free(list);
    for(auto value : array) {
        auto watch = value.toObject();
        auto freeze = watch["freeze"].toObject();
        if(!freeze["active"].toBool()) {
            continue;
        }
        bool ok = false;
        std::uint64_t base = watch["address"].toObject()["base"].toString().mid(2).toUInt(&ok, 16);
        if(ok && base >= address && base < static_cast<std::uint64_t>(address) + length) {
            this->set_frozen(nullptr, static_cast<std::uint32_t>(watch["id"].toInteger()), false);
        }
    }
    this->update_frozen();
}

void MemoryToolsController::unfreeze_all() {
    supershuckie_frontend_memory_unfreeze_all(this->frontend());
    this->update_frozen();
}

void MemoryToolsController::undo(QWidget *) {
    char error[512] = {};
    if(!supershuckie_frontend_memory_undo(this->frontend(), error, sizeof(error))) {
        emit this->message(QString::fromUtf8(error));
    }
    this->update_frozen();
}

void MemoryToolsController::redo(QWidget *) {
    char error[512] = {};
    if(!supershuckie_frontend_memory_redo(this->frontend(), error, sizeof(error))) {
        emit this->message(QString::fromUtf8(error));
    }
    this->update_frozen();
}

std::optional<QByteArray> MemoryToolsController::parse_value(QWidget *, std::size_t table, std::uint32_t value_type, std::uint8_t size, bool big_endian, const QString &text) {
    std::uint8_t bytes[SUPERSHUCKIE_MEMORY_MAX_VALUE_SIZE];
    std::size_t length = 0;
    char error[512] = {};
    if(!supershuckie_frontend_memory_parse_value(this->frontend(), table, value_type, size, big_endian, text.toUtf8().constData(), bytes, sizeof(bytes), &length, error, sizeof(error))) {
        emit this->message(QString::fromUtf8(error));
        return std::nullopt;
    }
    return QByteArray(reinterpret_cast<const char *>(bytes), static_cast<qsizetype>(length));
}

void MemoryToolsController::update_regions() {
    auto generation = supershuckie_frontend_memory_regions_generation(this->frontend());
    if(generation == this->cached_regions_generation) {
        return;
    }
    this->cached_regions_generation = generation;

    std::size_t count = supershuckie_frontend_memory_get_regions(this->frontend(), nullptr, 0);
    std::vector<SuperShuckieMemoryRegion> raw(count);
    supershuckie_frontend_memory_get_regions(this->frontend(), raw.data(), raw.size());

    this->region_cache.clear();
    std::uint64_t max_end = 0;
    for(auto &r : raw) {
        this->region_cache.push_back(MemoryRegionView {
            QString::fromUtf8(r.name),
            QString::fromUtf8(r.short_name),
            r.base_address,
            r.length,
            r.default_big_endian,
            r.writable
        });
        max_end = std::max<std::uint64_t>(max_end, static_cast<std::uint64_t>(r.base_address) + r.length - 1);
    }

    // Same rule as the frontend's address formatting: 4-6 digits, or all 8.
    int needed = 4;
    while(needed < 8 && (max_end >> (needed * 4)) != 0) {
        needed++;
    }
    this->digits = needed > 6 ? 8 : needed;

    emit this->regions_changed();
}

int MemoryToolsController::region_index_of(std::uint32_t address) const noexcept {
    for(std::size_t i = 0; i < this->region_cache.size(); i++) {
        if(this->region_cache[i].contains(address)) {
            return static_cast<int>(i);
        }
    }
    return -1;
}

QString MemoryToolsController::format_address(std::uint32_t address, bool region_relative) const {
    char buffer[64];
    supershuckie_frontend_memory_format_address(this->frontend(), address, region_relative, buffer, sizeof(buffer));
    return QString::fromUtf8(buffer);
}

std::optional<std::uint32_t> MemoryToolsController::parse_address(const QString &text, QString *error) const {
    std::uint32_t address = 0;
    char error_buffer[256] = {};
    if(supershuckie_frontend_memory_parse_address(this->frontend(), text.toUtf8().constData(), &address, error_buffer, sizeof(error_buffer))) {
        return address;
    }
    if(error != nullptr) {
        *error = QString::fromUtf8(error_buffer);
    }
    return std::nullopt;
}

QString MemoryToolsController::format_value(std::size_t table, std::uint32_t value_type, std::uint8_t size, bool big_endian, std::uint32_t display, const std::uint8_t *bytes, std::size_t length) const {
    char buffer[512];
    supershuckie_frontend_memory_format_value(this->frontend(), table, value_type, size, big_endian, display, bytes, length, buffer, sizeof(buffer));
    return QString::fromUtf8(buffer);
}

QStringList MemoryToolsController::table_names() const {
    QStringList names;
    std::size_t count = supershuckie_frontend_memory_table_count(this->frontend());
    for(std::size_t i = 0; i < count; i++) {
        names << QString::fromUtf8(supershuckie_frontend_memory_table_name(this->frontend(), i));
    }
    return names;
}

const std::array<QString, 256> &MemoryToolsController::glyphs(std::size_t table) {
    std::size_t count = supershuckie_frontend_memory_table_count(this->frontend());
    if(table >= count) {
        table = 0;
    }
    while(this->glyph_cache.size() <= table) {
        std::size_t index = this->glyph_cache.size();
        std::array<QString, 256> glyphs;
        char buffer[64];
        for(int b = 0; b < 256; b++) {
            if(supershuckie_frontend_memory_table_glyph(this->frontend(), index, static_cast<std::uint8_t>(b), buffer, sizeof(buffer))) {
                glyphs[b] = QString::fromUtf8(buffer);
            }
        }
        this->glyph_cache.push_back(std::move(glyphs));
    }
    return this->glyph_cache[table];
}

void MemoryToolsController::reload_tables() {
    char error[2048] = {};
    bool ok = supershuckie_frontend_memory_reload_tables(this->frontend(), error, sizeof(error));
    this->glyph_cache.clear();
    emit this->tables_changed();
    if(!ok) {
        this->main_window()->show_error("Some character tables could not be loaded", "%s", error);
    }
}

void MemoryToolsController::open_tables_folder() {
    char buffer[4096];
    supershuckie_frontend_memory_tables_directory(this->frontend(), buffer, sizeof(buffer));
    QDir().mkpath(QString::fromUtf8(buffer));
    QDesktopServices::openUrl(QUrl::fromLocalFile(QString::fromUtf8(buffer)));
}

int MemoryToolsController::refresh_rate() const {
    return supershuckie_frontend_memory_get_refresh_rate(this->frontend());
}

void MemoryToolsController::set_refresh_rate(int hz) {
    hz = std::clamp(hz, 1, 120);
    supershuckie_frontend_memory_set_refresh_rate(this->frontend(), static_cast<std::uint8_t>(hz));
    supershuckie_frontend_set_custom_setting(this->frontend(), RAM_REFRESH_RATE, QString::number(hz).toUtf8().constData());
    this->timer.setInterval(1000 / hz);
}

void MemoryToolsController::viewer_activated(HexViewerWindow *viewer) {
    this->last_viewer = viewer;
}

HexViewerWindow *MemoryToolsController::new_viewer() {
    for(std::size_t slot = 0; slot < this->viewers.size(); slot++) {
        if(this->viewers[slot] == nullptr) {
            this->viewers[slot] = new HexViewerWindow(this, static_cast<std::uint8_t>(slot));
        }
        auto *viewer = this->viewers[slot];
        if(!viewer->isVisible()) {
            viewer->show();
            viewer->raise();
            viewer->activateWindow();
            this->last_viewer = viewer;
            this->visibility_changed();
            return viewer;
        }
    }
    return nullptr;
}

void MemoryToolsController::open_viewer() {
    if(this->last_viewer != nullptr && this->last_viewer->isVisible()) {
        this->last_viewer->raise();
        this->last_viewer->activateWindow();
        return;
    }
    for(auto *viewer : this->viewers) {
        if(viewer != nullptr && viewer->isVisible()) {
            viewer->raise();
            viewer->activateWindow();
            this->last_viewer = viewer;
            return;
        }
    }
    this->new_viewer();
}

void MemoryToolsController::show_in_viewer(std::uint32_t address, std::uint32_t length) {
    this->update_regions();
    HexViewerWindow *viewer = (this->last_viewer != nullptr && this->last_viewer->isVisible()) ? this->last_viewer : nullptr;
    if(viewer == nullptr) {
        for(auto *v : this->viewers) {
            if(v != nullptr && v->isVisible()) {
                viewer = v;
                break;
            }
        }
    }
    if(viewer == nullptr) {
        viewer = this->new_viewer();
    }
    if(viewer == nullptr) {
        return;
    }
    viewer->go_to(address, length);
    viewer->raise();
    viewer->activateWindow();
}

void MemoryToolsController::save_windows() {
    QStringList open;
    for(std::size_t slot = 0; slot < this->viewers.size(); slot++) {
        auto *viewer = this->viewers[slot];
        if(viewer == nullptr) {
            continue;
        }
        QString key = QString(RAM_VIEWER_STATE_PREFIX) + QString::number(slot);
        supershuckie_frontend_set_custom_setting(this->frontend(), key.toUtf8().constData(), viewer->save_state().toUtf8().constData());
        if(viewer->isVisible()) {
            open << QString::number(slot);
        }
    }
    supershuckie_frontend_set_custom_setting(this->frontend(), RAM_VIEWERS_OPEN, open.join(',').toUtf8().constData());

    if(this->search != nullptr) {
        QString state = QString("%1|%2").arg(this->search->isVisible() ? "1" : "0", this->search->save_state());
        supershuckie_frontend_set_custom_setting(this->frontend(), RAM_SEARCH_STATE, state.toUtf8().constData());
    }
    if(this->watch != nullptr) {
        QString state = QString("%1|%2").arg(this->watch->isVisible() ? "1" : "0", this->watch->save_state());
        supershuckie_frontend_set_custom_setting(this->frontend(), RAM_WATCH_STATE, state.toUtf8().constData());
    }
}

void MemoryToolsController::restore_windows() {
    for(std::size_t slot = 0; slot < this->viewers.size(); slot++) {
        QString key = QString(RAM_VIEWER_STATE_PREFIX) + QString::number(slot);
        const char *state = supershuckie_frontend_get_custom_setting(this->frontend(), key.toUtf8().constData());
        if(state == nullptr) {
            continue;
        }
        // Copy out of the FFI buffer before constructing the window: the pointer is only valid
        // until the next API call, and the window's constructor makes plenty of those.
        QString state_copy = QString::fromUtf8(state);
        this->viewers[slot] = new HexViewerWindow(this, static_cast<std::uint8_t>(slot));
        this->viewers[slot]->restore_state(state_copy);
    }

    const char *open = supershuckie_frontend_get_custom_setting(this->frontend(), RAM_VIEWERS_OPEN);
    if(open != nullptr) {
        for(auto &slot_text : QString::fromUtf8(open).split(',', Qt::SkipEmptyParts)) {
            bool ok = false;
            int slot = slot_text.toInt(&ok);
            if(!ok || slot < 0 || slot >= static_cast<int>(this->viewers.size())) {
                continue;
            }
            if(this->viewers[slot] == nullptr) {
                this->viewers[slot] = new HexViewerWindow(this, static_cast<std::uint8_t>(slot));
            }
            this->viewers[slot]->show();
        }
    }

    const char *search_state = supershuckie_frontend_get_custom_setting(this->frontend(), RAM_SEARCH_STATE);
    if(search_state != nullptr) {
        QString state = QString::fromUtf8(search_state);
        int split = state.indexOf('|');
        if(split > 0) {
            this->search = new RamSearchWindow(this);
            this->search->restore_state(state.mid(split + 1));
            if(state.left(split) == "1") {
                this->search->show();
            }
        }
    }

    const char *watch_state = supershuckie_frontend_get_custom_setting(this->frontend(), RAM_WATCH_STATE);
    if(watch_state != nullptr) {
        QString state = QString::fromUtf8(watch_state);
        int split = state.indexOf('|');
        if(split > 0) {
            this->watch = new RamWatchWindow(this);
            this->watch->restore_state(state.mid(split + 1));
            if(state.left(split) == "1") {
                this->watch->show();
            }
        }
    }
    this->visibility_changed();
}
