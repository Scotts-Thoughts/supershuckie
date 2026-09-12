#ifndef __SUPERSHUCKIE_HEX_VIEWER_WINDOW_HPP__
#define __SUPERSHUCKIE_HEX_VIEWER_WINDOW_HPP__

#include <QWidget>
#include <cstdint>
#include <map>
#include <optional>
#include <vector>

class QTabBar;
class QLineEdit;
class QToolButton;
class QComboBox;
class QTableWidget;
class QLabel;
class QStackedWidget;

namespace SuperShuckie64 {

class MemoryToolsController;
class HexViewWidget;

/**
 * A RAM viewer window: a tab per memory region over a live hex view, with go-to, navigation
 * history and a data inspector. Up to SUPERSHUCKIE_MEMORY_MAX_VIEWERS can be open, each in its own
 * viewer slot.
 */
class HexViewerWindow: public QWidget {
    Q_OBJECT
public:
    HexViewerWindow(MemoryToolsController *controller, std::uint8_t slot);

    std::uint8_t slot() const noexcept { return this->viewer_slot; }

    /** Switch to the region containing `address` and put the cursor there. */
    bool go_to(std::uint32_t address, std::uint32_t length = 1, bool record_history = true);

    QString save_state() const;
    void restore_state(const QString &state);

protected:
    void showEvent(QShowEvent *event) override;
    void hideEvent(QHideEvent *event) override;
    void changeEvent(QEvent *event) override;

private slots:
    void on_regions_changed();
    void on_tables_changed();
    void on_refresh();
    void on_tab_changed(int index);
    void on_go_to();
    void on_back();
    void on_forward();
    void on_layout_changed();
    void on_table_changed(int index);
    void on_refresh_rate_changed(int index);
    void on_window_changed(std::uint32_t address, std::uint32_t length);
    void on_cursor_changed(std::uint32_t address);
    void on_context_menu(QPoint global_position);
    void on_inspector_activated(int row, int column);

private:
    MemoryToolsController *controller;
    std::uint8_t viewer_slot;

    QTabBar *tabs;
    QToolButton *back_button;
    QToolButton *forward_button;
    QLineEdit *go_to_edit;
    QComboBox *row_combo;
    QComboBox *group_combo;
    QComboBox *endian_combo;
    QComboBox *table_combo;
    QComboBox *rate_combo;
    QStackedWidget *stack;
    HexViewWidget *view;
    QLabel *no_game;
    QTableWidget *inspector;
    QLabel *status;

    /** Where the view was in each region, by short name. */
    struct Position {
        int top_row;
        std::uint32_t cursor;
    };
    std::map<QString, Position> positions;
    QString current_region;

    std::vector<std::uint32_t> history;
    std::size_t history_index = 0;

    std::uint64_t sample_generation = 0;
    std::uint64_t frame = 0;
    std::vector<std::uint8_t> buffer;

    /** Region and cursor to go to once regions are available (restored state). */
    std::optional<std::pair<QString, std::uint32_t>> pending_position;

    bool updating_tabs = false;

    void select_region(int index);
    void remember_position();
    void push_history(std::uint32_t address);
    void update_title();
    void update_status();
    void update_inspector();
    void update_window();
    void send_window(bool enabled);
    std::size_t table_index() const;
};

}

#endif
