#ifndef __SUPERSHUCKIE_MEMORY_TOOLS_CONTROLLER_HPP__
#define __SUPERSHUCKIE_MEMORY_TOOLS_CONTROLLER_HPP__

#include <QObject>
#include <QString>
#include <QTimer>
#include <array>
#include <cstdint>
#include <optional>
#include <vector>

#include <supershuckie/supershuckie.h>

class QMenu;

namespace SuperShuckie64 {

class MainWindow;
class HexViewerWindow;
class RamSearchWindow;
class RamWatchWindow;

/** A memory region of the running game, as the tool windows see it. */
struct MemoryRegionView {
    QString name;
    QString short_name;
    std::uint32_t base;
    std::uint32_t length;
    bool big_endian;
    bool writable;

    bool contains(std::uint32_t address) const noexcept {
        return address >= this->base && static_cast<std::uint64_t>(address) < static_cast<std::uint64_t>(this->base) + this->length;
    }
};

/**
 * Owns the RAM tool windows (separate top-level windows), refreshes them from one timer that only
 * runs while one of them is visible, and remembers which were open where.
 */
class MemoryToolsController: public QObject {
    Q_OBJECT
public:
    MemoryToolsController(MainWindow *main_window);

    SuperShuckieFrontendRaw *frontend() const noexcept;
    MainWindow *main_window() const noexcept { return this->main; }

    /** Raise the viewer used last, or open one. */
    void open_viewer();

    /** Open another viewer window if fewer than SUPERSHUCKIE_MEMORY_MAX_VIEWERS are open. */
    HexViewerWindow *new_viewer();

    /** Show `address` in the viewer used last (opening one if none is open). */
    void show_in_viewer(std::uint32_t address, std::uint32_t length = 1);

    /** Open (or raise) the RAM search window. */
    RamSearchWindow *open_search();

    /** Open the search window ready to search for `value`. */
    void search_for_value(std::uint32_t value_type, std::uint8_t size, bool big_endian, const QString &value);

    /** Open (or raise) the RAM watch window. */
    RamWatchWindow *open_watch();

    /** Ask for a new watch at `address` (from the viewer or a search result). */
    void add_watch(QWidget *parent, std::uint32_t address, std::uint32_t value_type, std::uint8_t size, bool big_endian, const QString &label);

    /** Add watches for search results straight away. */
    void add_watches(const std::vector<std::uint32_t> &addresses, std::uint32_t value_type, std::uint8_t size, bool big_endian);

    /** Add or replace a watch (JSON); returns its id or 0 with a message in `error`. */
    std::uint32_t upsert_watch(const QByteArray &json, char *error, std::size_t error_len);

    /** Editing hooks for the watch window (value editing and freezing). */
    bool edit_watch_value_inline(QWidget *parent, std::uint32_t id, bool value_column);
    void add_watch_actions(QMenu *menu, QWidget *parent, const std::vector<std::uint32_t> &ids);

    /** Re-open the windows that were open when the app last closed. */
    void restore_windows();

    /** Remember which windows are open and where. */
    void save_windows();

    const std::vector<MemoryRegionView> &regions() const noexcept { return this->region_cache; }
    std::uint64_t regions_generation() const noexcept { return this->cached_regions_generation; }
    int address_digits() const noexcept { return this->digits; }
    int region_index_of(std::uint32_t address) const noexcept;

    /** "0x02024284", or "EWRAM:24284" when `region_relative`. */
    QString format_address(std::uint32_t address, bool region_relative) const;
    std::optional<std::uint32_t> parse_address(const QString &text, QString *error) const;

    /** Format `bytes` as a value (types as in memory.h). */
    QString format_value(std::size_t table, std::uint32_t value_type, std::uint8_t size, bool big_endian, std::uint32_t display, const std::uint8_t *bytes, std::size_t length) const;

    QStringList table_names() const;
    const std::array<QString, 256> &glyphs(std::size_t table);
    void reload_tables();
    void open_tables_folder();

    int refresh_rate() const;
    void set_refresh_rate(int hz);

    /** Tool windows report focus and visibility here. */
    void viewer_activated(HexViewerWindow *viewer);
    void visibility_changed();

signals:
    void regions_changed();
    void tables_changed();
    /** Emitted at the refresh rate while a tool window is visible. */
    void refresh();

private slots:
    void on_timer();
    void on_idle_timer();

private:
    MainWindow *main;
    QTimer timer;

    std::array<HexViewerWindow *, SUPERSHUCKIE_MEMORY_MAX_VIEWERS> viewers = {};
    HexViewerWindow *last_viewer = nullptr;
    RamSearchWindow *search = nullptr;
    RamWatchWindow *watch = nullptr;
    QTimer idle_timer;

    std::vector<MemoryRegionView> region_cache;
    std::uint64_t cached_regions_generation = 0;
    int digits = 8;

    std::vector<std::array<QString, 256>> glyph_cache;

    void update_regions();
    bool any_window_visible() const;
};

}

#endif
