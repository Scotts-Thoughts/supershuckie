#include <QApplication>
#include <QBoxLayout>
#include <QCheckBox>
#include <QClipboard>
#include <QComboBox>
#include <QDoubleSpinBox>
#include <QGridLayout>
#include <QHeaderView>
#include <QInputDialog>
#include <QShortcut>
#include <QLabel>
#include <QLineEdit>
#include <QMenu>
#include <QProgressBar>
#include <QPushButton>
#include <QScrollBar>
#include <QSpinBox>
#include <QTableView>
#include <algorithm>
#include <climits>
#include <cstring>

#include "ram_search_window.hpp"
#include "memory_tools_controller.hpp"
#include "main_window.hpp"

using namespace SuperShuckie64;

static const char *RAM_SEARCH_PAUSE = "qt__ram_search_pause";

namespace {
    struct ComparisonInfo {
        std::uint32_t kind;
        const char *label;
        /** 0: no operand, 1: one, 2: two. */
        int operands;
        bool initial;
        bool refine;
        bool bytes;
    };

    const ComparisonInfo COMPARISONS[] = {
        { SuperShuckieSearchComparison__Equal, "Equal to", 1, true, true, false },
        { SuperShuckieSearchComparison__NotEqual, "Not equal to", 1, true, true, false },
        { SuperShuckieSearchComparison__Less, "Less than", 1, true, true, false },
        { SuperShuckieSearchComparison__LessOrEqual, "Less than or equal to", 1, true, true, false },
        { SuperShuckieSearchComparison__Greater, "Greater than", 1, true, true, false },
        { SuperShuckieSearchComparison__GreaterOrEqual, "Greater than or equal to", 1, true, true, false },
        { SuperShuckieSearchComparison__Between, "Between", 2, true, true, false },
        { SuperShuckieSearchComparison__InSet, "One of (comma-separated)", 1, true, true, false },
        { SuperShuckieSearchComparison__Unknown, "Unknown value", 0, true, false, true },
        { SuperShuckieSearchComparison__Pattern, "Matching", 1, true, true, true },
        { SuperShuckieSearchComparison__Changed, "Changed", 0, false, true, true },
        { SuperShuckieSearchComparison__Unchanged, "Unchanged", 0, false, true, true },
        { SuperShuckieSearchComparison__Increased, "Increased", 0, false, true, false },
        { SuperShuckieSearchComparison__Decreased, "Decreased", 0, false, true, false },
        { SuperShuckieSearchComparison__IncreasedBy, "Increased by", 1, false, true, false },
        { SuperShuckieSearchComparison__DecreasedBy, "Decreased by", 1, false, true, false },
        { SuperShuckieSearchComparison__ChangedBy, "Changed by (±)", 1, false, true, false },
        { SuperShuckieSearchComparison__ChangedByAtLeast, "Changed by at least (±)", 1, false, true, false },
        { SuperShuckieSearchComparison__EqualToFirst, "Same as first scan", 0, false, true, true },
        { SuperShuckieSearchComparison__NotEqualToFirst, "Different from first scan", 0, false, true, true },
        { SuperShuckieSearchComparison__IncreasedSinceFirst, "Increased since first scan", 0, false, true, false },
        { SuperShuckieSearchComparison__DecreasedSinceFirst, "Decreased since first scan", 0, false, true, false },
    };

    const ComparisonInfo *comparison_info(std::uint32_t kind) {
        for(auto &info : COMPARISONS) {
            if(info.kind == kind) {
                return &info;
            }
        }
        return &COMPARISONS[0];
    }

    const char *TYPE_NAMES[] = { "u8", "i8", "u16", "i16", "u32", "i32", "f32", "BCD", "Bytes", "Text" };

    bool type_has_size(std::uint32_t type) {
        return type == SuperShuckieMemoryValueType__BCD || type == SuperShuckieMemoryValueType__Bytes || type == SuperShuckieMemoryValueType__Text;
    }

    bool type_is_numeric(std::uint32_t type) {
        return type != SuperShuckieMemoryValueType__Bytes && type != SuperShuckieMemoryValueType__Text;
    }

    int fixed_size(std::uint32_t type) {
        switch(type) {
            case SuperShuckieMemoryValueType__U8: case SuperShuckieMemoryValueType__I8: return 1;
            case SuperShuckieMemoryValueType__U16: case SuperShuckieMemoryValueType__I16: return 2;
            case SuperShuckieMemoryValueType__U32: case SuperShuckieMemoryValueType__I32: case SuperShuckieMemoryValueType__F32: return 4;
            default: return 0;
        }
    }
}

// ---------------------------------------------------------------------------------------------

SearchResultsModel::SearchResultsModel(MemoryToolsController *controller, QObject *parent): QAbstractTableModel(parent), controller(controller) {}

int SearchResultsModel::rowCount(const QModelIndex &parent) const {
    if(parent.isValid() || !this->status.active || this->status.busy) {
        return 0;
    }
    return static_cast<int>(std::min<std::uint64_t>(this->status.result_count, INT_MAX));
}

int SearchResultsModel::columnCount(const QModelIndex &parent) const {
    return parent.isValid() ? 0 : ColumnCount;
}

QVariant SearchResultsModel::headerData(int section, Qt::Orientation orientation, int role) const {
    if(orientation != Qt::Horizontal || role != Qt::DisplayRole) {
        return {};
    }
    switch(section) {
        case Address: return "Address";
        case Region: return "Region";
        case Current: return "Current";
        case Previous: return "Previous scan";
        case First: return "First scan";
        case Change: return "Change";
        default: return {};
    }
}

std::optional<SuperShuckieSearchRow> SearchResultsModel::row(int row) const {
    if(row < 0) {
        return std::nullopt;
    }
    std::uint64_t page = static_cast<std::uint64_t>(row) / PAGE_ROWS;
    auto found = this->pages.find(page);
    if(found == this->pages.end()) {
        std::vector<SuperShuckieSearchRow> rows(PAGE_ROWS);
        std::size_t count = supershuckie_frontend_search_results(this->controller->frontend(), page * PAGE_ROWS, rows.data(), rows.size());
        if(count == 0) {
            return std::nullopt;
        }
        rows.resize(count);
        found = this->pages.emplace(page, std::move(rows)).first;
        this->page_order.push_back(page);
        while(this->page_order.size() > MAX_PAGES) {
            this->pages.erase(this->page_order.front());
            this->page_order.pop_front();
        }
    }
    std::size_t index = static_cast<std::size_t>(row) % PAGE_ROWS;
    if(index >= found->second.size()) {
        return std::nullopt;
    }
    return found->second[index];
}

QString SearchResultsModel::format(const std::uint8_t *bytes, std::size_t length) const {
    return this->controller->format_value(0, this->status.value_type, this->status.size, this->status.big_endian, this->hex ? SuperShuckieMemoryDisplay__Hex : SuperShuckieMemoryDisplay__Decimal, bytes, length);
}

std::optional<double> SearchResultsModel::number(const std::uint8_t *bytes, std::size_t length) const {
    if(!type_is_numeric(this->status.value_type)) {
        return std::nullopt;
    }
    bool ok = false;
    double value = this->controller->format_value(0, this->status.value_type, this->status.size, this->status.big_endian, SuperShuckieMemoryDisplay__Decimal, bytes, length).toDouble(&ok);
    return ok ? std::optional(value) : std::nullopt;
}

QVariant SearchResultsModel::data(const QModelIndex &index, int role) const {
    if(!index.isValid() || (role != Qt::DisplayRole && role != Qt::TextAlignmentRole)) {
        return {};
    }
    if(role == Qt::TextAlignmentRole) {
        return index.column() == Region ? QVariant(Qt::AlignLeft | Qt::AlignVCenter) : QVariant(Qt::AlignRight | Qt::AlignVCenter);
    }
    auto row = this->row(index.row());
    if(!row) {
        return {};
    }

    const std::vector<std::uint8_t> *current = nullptr;
    if(static_cast<std::uint64_t>(index.row()) >= this->current_first) {
        std::uint64_t i = static_cast<std::uint64_t>(index.row()) - this->current_first;
        if(i < this->current.size() && this->current[i]) {
            current = &*this->current[i];
        }
    }

    switch(index.column()) {
        case Address:
            return this->controller->format_address(row->address, false);
        case Region: {
            auto &regions = this->controller->regions();
            return row->region < regions.size() ? QVariant(regions[row->region].short_name) : QVariant();
        }
        case Current:
            return current != nullptr ? QVariant(this->format(current->data(), current->size())) : QVariant();
        case Previous:
            return this->format(row->previous, row->length);
        case First:
            return this->format(row->first, row->length);
        case Change: {
            if(current == nullptr) {
                return {};
            }
            auto now = this->number(current->data(), current->size());
            auto before = this->number(row->previous, row->length);
            if(!now || !before) {
                return current->size() == row->length && std::memcmp(current->data(), row->previous, row->length) != 0 ? QVariant("changed") : QVariant();
            }
            double delta = *now - *before;
            if(delta == 0) {
                return {};
            }
            return QString("%1%2").arg(delta > 0 ? "+" : "").arg(delta);
        }
        default:
            return {};
    }
}

void SearchResultsModel::reset(const SuperShuckieSearchStatus &status) {
    this->beginResetModel();
    this->status = status;
    this->pages.clear();
    this->page_order.clear();
    this->current.clear();
    this->endResetModel();
}

void SearchResultsModel::set_current(std::uint64_t first_row, const std::vector<std::optional<std::vector<std::uint8_t>>> &values) {
    this->current_first = first_row;
    this->current = values;
    if(!values.empty() && this->rowCount() > 0) {
        int first = static_cast<int>(std::min<std::uint64_t>(first_row, this->rowCount() - 1));
        int last = static_cast<int>(std::min<std::uint64_t>(first_row + values.size() - 1, this->rowCount() - 1));
        emit this->dataChanged(this->index(first, Current), this->index(last, Change), { Qt::DisplayRole });
    }
}

std::optional<std::vector<std::uint8_t>> SearchResultsModel::current_value(int row) const {
    if(row < 0 || static_cast<std::uint64_t>(row) < this->current_first) {
        return std::nullopt;
    }
    std::uint64_t i = static_cast<std::uint64_t>(row) - this->current_first;
    if(i >= this->current.size()) {
        return std::nullopt;
    }
    return this->current[i];
}

void SearchResultsModel::set_hex(bool hex) {
    this->hex = hex;
    if(this->rowCount() > 0) {
        emit this->dataChanged(this->index(0, Current), this->index(this->rowCount() - 1, Change), { Qt::DisplayRole });
    }
}

// ---------------------------------------------------------------------------------------------

RamSearchWindow::RamSearchWindow(MemoryToolsController *controller): QWidget(controller->main_window(), Qt::Window), controller(controller) {
    this->setWindowTitle("RAM search");

    auto *layout = new QVBoxLayout(this);
    layout->setContentsMargins(8, 8, 8, 8);

    auto *form = new QGridLayout();
    form->setHorizontalSpacing(6);
    layout->addLayout(form);

    // Value.
    form->addWidget(new QLabel("Value", this), 0, 0);
    auto *value_row = new QHBoxLayout();
    this->type_combo = new QComboBox(this);
    for(auto *name : TYPE_NAMES) {
        this->type_combo->addItem(name);
    }
    this->type_combo->setToolTip("What kind of value to look for");
    value_row->addWidget(this->type_combo);
    value_row->addWidget(new QLabel("Size", this));
    this->size_spin = new QSpinBox(this);
    this->size_spin->setRange(1, SUPERSHUCKIE_MEMORY_MAX_VALUE_SIZE);
    this->size_spin->setToolTip("Bytes (BCD: 1-4, two digits each)");
    value_row->addWidget(this->size_spin);
    this->endian_combo = new QComboBox(this);
    this->endian_combo->addItems({ "Little-endian", "Big-endian" });
    value_row->addWidget(this->endian_combo);
    value_row->addWidget(new QLabel("Aligned to", this));
    this->alignment_combo = new QComboBox(this);
    this->alignment_combo->addItems({ "1", "2", "4" });
    this->alignment_combo->setToolTip("Only look at addresses that are multiples of this from the region start");
    value_row->addWidget(this->alignment_combo);
    value_row->addWidget(new QLabel("Table", this));
    this->table_combo = new QComboBox(this);
    this->table_combo->addItems(this->controller->table_names());
    this->table_combo->setToolTip("Character table for text");
    value_row->addWidget(this->table_combo);
    value_row->addStretch(1);
    form->addLayout(value_row, 0, 1);

    // Where.
    form->addWidget(new QLabel("Regions", this), 1, 0);
    auto *where_row = new QHBoxLayout();
    this->regions_box = new QWidget(this);
    this->regions_layout = new QHBoxLayout(this->regions_box);
    this->regions_layout->setContentsMargins(0, 0, 0, 0);
    where_row->addWidget(this->regions_box);
    auto *all_button = new QPushButton("All", this);
    auto *writable_button = new QPushButton("Writable", this);
    all_button->setAutoDefault(false);
    writable_button->setAutoDefault(false);
    where_row->addWidget(all_button);
    where_row->addWidget(writable_button);
    where_row->addSpacing(12);
    this->range_check = new QCheckBox("Only from", this);
    this->range_start = new QLineEdit(this);
    this->range_start->setPlaceholderText("start");
    this->range_end = new QLineEdit(this);
    this->range_end->setPlaceholderText("end (exclusive)");
    where_row->addWidget(this->range_check);
    where_row->addWidget(this->range_start);
    where_row->addWidget(new QLabel("to", this));
    where_row->addWidget(this->range_end);
    where_row->addStretch(1);
    form->addLayout(where_row, 1, 1);

    // Comparison.
    form->addWidget(new QLabel("Compare", this), 2, 0);
    auto *compare_row = new QHBoxLayout();
    this->comparison_combo = new QComboBox(this);
    compare_row->addWidget(this->comparison_combo);
    this->operand_a = new QLineEdit(this);
    this->operand_a->setMinimumWidth(140);
    compare_row->addWidget(this->operand_a, 1);
    this->and_label = new QLabel("and", this);
    compare_row->addWidget(this->and_label);
    this->operand_b = new QLineEdit(this);
    compare_row->addWidget(this->operand_b, 1);
    this->epsilon_label = new QLabel("±", this);
    compare_row->addWidget(this->epsilon_label);
    this->epsilon_spin = new QDoubleSpinBox(this);
    this->epsilon_spin->setDecimals(4);
    this->epsilon_spin->setRange(0.0001, 1000.0);
    this->epsilon_spin->setValue(0.01);
    this->epsilon_spin->setToolTip("Floats this close together count as equal");
    compare_row->addWidget(this->epsilon_spin);
    form->addLayout(compare_row, 2, 1);

    // Actions.
    auto *buttons = new QHBoxLayout();
    this->new_button = new QPushButton("New search", this);
    this->new_button->setToolTip("Start over: scan all selected memory with this comparison");
    this->scan_button = new QPushButton("Scan", this);
    this->scan_button->setToolTip("Narrow the results with this comparison (Enter)");
    this->scan_button->setDefault(true);
    this->undo_button = new QPushButton("Undo", this);
    this->redo_button = new QPushButton("Redo", this);
    this->reset_button = new QPushButton("Reset", this);
    this->cancel_button = new QPushButton("Cancel", this);
    for(auto *button : { this->new_button, this->scan_button, this->undo_button, this->redo_button, this->reset_button, this->cancel_button }) {
        buttons->addWidget(button);
    }
    buttons->addSpacing(12);
    this->pause_check = new QCheckBox("Pause while scanning", this);
    const char *pause_setting = supershuckie_frontend_get_custom_setting(this->controller->frontend(), RAM_SEARCH_PAUSE);
    this->pause_check->setChecked(pause_setting != nullptr && pause_setting[0] == '1');
    buttons->addWidget(this->pause_check);
    buttons->addStretch(1);
    buttons->addWidget(new QLabel("Show", this));
    this->display_combo = new QComboBox(this);
    this->display_combo->addItems({ "Decimal", "Hexadecimal" });
    buttons->addWidget(this->display_combo);
    layout->addLayout(buttons);

    auto *status_row = new QHBoxLayout();
    this->status_label = new QLabel(this);
    this->status_label->setWordWrap(true);
    status_row->addWidget(this->status_label, 1);
    this->progress = new QProgressBar(this);
    this->progress->setRange(0, 1000);
    this->progress->setTextVisible(false);
    this->progress->setMaximumWidth(160);
    this->progress->hide();
    status_row->addWidget(this->progress);
    layout->addLayout(status_row);

    this->model = new SearchResultsModel(controller, this);
    this->table = new QTableView(this);
    this->table->setModel(this->model);
    this->table->setSelectionBehavior(QAbstractItemView::SelectRows);
    this->table->setSelectionMode(QAbstractItemView::ExtendedSelection);
    this->table->verticalHeader()->hide();
    this->table->verticalHeader()->setDefaultSectionSize(this->table->fontMetrics().height() + 4);
    this->table->verticalHeader()->setSectionResizeMode(QHeaderView::Fixed);
    this->table->horizontalHeader()->setStretchLastSection(true);
    this->table->setContextMenuPolicy(Qt::CustomContextMenu);
    this->table->setWordWrap(false);
    layout->addWidget(this->table, 1);

    connect(this->type_combo, SIGNAL(currentIndexChanged(int)), this, SLOT(on_type_changed()));
    connect(this->comparison_combo, SIGNAL(currentIndexChanged(int)), this, SLOT(on_comparison_changed()));
    connect(this->new_button, SIGNAL(clicked()), this, SLOT(on_new_search()));
    connect(this->scan_button, SIGNAL(clicked()), this, SLOT(on_scan()));
    connect(this->operand_a, SIGNAL(returnPressed()), this, SLOT(on_scan()));
    connect(this->operand_b, SIGNAL(returnPressed()), this, SLOT(on_scan()));
    connect(this->undo_button, SIGNAL(clicked()), this, SLOT(on_undo()));
    connect(this->redo_button, SIGNAL(clicked()), this, SLOT(on_redo()));
    connect(this->reset_button, SIGNAL(clicked()), this, SLOT(on_reset()));
    connect(this->cancel_button, SIGNAL(clicked()), this, SLOT(on_cancel()));
    connect(this->pause_check, &QCheckBox::toggled, this, [this](bool checked) {
        supershuckie_frontend_set_custom_setting(this->controller->frontend(), RAM_SEARCH_PAUSE, checked ? "1" : "0");
    });
    connect(this->display_combo, &QComboBox::currentIndexChanged, this, [this](int index) {
        this->model->set_hex(index == 1);
    });
    connect(all_button, &QPushButton::clicked, this, [this]() {
        for(auto *check : this->region_checks) {
            check->setChecked(true);
        }
    });
    connect(writable_button, &QPushButton::clicked, this, [this]() {
        auto &regions = this->controller->regions();
        for(std::size_t i = 0; i < this->region_checks.size() && i < regions.size(); i++) {
            this->region_checks[i]->setChecked(regions[i].writable);
        }
    });
    connect(this->table->verticalScrollBar(), &QScrollBar::valueChanged, this, &RamSearchWindow::on_visible_rows_changed);
    connect(this->table->verticalScrollBar(), &QScrollBar::rangeChanged, this, &RamSearchWindow::on_visible_rows_changed);
    connect(this->table, &QTableView::customContextMenuRequested, this, &RamSearchWindow::on_context_menu);
    connect(this->table, &QTableView::doubleClicked, this, &RamSearchWindow::on_row_activated);
    connect(this->controller, &MemoryToolsController::refresh, this, &RamSearchWindow::on_refresh);
    connect(this->controller, &MemoryToolsController::regions_changed, this, &RamSearchWindow::on_regions_changed);
    connect(this->controller, &MemoryToolsController::tables_changed, this, &RamSearchWindow::on_tables_changed);
    connect(this->controller, &MemoryToolsController::message, this, [this](const QString &text) {
        if(this->isActiveWindow()) {
            this->status_label->setText(text);
        }
    });
    auto *undo = new QShortcut(QKeySequence::Undo, this);
    connect(undo, &QShortcut::activated, this, [this]() { this->controller->undo(this); });
    auto *redo = new QShortcut(QKeySequence::Redo, this);
    connect(redo, &QShortcut::activated, this, [this]() { this->controller->redo(this); });

    this->on_regions_changed();
    this->on_type_changed();
    this->resize(820, 600);
    this->on_refresh();
}

std::uint32_t RamSearchWindow::selected_type() const {
    return static_cast<std::uint32_t>(std::max(0, this->type_combo->currentIndex()));
}

std::uint32_t RamSearchWindow::selected_comparison() const {
    return this->comparison_combo->currentData().toUInt();
}

void RamSearchWindow::on_regions_changed() {
    auto &regions = this->controller->regions();
    for(auto *check : this->region_checks) {
        delete check;
    }
    this->region_checks.clear();
    for(auto &region : regions) {
        auto *check = new QCheckBox(region.short_name, this->regions_box);
        check->setToolTip(QString("%1 (%2 bytes)%3").arg(region.name).arg(region.length).arg(region.writable ? "" : ", read-only"));
        // Searching I/O registers is rarely what anyone wants.
        check->setChecked(region.writable);
        this->regions_layout->addWidget(check);
        this->region_checks.push_back(check);
    }

    // Default byte order and alignment for the console.
    if(!this->last_status.active && !regions.empty()) {
        this->endian_combo->setCurrentIndex(regions[0].big_endian ? 1 : 0);
    }
    this->update_enabled();
}

void RamSearchWindow::on_tables_changed() {
    QString current = this->table_combo->currentText();
    this->table_combo->clear();
    this->table_combo->addItems(this->controller->table_names());
    int index = this->table_combo->findText(current);
    this->table_combo->setCurrentIndex(index >= 0 ? index : 0);
}

void RamSearchWindow::on_type_changed() {
    auto type = this->selected_type();
    bool has_size = type_has_size(type);
    if(type == SuperShuckieMemoryValueType__BCD) {
        this->size_spin->setRange(1, 4);
    }
    else {
        this->size_spin->setRange(1, SUPERSHUCKIE_MEMORY_MAX_VALUE_SIZE);
    }
    if(!has_size) {
        this->size_spin->setValue(fixed_size(type));
    }
    // Default alignment: the value's size on 32-bit consoles, 1 on the Game Boy.
    auto &regions = this->controller->regions();
    bool game_boy = !regions.empty() && regions[0].big_endian;
    int size = has_size ? 1 : fixed_size(type);
    this->alignment_combo->setCurrentIndex(game_boy || !type_is_numeric(type) ? 0 : size >= 4 ? 2 : size >= 2 ? 1 : 0);
    this->rebuild_comparisons();
}

void RamSearchWindow::rebuild_comparisons() {
    std::uint32_t previous = this->selected_comparison();
    auto type = this->last_status.active ? this->last_status.value_type : this->selected_type();
    bool numeric = type_is_numeric(type);
    bool active = this->last_status.active;

    this->comparison_combo->blockSignals(true);
    this->comparison_combo->clear();
    for(auto &info : COMPARISONS) {
        if(active ? !info.refine && !info.initial : !info.initial) {
            continue;
        }
        if(info.kind == SuperShuckieSearchComparison__Pattern && numeric) {
            continue;
        }
        if(!numeric && !info.bytes) {
            continue;
        }
        QString label = info.label;
        if(info.kind == SuperShuckieSearchComparison__Pattern) {
            label = type == SuperShuckieMemoryValueType__Text ? "Text" : "Bytes matching (?? = any)";
        }
        this->comparison_combo->addItem(label, info.kind);
    }
    int index = this->comparison_combo->findData(previous);
    this->comparison_combo->setCurrentIndex(index >= 0 ? index : 0);
    this->comparison_combo->blockSignals(false);
    this->on_comparison_changed();
}

void RamSearchWindow::on_comparison_changed() {
    auto *info = comparison_info(this->selected_comparison());
    this->operand_a->setVisible(info->operands >= 1);
    this->and_label->setVisible(info->operands >= 2);
    this->operand_b->setVisible(info->operands >= 2);
    auto type = this->last_status.active ? this->last_status.value_type : this->selected_type();
    bool is_float = type == SuperShuckieMemoryValueType__F32;
    this->epsilon_label->setVisible(is_float && info->operands >= 1);
    this->epsilon_spin->setVisible(is_float && info->operands >= 1);
    if(info->kind == SuperShuckieSearchComparison__Pattern) {
        this->operand_a->setPlaceholderText(type == SuperShuckieMemoryValueType__Text ? "text" : "12 ?? 3F");
    }
    else {
        this->operand_a->setPlaceholderText(type == SuperShuckieMemoryValueType__BCD ? "digits" : "value (123, -5, 0x7B)");
    }
}

void RamSearchWindow::update_enabled() {
    bool active = this->last_status.active;
    bool busy = this->last_status.busy;
    bool game = !this->controller->regions().empty();
    auto type = this->selected_type();

    for(QWidget *w : std::initializer_list<QWidget *>{ this->type_combo, this->alignment_combo, this->regions_box, this->range_check, this->range_start, this->range_end }) {
        w->setEnabled(!active && !busy);
    }
    this->size_spin->setEnabled(!active && !busy && type_has_size(type));
    this->endian_combo->setEnabled(!active && !busy && type != SuperShuckieMemoryValueType__U8 && type != SuperShuckieMemoryValueType__I8 && type_is_numeric(type));
    this->table_combo->setEnabled(!busy && type == SuperShuckieMemoryValueType__Text);
    this->new_button->setEnabled(game && !busy);
    this->scan_button->setEnabled(game && active && !busy);
    this->undo_button->setEnabled(active && !busy && this->last_status.can_undo);
    this->redo_button->setEnabled(active && !busy && this->last_status.can_redo);
    this->reset_button->setEnabled((active || busy));
    this->cancel_button->setEnabled(busy);
    this->comparison_combo->setEnabled(!busy);
}

void RamSearchWindow::on_refresh() {
    if(!this->isVisible()) {
        return;
    }
    SuperShuckieSearchStatus status = {};
    char message[512] = {};
    bool has_message = supershuckie_frontend_search_status(this->controller->frontend(), &status, message, sizeof(message));

    bool changed = status.generation != this->last_status.generation || status.busy != this->last_status.busy || status.active != this->last_status.active;
    bool active_changed = status.active != this->last_status.active;
    this->last_status = status;
    if(changed) {
        this->model->reset(status);
        this->visible_generation = 0;
        if(active_changed) {
            if(status.active) {
                // Show the active search's format.
                this->type_combo->blockSignals(true);
                this->type_combo->setCurrentIndex(static_cast<int>(status.value_type));
                this->type_combo->blockSignals(false);
                this->size_spin->setValue(status.size);
                this->endian_combo->setCurrentIndex(status.big_endian ? 1 : 0);
                this->alignment_combo->setCurrentIndex(status.alignment >= 4 ? 2 : status.alignment >= 2 ? 1 : 0);
            }
            this->rebuild_comparisons();
        }
        this->on_visible_rows_changed();
    }

    if(status.busy) {
        this->progress->show();
        this->progress->setValue(static_cast<int>(status.progress_per_mille));
        this->status_label->setText(status.progress_per_mille == 0 ? "Waiting for the next frame…" : QString("Scanning… %1%").arg(status.progress_per_mille / 10));
    }
    else {
        this->progress->hide();
        QString text;
        if(status.active) {
            text = QString("%1 result%2  ·  scan %3 at frame %4")
                .arg(QLocale().toString(static_cast<qulonglong>(status.result_count)))
                .arg(status.result_count == 1 ? "" : "s")
                .arg(status.steps)
                .arg(status.frame);
            if(status.state_changed) {
                text += "  ·  memory was replaced (state load or seek) since the scan before";
            }
        }
        else if(this->controller->regions().empty()) {
            text = "No game is loaded.";
        }
        else {
            text = "Choose what to look for and press New search.";
        }
        if(has_message) {
            text += QString("  ·  %1").arg(QString::fromUtf8(message));
        }
        this->status_label->setText(text);
    }
    this->update_enabled();

    // Current values of the rows on screen.
    if(status.active && !status.busy && this->model->rowCount() > 0) {
        std::uint64_t first_row = 0;
        std::uint64_t generation = 0;
        std::vector<std::uint8_t> values(512 * 64);
        bool ok[512] = {};
        std::size_t count = supershuckie_frontend_search_read_visible(this->controller->frontend(), &first_row, &generation, values.data(), ok, 512);
        if(generation != this->visible_generation) {
            this->visible_generation = generation;
            std::vector<std::optional<std::vector<std::uint8_t>>> current(count);
            for(std::size_t i = 0; i < count; i++) {
                if(ok[i]) {
                    std::size_t length = std::max<std::size_t>(1, this->last_status.size);
                    current[i] = std::vector<std::uint8_t>(values.begin() + i * 64, values.begin() + i * 64 + length);
                }
            }
            this->model->set_current(first_row, current);
        }
    }
}

void RamSearchWindow::on_visible_rows_changed() {
    int rows = this->model->rowCount();
    if(rows == 0) {
        supershuckie_frontend_search_set_visible_rows(this->controller->frontend(), 0, 0);
        return;
    }
    int first = std::max(0, this->table->rowAt(0));
    int last = this->table->rowAt(this->table->viewport()->height() - 1);
    if(last < 0) {
        last = rows - 1;
    }
    int count = std::min(512, last - first + 2);
    supershuckie_frontend_search_set_visible_rows(this->controller->frontend(), static_cast<std::uint64_t>(first), static_cast<std::uint32_t>(std::max(0, count)));
}

bool RamSearchWindow::fill_params(SuperShuckieSearchParams &params, std::vector<std::uint32_t> &regions, QString &error) {
    params = {};
    params.value_type = this->selected_type();
    params.size = static_cast<std::uint8_t>(this->size_spin->value());
    params.big_endian = this->endian_combo->currentIndex() == 1;
    static const std::uint8_t ALIGNMENTS[] = { 1, 2, 4 };
    params.alignment = ALIGNMENTS[std::clamp(this->alignment_combo->currentIndex(), 0, 2)];
    params.table = static_cast<std::size_t>(std::max(0, this->table_combo->currentIndex()));
    params.epsilon = this->epsilon_spin->value();

    regions.clear();
    for(std::size_t i = 0; i < this->region_checks.size(); i++) {
        if(this->region_checks[i]->isChecked()) {
            regions.push_back(static_cast<std::uint32_t>(i));
        }
    }
    if(regions.empty()) {
        error = "Choose at least one region to search.";
        return false;
    }
    params.regions = regions.data();
    params.region_count = regions.size();

    if(this->range_check->isChecked()) {
        QString parse_error;
        auto start = this->controller->parse_address(this->range_start->text(), &parse_error);
        auto end = start ? this->controller->parse_address(this->range_end->text(), &parse_error) : std::nullopt;
        if(!start || !end) {
            error = QString("Address range: %1").arg(parse_error);
            return false;
        }
        params.use_range = true;
        params.range_start = *start;
        params.range_end = *end;
    }
    return true;
}

void RamSearchWindow::show_error(const QString &message) {
    this->status_label->setText(message);
}

void RamSearchWindow::on_new_search() {
    auto *info = comparison_info(this->selected_comparison());
    std::uint32_t comparison = info->kind;
    if(!info->initial) {
        this->show_error("A new search needs a comparison with a value (or Unknown value).");
        return;
    }
    SuperShuckieSearchParams params;
    std::vector<std::uint32_t> regions;
    QString error;
    if(!this->fill_params(params, regions, error)) {
        this->show_error(error);
        return;
    }
    if(this->last_status.active) {
        // The format may change with a new search.
        supershuckie_frontend_search_reset(this->controller->frontend());
    }
    char error_buffer[512] = {};
    if(!supershuckie_frontend_search_new(this->controller->frontend(), &params, comparison, this->operand_a->text().toUtf8().constData(), this->operand_b->text().toUtf8().constData(), this->pause_check->isChecked(), error_buffer, sizeof(error_buffer))) {
        this->show_error(QString::fromUtf8(error_buffer));
        return;
    }
    this->on_refresh();
}

void RamSearchWindow::on_scan() {
    if(!this->last_status.active) {
        this->on_new_search();
        return;
    }
    auto *info = comparison_info(this->selected_comparison());
    if(!info->refine) {
        this->show_error("That comparison only starts a new search.");
        return;
    }
    char error_buffer[512] = {};
    if(!supershuckie_frontend_search_refine(this->controller->frontend(), info->kind, this->operand_a->text().toUtf8().constData(), this->operand_b->text().toUtf8().constData(), static_cast<std::size_t>(std::max(0, this->table_combo->currentIndex())), this->pause_check->isChecked(), error_buffer, sizeof(error_buffer))) {
        this->show_error(QString::fromUtf8(error_buffer));
        return;
    }
    this->on_refresh();
}

void RamSearchWindow::on_undo() {
    supershuckie_frontend_search_undo(this->controller->frontend());
}

void RamSearchWindow::on_redo() {
    supershuckie_frontend_search_redo(this->controller->frontend());
}

void RamSearchWindow::on_reset() {
    supershuckie_frontend_search_reset(this->controller->frontend());
}

void RamSearchWindow::on_cancel() {
    supershuckie_frontend_search_cancel(this->controller->frontend());
}

void RamSearchWindow::on_row_activated(const QModelIndex &index) {
    auto row = this->model->row(index.row());
    if(row) {
        this->controller->show_in_viewer(row->address, std::max<std::uint32_t>(1, row->length));
    }
}

void RamSearchWindow::on_context_menu(const QPoint &position) {
    auto index = this->table->indexAt(position);
    if(!index.isValid()) {
        return;
    }
    auto row = this->model->row(index.row());
    if(!row) {
        return;
    }
    QMenu menu(this);
    auto *show = menu.addAction("Show in RAM viewer");
    connect(show, &QAction::triggered, this, [this, row]() {
        this->controller->show_in_viewer(row->address, std::max<std::uint32_t>(1, row->length));
    });
    auto *copy = menu.addAction(QString("Copy address (%1)").arg(this->controller->format_address(row->address, false)));
    connect(copy, &QAction::triggered, this, [this, row]() {
        QApplication::clipboard()->setText(this->controller->format_address(row->address, false));
    });

    std::vector<std::uint32_t> addresses;
    for(auto &selected : this->table->selectionModel()->selectedRows()) {
        auto selected_row = this->model->row(selected.row());
        if(selected_row) {
            addresses.push_back(selected_row->address);
        }
        if(addresses.size() >= 256) {
            break;
        }
    }
    if(addresses.empty()) {
        addresses.push_back(row->address);
    }
    auto status = this->model->search_status();
    auto *add_watch = menu.addAction(addresses.size() == 1 ? QString("Add to watch list") : QString("Add %1 results to watch list").arg(addresses.size()));
    connect(add_watch, &QAction::triggered, this, [this, addresses, status]() {
        this->controller->add_watches(addresses, status.value_type, status.size, status.big_endian);
    });

    menu.addSeparator();
    auto *set_value = menu.addAction(addresses.size() == 1 ? QString("Set value…") : QString("Set %1 values…").arg(addresses.size()));
    connect(set_value, &QAction::triggered, this, [this, addresses, status]() {
        bool ok = false;
        QString text = QInputDialog::getText(this, "Set value", QString("New value for %1 address%2:").arg(addresses.size()).arg(addresses.size() == 1 ? "" : "es"), QLineEdit::Normal, QString(), &ok);
        if(!ok) {
            return;
        }
        auto bytes = this->controller->parse_value(this, static_cast<std::size_t>(std::max(0, this->table_combo->currentIndex())), status.value_type, status.size, status.big_endian, text);
        if(!bytes) {
            return;
        }
        if(bytes->size() < status.size) {
            bytes->append(QByteArray(status.size - bytes->size(), '\0'));
        }
        for(auto address : addresses) {
            if(!this->controller->write(this, address, *bytes)) {
                break;
            }
        }
    });
    auto *freeze = menu.addAction(addresses.size() == 1 ? QString("Freeze at current value") : QString("Freeze %1 at their current values").arg(addresses.size()));
    connect(freeze, &QAction::triggered, this, [this, addresses, status]() {
        for(auto address : addresses) {
            // The value on screen if it is sampled, else the value at the last scan.
            QByteArray value;
            for(int r = 0; r < this->model->rowCount(); r++) {
                auto candidate = this->model->row(r);
                if(candidate && candidate->address == address) {
                    auto current = this->model->current_value(r);
                    value = current ? QByteArray(reinterpret_cast<const char *>(current->data()), static_cast<qsizetype>(current->size())) : QByteArray(reinterpret_cast<const char *>(candidate->previous), candidate->length);
                    break;
                }
                if(r > 100000) {
                    break;
                }
            }
            if(value.isEmpty() || this->controller->freeze(this, address, status.value_type, status.size, status.big_endian, value, "Frozen from search") == 0) {
                break;
            }
        }
    });
    menu.exec(this->table->viewport()->mapToGlobal(position));
}

void RamSearchWindow::prefill(std::uint32_t value_type, std::uint8_t size, bool big_endian, const QString &value) {
    if(this->last_status.active) {
        supershuckie_frontend_search_reset(this->controller->frontend());
        this->last_status.active = false;
    }
    this->type_combo->setCurrentIndex(static_cast<int>(value_type));
    this->size_spin->setValue(size);
    this->endian_combo->setCurrentIndex(big_endian ? 1 : 0);
    this->rebuild_comparisons();
    int equal = this->comparison_combo->findData(SuperShuckieSearchComparison__Equal);
    if(equal >= 0) {
        this->comparison_combo->setCurrentIndex(equal);
    }
    this->operand_a->setText(value);
    this->operand_a->setFocus();
    this->update_enabled();
}

void RamSearchWindow::showEvent(QShowEvent *event) {
    QWidget::showEvent(event);
    this->controller->visibility_changed();
    this->on_refresh();
    this->on_visible_rows_changed();
}

void RamSearchWindow::hideEvent(QHideEvent *event) {
    QWidget::hideEvent(event);
    supershuckie_frontend_search_set_visible_rows(this->controller->frontend(), 0, 0);
    this->controller->visibility_changed();
}

QString RamSearchWindow::save_state() const {
    return QString::fromLatin1(this->saveGeometry().toBase64());
}

void RamSearchWindow::restore_state(const QString &state) {
    this->restoreGeometry(QByteArray::fromBase64(state.toLatin1()));
}
