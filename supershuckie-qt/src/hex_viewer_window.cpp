#include <QApplication>
#include <QBoxLayout>
#include <QClipboard>
#include <QComboBox>
#include <QEvent>
#include <QHeaderView>
#include <QLabel>
#include <QLineEdit>
#include <QMenu>
#include <QShortcut>
#include <QSplitter>
#include <QStackedWidget>
#include <QTabBar>
#include <QTableWidget>
#include <QToolButton>
#include <algorithm>

#include "hex_viewer_window.hpp"
#include "hex_view_widget.hpp"
#include "memory_tools_controller.hpp"
#include "main_window.hpp"

using namespace SuperShuckie64;

static const int REFRESH_RATES[] = { 15, 30, 60 };

namespace {
    /** One row of the data inspector: a value type read at the cursor. */
    struct InspectorRow {
        const char *label;
        std::uint32_t value_type;
        std::uint8_t size;
        bool endianness;
    };

    const InspectorRow INSPECTOR_ROWS[] = {
        { "u8", SuperShuckieMemoryValueType__U8, 1, false },
        { "i8", SuperShuckieMemoryValueType__I8, 1, false },
        { "u16", SuperShuckieMemoryValueType__U16, 2, true },
        { "i16", SuperShuckieMemoryValueType__I16, 2, true },
        { "u32", SuperShuckieMemoryValueType__U32, 4, true },
        { "i32", SuperShuckieMemoryValueType__I32, 4, true },
        { "f32", SuperShuckieMemoryValueType__F32, 4, true },
        { "BCD (2 digits)", SuperShuckieMemoryValueType__BCD, 1, false },
        { "BCD (4 digits)", SuperShuckieMemoryValueType__BCD, 2, true },
        { "BCD (6 digits)", SuperShuckieMemoryValueType__BCD, 3, true },
        { "BCD (8 digits)", SuperShuckieMemoryValueType__BCD, 4, true },
        { "binary", SuperShuckieMemoryValueType__U8, 1, false },
    };
    constexpr int INSPECTOR_POINTER_ROW = sizeof(INSPECTOR_ROWS) / sizeof(INSPECTOR_ROWS[0]);
    constexpr int INSPECTOR_TEXT_ROW = INSPECTOR_POINTER_ROW + 1;
    constexpr std::size_t INSPECTOR_TEXT_BYTES = 16;
}

HexViewerWindow::HexViewerWindow(MemoryToolsController *controller, std::uint8_t slot):
    QWidget(controller->main_window(), Qt::Window), controller(controller), viewer_slot(slot) {

    this->buffer.resize(SUPERSHUCKIE_MEMORY_MAX_VIEWER_BYTES);

    auto *layout = new QVBoxLayout(this);
    layout->setContentsMargins(6, 6, 6, 6);
    layout->setSpacing(4);

    this->tabs = new QTabBar(this);
    this->tabs->setDocumentMode(true);
    this->tabs->setExpanding(false);
    this->tabs->setFocusPolicy(Qt::NoFocus);
    layout->addWidget(this->tabs);

    auto *toolbar = new QHBoxLayout();
    toolbar->setSpacing(4);

    this->back_button = new QToolButton(this);
    this->back_button->setArrowType(Qt::LeftArrow);
    this->back_button->setToolTip("Back (Alt+Left)");
    this->forward_button = new QToolButton(this);
    this->forward_button->setArrowType(Qt::RightArrow);
    this->forward_button->setToolTip("Forward (Alt+Right)");
    toolbar->addWidget(this->back_button);
    toolbar->addWidget(this->forward_button);

    this->go_to_edit = new QLineEdit(this);
    this->go_to_edit->setPlaceholderText("Go to address (Ctrl+G)");
    this->go_to_edit->setToolTip("0x02024284, 2024284 (hexadecimal), EWRAM:24284 or EWRAM+24284");
    this->go_to_edit->setMinimumWidth(170);
    toolbar->addWidget(this->go_to_edit, 1);

    auto add_combo = [this, toolbar](const char *label, QStringList items) {
        auto *text = new QLabel(label, this);
        toolbar->addWidget(text);
        auto *combo = new QComboBox(this);
        combo->addItems(items);
        combo->setFocusPolicy(Qt::NoFocus);
        toolbar->addWidget(combo);
        return combo;
    };
    this->row_combo = add_combo("Row", { "8", "16", "32" });
    this->row_combo->setCurrentIndex(1);
    this->group_combo = add_combo("Group", { "1", "2", "4" });
    this->endian_combo = add_combo("", { "LE", "BE" });
    this->endian_combo->setToolTip("Byte order of grouped values");
    this->table_combo = add_combo("Chars", this->controller->table_names());
    this->table_combo->setToolTip("Character table for the text column (put .tbl files in the tables folder)");
    this->rate_combo = add_combo("Refresh", { "15 Hz", "30 Hz", "60 Hz" });
    this->rate_combo->setToolTip("How often the RAM tools refresh while the game runs (shared by all tool windows)");
    int rate = this->controller->refresh_rate();
    this->rate_combo->setCurrentIndex(rate <= 15 ? 0 : rate <= 30 ? 1 : 2);
    layout->addLayout(toolbar);

    auto *splitter = new QSplitter(Qt::Horizontal, this);

    this->stack = new QStackedWidget(splitter);
    this->view = new HexViewWidget(this->stack);
    this->no_game = new QLabel("No game is loaded.", this->stack);
    this->no_game->setAlignment(Qt::AlignCenter);
    this->stack->addWidget(this->view);
    this->stack->addWidget(this->no_game);
    splitter->addWidget(this->stack);

    this->inspector = new QTableWidget(INSPECTOR_TEXT_ROW + 1, 3, splitter);
    this->inspector->setHorizontalHeaderLabels({ "Type", "Little-endian", "Big-endian" });
    this->inspector->verticalHeader()->hide();
    this->inspector->setEditTriggers(QAbstractItemView::NoEditTriggers);
    this->inspector->setSelectionMode(QAbstractItemView::NoSelection);
    this->inspector->setFocusPolicy(Qt::NoFocus);
    this->inspector->horizontalHeader()->setSectionResizeMode(0, QHeaderView::ResizeToContents);
    this->inspector->horizontalHeader()->setSectionResizeMode(1, QHeaderView::Stretch);
    this->inspector->horizontalHeader()->setSectionResizeMode(2, QHeaderView::Stretch);
    for(int row = 0; row <= INSPECTOR_TEXT_ROW; row++) {
        const char *label = row < INSPECTOR_POINTER_ROW ? INSPECTOR_ROWS[row].label : row == INSPECTOR_POINTER_ROW ? "pointer" : "text";
        auto *item = new QTableWidgetItem(label);
        this->inspector->setItem(row, 0, item);
        this->inspector->setItem(row, 1, new QTableWidgetItem());
        this->inspector->setItem(row, 2, new QTableWidgetItem());
    }
    this->inspector->setSpan(INSPECTOR_TEXT_ROW, 1, 1, 2);
    this->inspector->item(INSPECTOR_POINTER_ROW, 0)->setToolTip("Double-click a pointer that lands in a region to go there");
    splitter->addWidget(this->inspector);
    splitter->setStretchFactor(0, 3);
    splitter->setStretchFactor(1, 1);
    layout->addWidget(splitter, 1);

    this->status = new QLabel(this);
    this->status->setTextInteractionFlags(Qt::TextSelectableByMouse);
    layout->addWidget(this->status);

    connect(this->tabs, SIGNAL(currentChanged(int)), this, SLOT(on_tab_changed(int)));
    connect(this->go_to_edit, SIGNAL(returnPressed()), this, SLOT(on_go_to()));
    connect(this->back_button, SIGNAL(clicked()), this, SLOT(on_back()));
    connect(this->forward_button, SIGNAL(clicked()), this, SLOT(on_forward()));
    connect(this->row_combo, SIGNAL(currentIndexChanged(int)), this, SLOT(on_layout_changed()));
    connect(this->group_combo, SIGNAL(currentIndexChanged(int)), this, SLOT(on_layout_changed()));
    connect(this->endian_combo, SIGNAL(currentIndexChanged(int)), this, SLOT(on_layout_changed()));
    connect(this->table_combo, SIGNAL(currentIndexChanged(int)), this, SLOT(on_table_changed(int)));
    connect(this->rate_combo, SIGNAL(currentIndexChanged(int)), this, SLOT(on_refresh_rate_changed(int)));
    connect(this->view, &HexViewWidget::window_changed, this, &HexViewerWindow::on_window_changed);
    connect(this->view, &HexViewWidget::cursor_changed, this, &HexViewerWindow::on_cursor_changed);
    connect(this->view, &HexViewWidget::context_menu_requested, this, &HexViewerWindow::on_context_menu);
    connect(this->inspector, &QTableWidget::cellDoubleClicked, this, &HexViewerWindow::on_inspector_activated);
    connect(this->controller, &MemoryToolsController::regions_changed, this, &HexViewerWindow::on_regions_changed);
    connect(this->controller, &MemoryToolsController::tables_changed, this, &HexViewerWindow::on_tables_changed);
    connect(this->controller, &MemoryToolsController::refresh, this, &HexViewerWindow::on_refresh);

    // Region switching: Ctrl+1..9 and Ctrl+PgUp/PgDn; navigation: Ctrl+G, Alt+Left/Right.
    for(int i = 0; i < 9; i++) {
        auto *shortcut = new QShortcut(QKeyCombination(Qt::ControlModifier, static_cast<Qt::Key>(Qt::Key_1 + i)), this);
        connect(shortcut, &QShortcut::activated, this, [this, i]() {
            if(i < this->tabs->count()) {
                this->tabs->setCurrentIndex(i);
            }
        });
    }
    auto *previous_tab = new QShortcut(QKeyCombination(Qt::ControlModifier, Qt::Key_PageUp), this);
    connect(previous_tab, &QShortcut::activated, this, [this]() {
        if(this->tabs->count() > 0) {
            this->tabs->setCurrentIndex((this->tabs->currentIndex() + this->tabs->count() - 1) % this->tabs->count());
        }
    });
    auto *next_tab = new QShortcut(QKeyCombination(Qt::ControlModifier, Qt::Key_PageDown), this);
    connect(next_tab, &QShortcut::activated, this, [this]() {
        if(this->tabs->count() > 0) {
            this->tabs->setCurrentIndex((this->tabs->currentIndex() + 1) % this->tabs->count());
        }
    });
    auto *go_to_shortcut = new QShortcut(QKeyCombination(Qt::ControlModifier, Qt::Key_G), this);
    connect(go_to_shortcut, &QShortcut::activated, this, [this]() {
        this->go_to_edit->setFocus(Qt::ShortcutFocusReason);
        this->go_to_edit->selectAll();
    });
    auto *back_shortcut = new QShortcut(QKeyCombination(Qt::AltModifier, Qt::Key_Left), this);
    connect(back_shortcut, &QShortcut::activated, this, &HexViewerWindow::on_back);
    auto *forward_shortcut = new QShortcut(QKeyCombination(Qt::AltModifier, Qt::Key_Right), this);
    connect(forward_shortcut, &QShortcut::activated, this, &HexViewerWindow::on_forward);

    this->view->set_glyphs(this->controller->glyphs(0));
    this->resize(900, 560);
    this->on_regions_changed();
}

std::size_t HexViewerWindow::table_index() const {
    return static_cast<std::size_t>(std::max(0, this->table_combo->currentIndex()));
}

void HexViewerWindow::update_title() {
    QString title = this->viewer_slot == 0 ? QString("RAM viewer") : QString("RAM viewer %1").arg(this->viewer_slot + 1);
    int index = this->tabs->currentIndex();
    auto &regions = this->controller->regions();
    if(index >= 0 && index < static_cast<int>(regions.size())) {
        auto &region = regions[index];
        title += QString(" — %1 (%2)").arg(region.name, this->controller->format_address(region.base, false));
    }
    this->setWindowTitle(title);
}

void HexViewerWindow::send_window(bool enabled) {
    auto [address, length] = this->view->visible_window();
    supershuckie_frontend_memory_set_viewer_window(this->controller->frontend(), this->viewer_slot, enabled && this->view->has_region() && this->isVisible(), address, length);
}

void HexViewerWindow::on_regions_changed() {
    auto &regions = this->controller->regions();

    // Stay on the same region (by short name) if the new game has it.
    QString wanted = this->pending_position ? this->pending_position->first : this->current_region;
    this->remember_position();

    this->updating_tabs = true;
    while(this->tabs->count() > 0) {
        this->tabs->removeTab(0);
    }
    int select = 0;
    for(std::size_t i = 0; i < regions.size(); i++) {
        auto &region = regions[i];
        int tab = this->tabs->addTab(region.short_name);
        QString tooltip = QString("%1\n%2 – %3 (%4 bytes)%5")
            .arg(region.name)
            .arg(this->controller->format_address(region.base, false))
            .arg(this->controller->format_address(region.base + region.length - 1, false))
            .arg(region.length)
            .arg(region.writable ? "" : "\nread-only");
        if(i < 9) {
            tooltip += QString("\nCtrl+%1").arg(i + 1);
        }
        this->tabs->setTabToolTip(tab, tooltip);
        if(region.short_name == wanted) {
            select = static_cast<int>(i);
        }
    }
    this->updating_tabs = false;

    if(regions.empty()) {
        this->current_region.clear();
        this->view->clear_region();
        this->stack->setCurrentWidget(this->no_game);
        this->send_window(false);
        this->update_title();
        this->update_status();
        this->update_inspector();
        return;
    }

    this->stack->setCurrentWidget(this->view);
    this->tabs->setCurrentIndex(select);
    this->select_region(select);

    if(this->pending_position && this->pending_position->first == this->current_region) {
        this->view->go_to(this->pending_position->second);
    }
    this->pending_position.reset();
}

void HexViewerWindow::on_tables_changed() {
    this->updating_tabs = true;
    QString current = this->table_combo->currentText();
    this->table_combo->clear();
    this->table_combo->addItems(this->controller->table_names());
    int index = this->table_combo->findText(current);
    this->table_combo->setCurrentIndex(index >= 0 ? index : 0);
    this->updating_tabs = false;
    this->view->set_glyphs(this->controller->glyphs(this->table_index()));
    this->update_inspector();
}

void HexViewerWindow::remember_position() {
    if(!this->current_region.isEmpty() && this->view->has_region()) {
        this->positions[this->current_region] = Position { this->view->top_row(), this->view->cursor_address() };
    }
}

void HexViewerWindow::select_region(int index) {
    auto &regions = this->controller->regions();
    if(index < 0 || index >= static_cast<int>(regions.size())) {
        return;
    }
    auto &region = regions[index];
    bool same = region.short_name == this->current_region && this->view->has_region() && this->view->region_base() == region.base && this->view->region_size() == region.length;
    if(!same) {
        this->remember_position();
        this->current_region = region.short_name;
        this->view->set_region(region.base, region.length, region.writable, this->controller->address_digits());
        auto found = this->positions.find(region.short_name);
        if(found != this->positions.end() && region.contains(found->second.cursor)) {
            this->view->go_to(found->second.cursor);
            this->view->set_top_row(found->second.top_row);
        }
        this->sample_generation = 0;
    }
    this->update_title();
    this->update_status();
    this->send_window(true);
}

void HexViewerWindow::on_tab_changed(int index) {
    if(this->updating_tabs) {
        return;
    }
    this->select_region(index);
    this->push_history(this->view->cursor_address());
}

void HexViewerWindow::push_history(std::uint32_t address) {
    if(!this->history.empty() && this->history[this->history_index] == address) {
        return;
    }
    if(!this->history.empty()) {
        this->history.resize(this->history_index + 1);
    }
    this->history.push_back(address);
    if(this->history.size() > 100) {
        this->history.erase(this->history.begin());
    }
    this->history_index = this->history.size() - 1;
}

bool HexViewerWindow::go_to(std::uint32_t address, std::uint32_t length, bool record_history) {
    int index = this->controller->region_index_of(address);
    if(index < 0) {
        return false;
    }
    if(record_history) {
        // Remember where we were, then where we went.
        if(this->view->has_region()) {
            this->push_history(this->view->cursor_address());
        }
    }
    if(this->tabs->currentIndex() != index) {
        this->updating_tabs = true;
        this->tabs->setCurrentIndex(index);
        this->updating_tabs = false;
    }
    this->select_region(index);
    this->view->go_to(address, length);
    if(record_history) {
        this->push_history(address);
    }
    this->view->setFocus(Qt::OtherFocusReason);
    return true;
}

void HexViewerWindow::on_go_to() {
    QString error;
    auto address = this->controller->parse_address(this->go_to_edit->text(), &error);
    if(!address) {
        this->status->setText(QString("Can't go there: %1").arg(error));
        return;
    }
    if(!this->go_to(*address)) {
        this->status->setText(QString("%1 is not in any memory region").arg(this->controller->format_address(*address, false)));
    }
}

void HexViewerWindow::on_back() {
    if(this->history.empty() || this->history_index == 0) {
        return;
    }
    // Save the present position if we are not on a history entry.
    if(this->history[this->history_index] != this->view->cursor_address()) {
        this->push_history(this->view->cursor_address());
    }
    this->history_index--;
    this->go_to(this->history[this->history_index], 1, false);
}

void HexViewerWindow::on_forward() {
    if(this->history_index + 1 >= this->history.size()) {
        return;
    }
    this->history_index++;
    this->go_to(this->history[this->history_index], 1, false);
}

void HexViewerWindow::on_layout_changed() {
    static const int ROWS[] = { 8, 16, 32 };
    static const int GROUPS[] = { 1, 2, 4 };
    this->view->set_group_size(1);
    this->view->set_bytes_per_row(ROWS[std::clamp(this->row_combo->currentIndex(), 0, 2)]);
    this->view->set_group_size(GROUPS[std::clamp(this->group_combo->currentIndex(), 0, 2)]);
    this->view->set_big_endian(this->endian_combo->currentIndex() == 1);
    this->send_window(true);
}

void HexViewerWindow::on_table_changed(int) {
    if(this->updating_tabs) {
        return;
    }
    this->view->set_glyphs(this->controller->glyphs(this->table_index()));
    this->update_inspector();
}

void HexViewerWindow::on_refresh_rate_changed(int index) {
    this->controller->set_refresh_rate(REFRESH_RATES[std::clamp(index, 0, 2)]);
}

void HexViewerWindow::on_window_changed(std::uint32_t, std::uint32_t) {
    this->send_window(true);
}

void HexViewerWindow::on_cursor_changed(std::uint32_t) {
    this->update_status();
    this->update_inspector();
}

void HexViewerWindow::on_refresh() {
    if(!this->isVisible() || !this->view->has_region()) {
        return;
    }
    std::uint64_t generation = this->sample_generation;
    std::uint32_t address = 0, length = 0, valid = 0;
    if(!supershuckie_frontend_memory_read_viewer(this->controller->frontend(), this->viewer_slot, &generation, &this->frame, &address, this->buffer.data(), static_cast<std::uint32_t>(this->buffer.size()), &length, &valid)) {
        return;
    }
    this->sample_generation = generation;
    this->view->set_data(address, this->buffer.data(), length, valid);
    this->update_status();
    this->update_inspector();
}

void HexViewerWindow::update_status() {
    if(!this->view->has_region()) {
        this->status->setText("");
        return;
    }
    std::uint32_t cursor = this->view->cursor_address();
    auto [start, length] = this->view->selection();
    QString text = QString("Frame %1  ·  %2 (%3)")
        .arg(this->frame)
        .arg(this->controller->format_address(cursor, true))
        .arg(this->controller->format_address(cursor, false));
    if(length > 1) {
        text += QString("  ·  %1 bytes selected from %2").arg(length).arg(this->controller->format_address(start, false));
    }
    this->status->setText(text);
}

void HexViewerWindow::update_inspector() {
    std::uint32_t cursor = this->view->cursor_address();
    std::size_t table = this->table_index();

    for(int row = 0; row < INSPECTOR_POINTER_ROW; row++) {
        auto &definition = INSPECTOR_ROWS[row];
        auto bytes = this->view->has_region() ? this->view->bytes_at(cursor, definition.size) : std::nullopt;
        std::uint32_t display = std::string(definition.label) == "binary" ? SuperShuckieMemoryDisplay__Binary : SuperShuckieMemoryDisplay__Decimal;
        if(!bytes) {
            this->inspector->item(row, 1)->setText("—");
            this->inspector->item(row, 2)->setText(definition.endianness ? "—" : "");
            continue;
        }
        this->inspector->item(row, 1)->setText(this->controller->format_value(table, definition.value_type, definition.size, false, display, bytes->data(), bytes->size()));
        this->inspector->item(row, 2)->setText(definition.endianness ? this->controller->format_value(table, definition.value_type, definition.size, true, display, bytes->data(), bytes->size()) : "");
    }

    auto pointer_bytes = this->view->has_region() ? this->view->bytes_at(cursor, 4) : std::nullopt;
    auto describe_pointer = [this](std::uint32_t pointer) {
        int region = this->controller->region_index_of(pointer);
        return region >= 0 ? this->controller->format_address(pointer, true) : this->controller->format_address(pointer, false);
    };
    if(pointer_bytes) {
        auto &b = *pointer_bytes;
        std::uint32_t le = b[0] | (b[1] << 8) | (b[2] << 16) | (static_cast<std::uint32_t>(b[3]) << 24);
        std::uint32_t be = b[3] | (b[2] << 8) | (b[1] << 16) | (static_cast<std::uint32_t>(b[0]) << 24);
        this->inspector->item(INSPECTOR_POINTER_ROW, 1)->setText(describe_pointer(le));
        this->inspector->item(INSPECTOR_POINTER_ROW, 2)->setText(describe_pointer(be));
    }
    else {
        this->inspector->item(INSPECTOR_POINTER_ROW, 1)->setText("—");
        this->inspector->item(INSPECTOR_POINTER_ROW, 2)->setText("—");
    }

    std::size_t text_bytes = INSPECTOR_TEXT_BYTES;
    std::optional<std::vector<std::uint8_t>> text;
    while(this->view->has_region() && text_bytes > 0 && !(text = this->view->bytes_at(cursor, text_bytes))) {
        text_bytes--;
    }
    if(text) {
        this->inspector->item(INSPECTOR_TEXT_ROW, 1)->setText(this->controller->format_value(table, SuperShuckieMemoryValueType__Text, static_cast<std::uint8_t>(text->size()), false, 0, text->data(), text->size()));
    }
    else {
        this->inspector->item(INSPECTOR_TEXT_ROW, 1)->setText("—");
    }
}

void HexViewerWindow::on_inspector_activated(int row, int column) {
    if(row != INSPECTOR_POINTER_ROW || column < 1) {
        return;
    }
    auto bytes = this->view->bytes_at(this->view->cursor_address(), 4);
    if(!bytes) {
        return;
    }
    auto &b = *bytes;
    std::uint32_t pointer = column == 1
        ? (b[0] | (b[1] << 8) | (b[2] << 16) | (static_cast<std::uint32_t>(b[3]) << 24))
        : (b[3] | (b[2] << 8) | (b[1] << 16) | (static_cast<std::uint32_t>(b[0]) << 24));
    if(!this->go_to(pointer)) {
        this->status->setText(QString("%1 is not in any memory region").arg(this->controller->format_address(pointer, false)));
    }
}

void HexViewerWindow::on_context_menu(QPoint global_position) {
    if(!this->view->has_region()) {
        return;
    }
    auto [start, length] = this->view->selection();
    QMenu menu(this);

    auto *copy_address = menu.addAction(QString("Copy address (%1)").arg(this->controller->format_address(start, false)));
    connect(copy_address, &QAction::triggered, this, [this, start]() {
        QApplication::clipboard()->setText(this->controller->format_address(start, false));
    });
    auto *copy_region_address = menu.addAction(QString("Copy address as %1").arg(this->controller->format_address(start, true)));
    connect(copy_region_address, &QAction::triggered, this, [this, start]() {
        QApplication::clipboard()->setText(this->controller->format_address(start, true));
    });
    auto *copy_bytes = menu.addAction(length > 1 ? QString("Copy %1 bytes").arg(length) : QString("Copy byte"));
    connect(copy_bytes, &QAction::triggered, this, [this, start, length]() {
        auto bytes = this->view->bytes_at(start, length);
        if(!bytes) {
            return;
        }
        QString text;
        for(std::size_t i = 0; i < bytes->size(); i++) {
            text += QString::asprintf(i == 0 ? "%02X" : " %02X", (*bytes)[i]);
        }
        QApplication::clipboard()->setText(text);
    });
    copy_bytes->setEnabled(this->view->bytes_at(start, length).has_value());

    menu.addSeparator();
    int group = this->view->group_size();
    bool big_endian = this->view->is_big_endian();
    std::uint32_t type = group == 4 ? SuperShuckieMemoryValueType__U32 : group == 2 ? SuperShuckieMemoryValueType__U16 : SuperShuckieMemoryValueType__U8;
    std::uint32_t cursor = this->view->cursor_address();
    auto *add_watch = menu.addAction(QString("Add watch at %1…").arg(this->controller->format_address(cursor, true)));
    connect(add_watch, &QAction::triggered, this, [this, cursor, type, group, big_endian]() {
        this->controller->add_watch(this, cursor, type, static_cast<std::uint8_t>(group), big_endian, this->controller->format_address(cursor, true));
    });

    auto value_bytes = this->view->bytes_at(this->view->cursor_address(), static_cast<std::size_t>(group));
    if(value_bytes) {
        QString value = this->controller->format_value(0, type, static_cast<std::uint8_t>(group), big_endian, SuperShuckieMemoryDisplay__Decimal, value_bytes->data(), value_bytes->size());
        auto *search = menu.addAction(QString("Search for this value (%1)").arg(value));
        connect(search, &QAction::triggered, this, [this, type, group, big_endian, value]() {
            this->controller->search_for_value(type, static_cast<std::uint8_t>(group), big_endian, value);
        });
    }

    menu.exec(global_position);
}

void HexViewerWindow::showEvent(QShowEvent *event) {
    QWidget::showEvent(event);
    this->send_window(true);
    this->controller->visibility_changed();
    this->controller->viewer_activated(this);
}

void HexViewerWindow::hideEvent(QHideEvent *event) {
    QWidget::hideEvent(event);
    supershuckie_frontend_memory_set_viewer_window(this->controller->frontend(), this->viewer_slot, false, 0, 0);
    this->controller->visibility_changed();
}

void HexViewerWindow::changeEvent(QEvent *event) {
    QWidget::changeEvent(event);
    if(event->type() == QEvent::ActivationChange && this->isActiveWindow()) {
        this->controller->viewer_activated(this);
    }
    if(event->type() == QEvent::WindowStateChange) {
        // Nothing is copied for a minimized viewer.
        bool minimized = (this->windowState() & Qt::WindowMinimized) != 0;
        if(minimized) {
            supershuckie_frontend_memory_set_viewer_window(this->controller->frontend(), this->viewer_slot, false, 0, 0);
        }
        else {
            this->send_window(true);
        }
    }
}

QString HexViewerWindow::save_state() const {
    QStringList fields;
    fields << QString::fromLatin1(this->saveGeometry().toBase64());
    fields << this->current_region;
    fields << QString::number(this->view->cursor_address(), 16);
    fields << QString::number(this->row_combo->currentIndex());
    fields << QString::number(this->group_combo->currentIndex());
    fields << QString::number(this->endian_combo->currentIndex());
    fields << this->table_combo->currentText();
    return fields.join('|');
}

void HexViewerWindow::restore_state(const QString &state) {
    auto fields = state.split('|');
    if(fields.size() < 7) {
        return;
    }
    this->restoreGeometry(QByteArray::fromBase64(fields[0].toLatin1()));
    bool ok = false;
    std::uint32_t cursor = fields[2].toUInt(&ok, 16);
    if(!fields[1].isEmpty() && ok) {
        this->pending_position = std::make_pair(fields[1], cursor);
    }
    this->row_combo->setCurrentIndex(std::clamp(fields[3].toInt(), 0, 2));
    this->group_combo->setCurrentIndex(std::clamp(fields[4].toInt(), 0, 2));
    this->endian_combo->setCurrentIndex(std::clamp(fields[5].toInt(), 0, 1));
    int table = this->table_combo->findText(fields[6]);
    if(table >= 0) {
        this->table_combo->setCurrentIndex(table);
    }
    this->on_layout_changed();
    this->on_regions_changed();
}
