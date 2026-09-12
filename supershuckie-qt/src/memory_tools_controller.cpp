#include <QDesktopServices>
#include <algorithm>
#include <QDir>
#include <QUrl>

#include "memory_tools_controller.hpp"
#include "hex_viewer_window.hpp"
#include "main_window.hpp"
#include "error.hpp"

using namespace SuperShuckie64;

static const char *RAM_VIEWERS_OPEN = "qt__ram_viewers_open";
static const char *RAM_VIEWER_STATE_PREFIX = "qt__ram_viewer_";
static const char *RAM_REFRESH_RATE = "qt__ram_refresh_hz";

MemoryToolsController::MemoryToolsController(MainWindow *main_window): QObject(main_window), main(main_window) {
    connect(&this->timer, SIGNAL(timeout()), this, SLOT(on_timer()));

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
    return false;
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
    emit this->refresh();
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
        DISPLAY_ERROR_DIALOG("Some character tables could not be loaded", "%s", error);
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
}

void MemoryToolsController::restore_windows() {
    for(std::size_t slot = 0; slot < this->viewers.size(); slot++) {
        QString key = QString(RAM_VIEWER_STATE_PREFIX) + QString::number(slot);
        const char *state = supershuckie_frontend_get_custom_setting(this->frontend(), key.toUtf8().constData());
        if(state == nullptr) {
            continue;
        }
        this->viewers[slot] = new HexViewerWindow(this, static_cast<std::uint8_t>(slot));
        this->viewers[slot]->restore_state(QString::fromUtf8(state));
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
    this->visibility_changed();
}
