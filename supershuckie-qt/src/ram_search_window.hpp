#ifndef __SUPERSHUCKIE_RAM_SEARCH_WINDOW_HPP__
#define __SUPERSHUCKIE_RAM_SEARCH_WINDOW_HPP__

#include <QAbstractTableModel>
#include <QWidget>
#include <cstdint>
#include <list>
#include <optional>
#include <unordered_map>
#include <vector>

#include <supershuckie/supershuckie.h>

class QCheckBox;
class QComboBox;
class QDoubleSpinBox;
class QLabel;
class QLineEdit;
class QProgressBar;
class QPushButton;
class QSpinBox;
class QTableView;
class QHBoxLayout;

namespace SuperShuckie64 {

class MemoryToolsController;

/** Search results, fetched a page at a time as rows are shown. */
class SearchResultsModel: public QAbstractTableModel {
    Q_OBJECT
public:
    enum Column { Address, Region, Current, Previous, First, Change, ColumnCount };

    SearchResultsModel(MemoryToolsController *controller, QObject *parent);

    int rowCount(const QModelIndex &parent = QModelIndex()) const override;
    int columnCount(const QModelIndex &parent = QModelIndex()) const override;
    QVariant data(const QModelIndex &index, int role) const override;
    QVariant headerData(int section, Qt::Orientation orientation, int role) const override;

    /** The results changed: forget cached rows. */
    void reset(const SuperShuckieSearchStatus &status);

    /** New current values for the rows from `first_row`. */
    void set_current(std::uint64_t first_row, const std::vector<std::optional<std::vector<std::uint8_t>>> &values);

    void set_hex(bool hex);

    /** The row's candidate, if it can be fetched. */
    std::optional<SuperShuckieSearchRow> row(int row) const;

    const SuperShuckieSearchStatus &search_status() const noexcept { return this->status; }

private:
    MemoryToolsController *controller;
    SuperShuckieSearchStatus status = {};
    bool hex = false;

    static constexpr std::uint64_t PAGE_ROWS = 256;
    static constexpr std::size_t MAX_PAGES = 64;
    mutable std::unordered_map<std::uint64_t, std::vector<SuperShuckieSearchRow>> pages;
    mutable std::list<std::uint64_t> page_order;

    std::uint64_t current_first = 0;
    std::vector<std::optional<std::vector<std::uint8_t>>> current;

    QString format(const std::uint8_t *bytes, std::size_t length) const;
    std::optional<double> number(const std::uint8_t *bytes, std::size_t length) const;
};

/** The RAM search window. */
class RamSearchWindow: public QWidget {
    Q_OBJECT
public:
    RamSearchWindow(MemoryToolsController *controller);

    /** Prefill a new search for `value` of the given type. */
    void prefill(std::uint32_t value_type, std::uint8_t size, bool big_endian, const QString &value);

    QString save_state() const;
    void restore_state(const QString &state);

protected:
    void showEvent(QShowEvent *event) override;
    void hideEvent(QHideEvent *event) override;

private slots:
    void on_refresh();
    void on_regions_changed();
    void on_tables_changed();
    void on_type_changed();
    void on_comparison_changed();
    void on_new_search();
    void on_scan();
    void on_undo();
    void on_redo();
    void on_reset();
    void on_cancel();
    void on_visible_rows_changed();
    void on_context_menu(const QPoint &position);
    void on_row_activated(const QModelIndex &index);

private:
    MemoryToolsController *controller;

    QComboBox *type_combo;
    QSpinBox *size_spin;
    QComboBox *endian_combo;
    QComboBox *alignment_combo;
    QComboBox *table_combo;
    QWidget *regions_box;
    QHBoxLayout *regions_layout;
    std::vector<QCheckBox *> region_checks;
    QCheckBox *range_check;
    QLineEdit *range_start;
    QLineEdit *range_end;
    QComboBox *comparison_combo;
    QLineEdit *operand_a;
    QLabel *and_label;
    QLineEdit *operand_b;
    QLabel *epsilon_label;
    QDoubleSpinBox *epsilon_spin;
    QPushButton *new_button;
    QPushButton *scan_button;
    QPushButton *undo_button;
    QPushButton *redo_button;
    QPushButton *reset_button;
    QPushButton *cancel_button;
    QCheckBox *pause_check;
    QComboBox *display_combo;
    QProgressBar *progress;
    QLabel *status_label;
    QTableView *table;
    SearchResultsModel *model;

    SuperShuckieSearchStatus last_status = {};
    bool was_active = false;
    std::uint64_t visible_generation = 0;

    void rebuild_comparisons();
    void update_enabled();
    std::uint32_t selected_type() const;
    std::uint32_t selected_comparison() const;
    bool fill_params(SuperShuckieSearchParams &params, std::vector<std::uint32_t> &regions, QString &error);
    void show_error(const QString &message);
};

}

#endif
