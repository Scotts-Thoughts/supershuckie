#include <QApplication>
#include <QBoxLayout>
#include <QClipboard>
#include <QFile>
#include <QFileDialog>
#include <QFontDatabase>
#include <QTextStream>
#include <QHeaderView>
#include <QJsonArray>
#include <QJsonDocument>
#include <QLabel>
#include <QMenu>
#include <QMessageBox>
#include <QPlainTextEdit>
#include <QPushButton>
#include <QScrollBar>
#include <QShortcut>
#include <QSplitter>
#include <QTreeWidget>
#include <algorithm>

#include "ram_watch_window.hpp"
#include "watch_edit_dialog.hpp"
#include "memory_tools_controller.hpp"
#include "main_window.hpp"

using namespace SuperShuckie64;

RamWatchWindow::RamWatchWindow(MemoryToolsController *controller): QWidget(controller->main_window(), Qt::Window), controller(controller) {
    this->setWindowTitle("RAM watch");

    auto *layout = new QVBoxLayout(this);
    layout->setContentsMargins(8, 8, 8, 8);

    auto *buttons = new QHBoxLayout();
    auto *add = new QPushButton("Add…", this);
    this->edit_button = new QPushButton("Edit…", this);
    this->duplicate_button = new QPushButton("Duplicate", this);
    this->delete_button = new QPushButton("Delete", this);
    this->freeze_button = new QPushButton("Freeze", this);
    this->freeze_button->setToolTip("Hold the selected watches at their current values");
    this->unfreeze_button = new QPushButton("Unfreeze", this);
    auto *unfreeze_all = new QPushButton("Unfreeze all", this);
    auto *import_button = new QPushButton("Import…", this);
    auto *export_button = new QPushButton("Export…", this);
    for(auto *button : { add, this->edit_button, this->duplicate_button, this->delete_button }) {
        buttons->addWidget(button);
    }
    buttons->addSpacing(12);
    for(auto *button : { this->freeze_button, this->unfreeze_button, unfreeze_all }) {
        buttons->addWidget(button);
    }
    buttons->addStretch(1);
    buttons->addWidget(import_button);
    buttons->addWidget(export_button);
    layout->addLayout(buttons);

    this->splitter = new QSplitter(Qt::Vertical, this);

    this->tree = new QTreeWidget(this->splitter);
    this->tree->setColumnCount(ColumnCount);
    this->tree->setHeaderLabels({ "Label", "Region", "Address", "Value", "Previous", "Changed", "Frozen", "" });
    this->tree->setSelectionMode(QAbstractItemView::ExtendedSelection);
    this->tree->setContextMenuPolicy(Qt::CustomContextMenu);
    this->tree->setUniformRowHeights(true);
    this->tree->setAllColumnsShowFocus(true);
    this->tree->header()->setStretchLastSection(false);
    this->tree->header()->setSectionResizeMode(Label, QHeaderView::Stretch);
    for(int column = Region; column < ColumnCount; column++) {
        this->tree->header()->setSectionResizeMode(column, QHeaderView::ResizeToContents);
    }
    this->splitter->addWidget(this->tree);

    auto *log_panel = new QWidget(this->splitter);
    auto *log_layout = new QVBoxLayout(log_panel);
    log_layout->setContentsMargins(0, 4, 0, 0);
    auto *log_header = new QHBoxLayout();
    log_header->addWidget(new QLabel("Change log (watches set to log changes or pause)", log_panel), 1);
    auto *clear_log = new QPushButton("Clear", log_panel);
    auto *copy_log = new QPushButton("Copy", log_panel);
    auto *export_log = new QPushButton("Export CSV…", log_panel);
    log_header->addWidget(clear_log);
    log_header->addWidget(copy_log);
    log_header->addWidget(export_log);
    log_layout->addLayout(log_header);
    this->log = new QPlainTextEdit(log_panel);
    this->log->setReadOnly(true);
    this->log->setMaximumBlockCount(10000);
    this->log->setFont(QFontDatabase::systemFont(QFontDatabase::FixedFont));
    this->log->setLineWrapMode(QPlainTextEdit::NoWrap);
    log_layout->addWidget(this->log);
    this->splitter->addWidget(log_panel);
    this->splitter->setStretchFactor(0, 3);
    this->splitter->setStretchFactor(1, 1);
    layout->addWidget(this->splitter, 1);

    this->status = new QLabel(this);
    this->status->setWordWrap(true);
    layout->addWidget(this->status);

    connect(add, SIGNAL(clicked()), this, SLOT(on_add()));
    connect(this->edit_button, SIGNAL(clicked()), this, SLOT(on_edit()));
    connect(this->duplicate_button, SIGNAL(clicked()), this, SLOT(on_duplicate()));
    connect(this->delete_button, SIGNAL(clicked()), this, SLOT(on_delete()));
    connect(this->freeze_button, SIGNAL(clicked()), this, SLOT(on_freeze()));
    connect(this->unfreeze_button, SIGNAL(clicked()), this, SLOT(on_unfreeze()));
    connect(unfreeze_all, &QPushButton::clicked, this, [this]() { this->controller->unfreeze_all(); });
    connect(this->controller, &MemoryToolsController::message, this, [this](const QString &text) {
        if(this->isActiveWindow()) {
            this->status->setText(text);
        }
    });
    auto *undo = new QShortcut(QKeySequence::Undo, this);
    connect(undo, &QShortcut::activated, this, [this]() { this->controller->undo(this); });
    auto *redo = new QShortcut(QKeySequence::Redo, this);
    connect(redo, &QShortcut::activated, this, [this]() { this->controller->redo(this); });
    connect(import_button, SIGNAL(clicked()), this, SLOT(on_import()));
    connect(export_button, SIGNAL(clicked()), this, SLOT(on_export()));
    connect(clear_log, SIGNAL(clicked()), this, SLOT(on_clear_log()));
    connect(copy_log, &QPushButton::clicked, this, [this]() {
        QApplication::clipboard()->setText(this->log->toPlainText());
    });
    connect(export_log, SIGNAL(clicked()), this, SLOT(on_export_log()));
    connect(this->tree, &QTreeWidget::itemDoubleClicked, this, &RamWatchWindow::on_item_double_clicked);
    connect(this->tree, &QTreeWidget::customContextMenuRequested, this, &RamWatchWindow::on_context_menu);
    connect(this->tree, &QTreeWidget::itemSelectionChanged, this, [this]() {
        bool any = !this->selected_ids().empty();
        this->edit_button->setEnabled(this->selected_ids().size() == 1);
        this->duplicate_button->setEnabled(any);
        this->delete_button->setEnabled(any);
        this->freeze_button->setEnabled(any);
        this->unfreeze_button->setEnabled(any);
    });
    connect(this->tree, &QTreeWidget::itemExpanded, this, [this](QTreeWidgetItem *item) {
        this->collapsed_groups.erase(item->text(Label));
        this->on_visible_changed();
    });
    connect(this->tree, &QTreeWidget::itemCollapsed, this, [this](QTreeWidgetItem *item) {
        this->collapsed_groups.insert(item->text(Label));
        this->on_visible_changed();
    });
    connect(this->tree->verticalScrollBar(), &QScrollBar::valueChanged, this, &RamWatchWindow::on_visible_changed);
    connect(this->tree->verticalScrollBar(), &QScrollBar::rangeChanged, this, &RamWatchWindow::on_visible_changed);
    connect(this->controller, &MemoryToolsController::refresh, this, &RamWatchWindow::on_refresh);
    connect(this->controller, &MemoryToolsController::regions_changed, this, [this]() {
        this->watch_generation = 0;
        this->on_refresh();
    });

    this->edit_button->setEnabled(false);
    this->duplicate_button->setEnabled(false);
    this->delete_button->setEnabled(false);
    this->freeze_button->setEnabled(false);
    this->unfreeze_button->setEnabled(false);
    this->resize(760, 520);
}

QString RamWatchWindow::address_text(const QJsonObject &watch) const {
    QByteArray json = QJsonDocument(watch["address"].toObject()).toJson(QJsonDocument::Compact);
    char text[256];
    supershuckie_frontend_watch_format_address(this->controller->frontend(), json.constData(), text, sizeof(text));
    return QString::fromUtf8(text);
}

void RamWatchWindow::rebuild() {
    auto selected = this->selected_ids();

    char *list = supershuckie_frontend_watch_list_json(this->controller->frontend());
    auto array = QJsonDocument::fromJson(QByteArray(list)).array();
    supershuckie_string_free(list);

    this->tree->clear();
    this->items.clear();
    this->watches.clear();

    std::map<QString, QTreeWidgetItem *> groups;
    for(auto value : array) {
        auto watch = value.toObject();
        auto id = static_cast<std::uint32_t>(watch["id"].toInteger());
        this->watches[id] = watch;

        QTreeWidgetItem *parent = nullptr;
        QString group = watch["group"].toString();
        if(!group.isEmpty()) {
            auto found = groups.find(group);
            if(found == groups.end()) {
                auto *group_item = new QTreeWidgetItem(this->tree);
                group_item->setText(Label, group);
                QFont font = group_item->font(Label);
                font.setBold(true);
                group_item->setFont(Label, font);
                group_item->setFirstColumnSpanned(true);
                group_item->setData(Label, Qt::UserRole, 0);
                found = groups.emplace(group, group_item).first;
            }
            parent = found->second;
        }

        auto *item = parent != nullptr ? new QTreeWidgetItem(parent) : new QTreeWidgetItem(this->tree);
        item->setData(Label, Qt::UserRole, static_cast<qint64>(id));
        item->setText(Label, watch["label"].toString());
        QString address = this->address_text(watch);
        item->setText(Address, address);
        auto base = watch["address"].toObject()["base"].toString();
        bool ok = false;
        std::uint32_t base_address = base.mid(2).toUInt(&ok, 16);
        int region = this->controller->region_index_of(base_address);
        item->setText(Region, region >= 0 ? this->controller->regions()[region].short_name : "");

        QString notes = watch["notes"].toString();
        auto format = watch["format"].toObject();
        item->setToolTip(Label, QString("%1 %2%3%4").arg(format["type"].toString()).arg(format["size"].toInt() > 1 ? QString("×%1").arg(format["size"].toInt()) : "").arg(format["big_endian"].toBool() ? " big-endian" : "").arg(notes.isEmpty() ? "" : "\n" + notes));

        QStringList flags;
        if(watch["trace"].toBool()) {
            flags << "log";
        }
        if(watch.contains("pause_when")) {
            flags << "pause";
        }
        item->setText(Flags, flags.join(", "));
        auto freeze = watch["freeze"].toObject();
        item->setText(Frozen, freeze["active"].toBool() ? "frozen" : "");
        item->setTextAlignment(Value, Qt::AlignRight | Qt::AlignVCenter);
        item->setTextAlignment(Previous, Qt::AlignRight | Qt::AlignVCenter);
        item->setTextAlignment(Changed, Qt::AlignRight | Qt::AlignVCenter);
        this->items[id] = item;
    }

    for(auto &[name, item] : groups) {
        item->setExpanded(this->collapsed_groups.find(name) == this->collapsed_groups.end());
    }
    for(auto id : selected) {
        auto found = this->items.find(id);
        if(found != this->items.end()) {
            found->second->setSelected(true);
        }
    }
    this->visible_ids.clear();
    this->on_visible_changed();
}

std::vector<std::uint32_t> RamWatchWindow::selected_ids() const {
    std::vector<std::uint32_t> ids;
    for(auto *item : this->tree->selectedItems()) {
        auto id = static_cast<std::uint32_t>(item->data(Label, Qt::UserRole).toLongLong());
        if(id != 0) {
            ids.push_back(id);
        }
    }
    return ids;
}

void RamWatchWindow::on_visible_changed() {
    std::vector<std::uint32_t> visible;
    if(this->isVisible()) {
        QRect viewport = this->tree->viewport()->rect();
        for(auto &[id, item] : this->items) {
            QRect rect = this->tree->visualItemRect(item);
            if(!rect.isEmpty() && rect.intersects(viewport)) {
                visible.push_back(id);
            }
        }
    }
    if(visible != this->visible_ids) {
        this->visible_ids = visible;
        supershuckie_frontend_watch_set_visible(this->controller->frontend(), visible.data(), visible.size());
    }
}

void RamWatchWindow::on_refresh() {
    auto frontend = this->controller->frontend();
    auto generation = supershuckie_frontend_watch_generation(frontend);
    if(generation != this->watch_generation) {
        this->watch_generation = generation;
        this->rebuild();
    }

    // Log lines (drained even while hidden so traces are not lost).
    SuperShuckieWatchLogEntry entries[256];
    std::uint64_t dropped = 0;
    std::uint32_t paused_watch = 0;
    std::size_t count;
    do {
        count = supershuckie_frontend_watch_drain_log(frontend, entries, 256, &dropped);
        if(dropped > 0) {
            this->log->appendPlainText(QString("… %1 log lines were dropped …").arg(dropped));
        }
        for(std::size_t i = 0; i < count; i++) {
            auto &entry = entries[i];
            QString text = QString::fromUtf8(entry.text);
            switch(entry.kind) {
                case SuperShuckieWatchLogKind__Discontinuity:
                    this->log->appendPlainText(QString("— frame %1: %2 —").arg(entry.frame).arg(text));
                    break;
                case SuperShuckieWatchLogKind__Paused:
                    paused_watch = entry.watch_id;
                    [[fallthrough]];
                default:
                    this->log->appendPlainText(QString("frame %1  %2").arg(entry.frame, 8).arg(text));
                    break;
            }
        }
    } while(count == 256);

    if(paused_watch != 0) {
        this->select_watch(paused_watch);
    }

    if(!this->isVisible()) {
        return;
    }

    char problems[1024];
    if(supershuckie_frontend_watch_problems(frontend, problems, sizeof(problems))) {
        this->status->setText(QString::fromUtf8(problems));
    }
    else if(this->controller->regions().empty()) {
        this->status->setText("No game is loaded.");
    }
    else if(this->status->text() == "No game is loaded.") {
        this->status->clear();
    }

    std::vector<SuperShuckieWatchValue> values(this->visible_ids.size() + 1);
    std::size_t value_count = supershuckie_frontend_watch_read_values(frontend, values.data(), values.size());
    for(std::size_t i = 0; i < value_count; i++) {
        auto &value = values[i];
        auto found = this->items.find(value.id);
        if(found == this->items.end()) {
            continue;
        }
        auto *item = found->second;
        QString text = QString::fromUtf8(value.text);
        if(item->text(Value) != text) {
            item->setText(Value, text);
        }
        QString previous = QString::fromUtf8(value.previous_text);
        if(item->text(Previous) != previous) {
            item->setText(Previous, previous);
        }
        QString changed = value.frames_since_change == UINT64_MAX ? QString() : QString("%1 frames ago").arg(value.frames_since_change);
        if(item->text(Changed) != changed) {
            item->setText(Changed, changed);
        }
        auto watch = this->watches.find(value.id);
        if(watch != this->watches.end() && watch->second["freeze"].toObject()["active"].toBool()) {
            std::uint32_t restores = 0;
            bool resolved = true;
            QString frozen;
            char reason[256];
            if(!supershuckie_frontend_memory_can_write(frontend, reason, sizeof(reason)) && supershuckie_frontend_get_replay_state(frontend) == SuperShuckieReplayState__Playback) {
                frozen = "suspended";
            }
            else if(supershuckie_frontend_watch_freeze_status(frontend, value.id, &restores, &resolved)) {
                frozen = !resolved ? "unresolved" : restores == 0 ? "frozen" : QString("frozen, restored ×%1").arg(restores);
            }
            else {
                frozen = "frozen";
            }
            if(item->text(Frozen) != frozen) {
                item->setText(Frozen, frozen);
            }
        }
        if(watch != this->watches.end() && watch->second["address"].toObject().contains("offsets")) {
            QString address = this->address_text(watch->second);
            if(value.resolved) {
                address += QString(" → %1").arg(this->controller->format_address(value.resolved_address, false));
            }
            else {
                address += " → ?";
            }
            if(item->text(Address) != address) {
                item->setText(Address, address);
            }
        }
    }
}

void RamWatchWindow::select_watch(std::uint32_t id) {
    this->on_refresh();
    auto found = this->items.find(id);
    if(found == this->items.end()) {
        return;
    }
    if(!this->isVisible()) {
        this->show();
    }
    this->raise();
    this->tree->clearSelection();
    found->second->setSelected(true);
    this->tree->scrollToItem(found->second);
}

void RamWatchWindow::on_add() {
    if(this->controller->regions().empty()) {
        this->status->setText("Load a game first.");
        return;
    }
    auto &region = this->controller->regions()[0];
    auto watch = WatchEditDialog::new_watch(region.base, SuperShuckieMemoryValueType__U8, 1, region.big_endian, "");
    auto id = WatchEditDialog::edit(this->controller, this, watch);
    if(id) {
        this->select_watch(*id);
    }
}

void RamWatchWindow::on_edit() {
    auto ids = this->selected_ids();
    if(ids.size() != 1) {
        return;
    }
    auto found = this->watches.find(ids[0]);
    if(found == this->watches.end()) {
        return;
    }
    WatchEditDialog::edit(this->controller, this, found->second);
}

void RamWatchWindow::on_duplicate() {
    for(auto id : this->selected_ids()) {
        auto found = this->watches.find(id);
        if(found == this->watches.end()) {
            continue;
        }
        auto copy = found->second;
        copy["id"] = 0;
        copy["label"] = copy["label"].toString() + " (copy)";
        // A copy never starts out frozen or pausing.
        copy.remove("pause_when");
        if(copy.contains("freeze")) {
            auto freeze = copy["freeze"].toObject();
            freeze["active"] = false;
            copy["freeze"] = freeze;
        }
        char error[256];
        this->controller->upsert_watch(QJsonDocument(copy).toJson(QJsonDocument::Compact), error, sizeof(error));
    }
}

void RamWatchWindow::on_delete() {
    auto ids = this->selected_ids();
    if(ids.empty()) {
        return;
    }
    if(ids.size() > 1 && QMessageBox::question(this, "Delete watches", QString("Delete %1 watches?").arg(ids.size())) != QMessageBox::Yes) {
        return;
    }
    for(auto id : ids) {
        supershuckie_frontend_watch_remove(this->controller->frontend(), id);
    }
}

void RamWatchWindow::on_import() {
    auto path = QFileDialog::getOpenFileName(this, "Import watches", QString(), "Watch lists (*.json);;All files (*)");
    if(path.isEmpty()) {
        return;
    }
    auto answer = QMessageBox::question(this, "Import watches", "Replace the current watches with the imported ones?\n\nChoose No to add them to the list instead.", QMessageBox::Yes | QMessageBox::No | QMessageBox::Cancel);
    if(answer == QMessageBox::Cancel) {
        return;
    }
    char error[1024];
    if(!supershuckie_frontend_watch_import(this->controller->frontend(), path.toUtf8().constData(), answer == QMessageBox::Yes, error, sizeof(error))) {
        this->status->setText(QString::fromUtf8(error));
    }
}

void RamWatchWindow::on_export() {
    auto path = QFileDialog::getSaveFileName(this, "Export watches", "watches.json", "Watch lists (*.json)");
    if(path.isEmpty()) {
        return;
    }
    char error[1024];
    if(!supershuckie_frontend_watch_export(this->controller->frontend(), path.toUtf8().constData(), error, sizeof(error))) {
        this->status->setText(QString::fromUtf8(error));
    }
}

void RamWatchWindow::on_freeze() {
    std::vector<SuperShuckieWatchValue> values(this->visible_ids.size() + 1);
    std::size_t count = supershuckie_frontend_watch_read_values(this->controller->frontend(), values.data(), values.size());
    for(auto id : this->selected_ids()) {
        bool found = false;
        for(std::size_t i = 0; i < count; i++) {
            if(values[i].id == id && values[i].ok) {
                found = true;
                if(!this->controller->set_frozen(this, id, true, QByteArray(reinterpret_cast<const char *>(values[i].value), values[i].length))) {
                    return;
                }
            }
        }
        if(!found) {
            // Not on screen or not readable: freeze at the value it was last frozen at, if any.
            if(!this->controller->set_frozen(this, id, true)) {
                return;
            }
        }
    }
}

void RamWatchWindow::on_unfreeze() {
    for(auto id : this->selected_ids()) {
        this->controller->set_frozen(this, id, false);
    }
}

void RamWatchWindow::on_clear_log() {
    this->log->clear();
}

void RamWatchWindow::on_export_log() {
    auto path = QFileDialog::getSaveFileName(this, "Export change log", "change-log.csv", "CSV files (*.csv)");
    if(path.isEmpty()) {
        return;
    }
    QFile file(path);
    if(!file.open(QIODevice::WriteOnly | QIODevice::Text)) {
        this->status->setText(QString("Can't write %1").arg(path));
        return;
    }
    QTextStream out(&file);
    out << "frame,entry\n";
    for(auto &line : this->log->toPlainText().split('\n', Qt::SkipEmptyParts)) {
        QString trimmed = line.trimmed();
        QString frame;
        QString text = trimmed;
        if(trimmed.startsWith("frame ")) {
            auto rest = trimmed.mid(6).trimmed();
            int space = rest.indexOf(' ');
            frame = rest.left(space);
            text = rest.mid(space + 1).trimmed();
        }
        text.replace('"', "\"\"");
        out << frame << ",\"" << text << "\"\n";
    }
}

void RamWatchWindow::on_item_double_clicked(QTreeWidgetItem *item, int column) {
    auto id = static_cast<std::uint32_t>(item->data(Label, Qt::UserRole).toLongLong());
    if(id == 0) {
        return;
    }
    if(this->controller->edit_watch_value_inline(this, id, column == Value)) {
        return;
    }
    auto found = this->watches.find(id);
    if(found != this->watches.end()) {
        WatchEditDialog::edit(this->controller, this, found->second);
    }
}

void RamWatchWindow::on_context_menu(const QPoint &position) {
    auto *item = this->tree->itemAt(position);
    if(item == nullptr) {
        return;
    }
    auto id = static_cast<std::uint32_t>(item->data(Label, Qt::UserRole).toLongLong());
    if(id == 0) {
        return;
    }
    if(!item->isSelected()) {
        this->tree->clearSelection();
        item->setSelected(true);
    }
    auto watch = this->watches[id];

    QMenu menu(this);
    auto *show = menu.addAction("Show in RAM viewer");
    connect(show, &QAction::triggered, this, [this, id, watch]() {
        std::vector<SuperShuckieWatchValue> values(this->visible_ids.size() + 1);
        std::size_t count = supershuckie_frontend_watch_read_values(this->controller->frontend(), values.data(), values.size());
        auto size = static_cast<std::uint32_t>(watch["format"].toObject()["size"].toInt());
        for(std::size_t i = 0; i < count; i++) {
            if(values[i].id == id && values[i].resolved) {
                this->controller->show_in_viewer(values[i].resolved_address, size);
                return;
            }
        }
        bool ok = false;
        auto base = watch["address"].toObject()["base"].toString().mid(2).toUInt(&ok, 16);
        if(ok) {
            this->controller->show_in_viewer(base, size);
        }
    });
    menu.addAction("Edit…", this, &RamWatchWindow::on_edit);
    menu.addAction("Duplicate", this, &RamWatchWindow::on_duplicate);
    menu.addAction("Delete", this, &RamWatchWindow::on_delete);
    menu.addSeparator();
    auto *copy = menu.addAction("Copy address");
    connect(copy, &QAction::triggered, this, [this, watch]() {
        QApplication::clipboard()->setText(this->address_text(watch));
    });
    this->controller->add_watch_actions(&menu, this, this->selected_ids());
    menu.exec(this->tree->viewport()->mapToGlobal(position));
}

void RamWatchWindow::showEvent(QShowEvent *event) {
    QWidget::showEvent(event);
    this->controller->visibility_changed();
    this->watch_generation = 0;
    this->on_refresh();
}

void RamWatchWindow::hideEvent(QHideEvent *event) {
    QWidget::hideEvent(event);
    this->visible_ids.clear();
    supershuckie_frontend_watch_set_visible(this->controller->frontend(), nullptr, 0);
    this->controller->visibility_changed();
}

QString RamWatchWindow::save_state() const {
    return QString("%1|%2").arg(QString::fromLatin1(this->saveGeometry().toBase64()), QString::fromLatin1(this->splitter->saveState().toBase64()));
}

void RamWatchWindow::restore_state(const QString &state) {
    auto fields = state.split('|');
    if(fields.size() >= 1) {
        this->restoreGeometry(QByteArray::fromBase64(fields[0].toLatin1()));
    }
    if(fields.size() >= 2) {
        this->splitter->restoreState(QByteArray::fromBase64(fields[1].toLatin1()));
    }
}
