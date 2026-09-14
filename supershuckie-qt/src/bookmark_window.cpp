#include <QBoxLayout>
#include <QCheckBox>
#include <QColorDialog>
#include <QComboBox>
#include <QDialogButtonBox>
#include <QFormLayout>
#include <QHeaderView>
#include <QInputDialog>
#include <QJsonDocument>
#include <QLabel>
#include <QLineEdit>
#include <QListWidget>
#include <QMenu>
#include <QMessageBox>
#include <QPainter>
#include <QPixmap>
#include <QPushButton>
#include <QScrollBar>
#include <QShortcut>
#include <QSpinBox>
#include <QTreeWidget>
#include <limits>
#include <set>

#include <supershuckie/supershuckie.h>

#include "bookmark_window.hpp"
#include "main_window.hpp"

using namespace SuperShuckie64;

namespace {

/** Big enough for a bookmark as JSON or any message. */
constexpr std::size_t RESULT_BUFFER = 8192;

QString json_string(const QJsonObject &object) {
    return QString::fromUtf8(QJsonDocument(object).toJson(QJsonDocument::Compact));
}

std::uint64_t json_id(const QJsonValue &value) {
    return static_cast<std::uint64_t>(value.toDouble());
}

/** Text readable on a row tinted with `color`, in the current palette. */
QColor tint_for(const QColor &color) {
    QColor tint = color;
    tint.setAlphaF(0.22f);
    return tint;
}

}

QIcon SuperShuckie64::bookmark_swatch(const QColor &color) {
    QPixmap pixmap(12, 12);
    pixmap.fill(Qt::transparent);
    QPainter painter(&pixmap);
    painter.setRenderHint(QPainter::Antialiasing, true);
    painter.setBrush(color);
    painter.setPen(color.darker(140));
    painter.drawRoundedRect(QRectF(0.5, 0.5, 11, 11), 2, 2);
    return QIcon(pixmap);
}

QString SuperShuckie64::format_bookmark_time(std::uint64_t millis) {
    std::uint64_t seconds = millis / 1000;
    std::uint64_t minutes = seconds / 60;
    std::uint64_t hours = minutes / 60;
    char text[64];
    if(hours > 0) {
        std::snprintf(text, sizeof(text), "%llu:%02llu:%02llu.%03llu", static_cast<unsigned long long>(hours), static_cast<unsigned long long>(minutes % 60), static_cast<unsigned long long>(seconds % 60), static_cast<unsigned long long>(millis % 1000));
    }
    else {
        std::snprintf(text, sizeof(text), "%llu:%02llu.%03llu", static_cast<unsigned long long>(minutes), static_cast<unsigned long long>(seconds % 60), static_cast<unsigned long long>(millis % 1000));
    }
    return QString::fromLatin1(text);
}

std::optional<QJsonObject> BookmarkWindow::run_operation(MainWindow *main_window, QWidget *parent, const BookmarkOperation &operation) {
    std::vector<char> buffer(RESULT_BUFFER, 0);
    auto result = operation(false, buffer.data(), buffer.size());

    if(result == SuperShuckieBookmarkResult::SuperShuckieBookmarkResult__NeedsUpgrade) {
        QMessageBox box(parent);
        box.setWindowTitle("Upgrade replay?");
        box.setIcon(QMessageBox::Question);
        box.setText(QString::fromUtf8(buffer.data()));
        box.setInformativeText("Bookmarks are saved inside the replay file.");
        auto *upgrade = box.addButton("Upgrade and save", QMessageBox::AcceptRole);
        box.addButton(QMessageBox::Cancel);
        auto *dont_ask = new QCheckBox("Don't ask again", &box);
        box.setCheckBox(dont_ask);
        main_window->stop_timer();
        box.exec();
        main_window->start_timer();
        if(box.clickedButton() != upgrade) {
            return std::nullopt;
        }
        if(dont_ask->isChecked()) {
            supershuckie_frontend_set_bookmark_confirm_upgrade(main_window->frontend, false);
        }
        std::fill(buffer.begin(), buffer.end(), 0);
        result = operation(true, buffer.data(), buffer.size());
    }

    if(result != SuperShuckieBookmarkResult::SuperShuckieBookmarkResult__Ok) {
        main_window->stop_timer();
        QMessageBox::warning(parent, "Bookmarks", QString::fromUtf8(buffer.data()));
        main_window->start_timer();
        return std::nullopt;
    }

    auto document = QJsonDocument::fromJson(QByteArray(buffer.data()));
    return document.isObject() ? document.object() : QJsonObject();
}

QJsonObject BookmarkWindow::read_state(MainWindow *main_window) {
    char *json = supershuckie_frontend_bookmarks_json(main_window->frontend);
    auto document = QJsonDocument::fromJson(QByteArray(json));
    supershuckie_string_free(json);
    return document.object();
}

BookmarkWindow::BookmarkWindow(MainWindow *main_window): QWidget(main_window, Qt::Window), main_window(main_window) {
    this->setWindowTitle("Bookmarks");

    auto *layout = new QVBoxLayout(this);
    layout->setContentsMargins(8, 8, 8, 8);

    auto *type_row = new QHBoxLayout();
    type_row->addWidget(new QLabel("Type for new bookmarks:", this));
    this->type_combo = new QComboBox(this);
    this->type_combo->setMinimumContentsLength(16);
    this->type_combo->setSizeAdjustPolicy(QComboBox::AdjustToMinimumContentsLengthWithIcon);
    type_row->addWidget(this->type_combo);
    this->types_button = new QPushButton("Types…", this);
    type_row->addWidget(this->types_button);
    type_row->addStretch(1);
    layout->addLayout(type_row);

    auto *buttons = new QHBoxLayout();
    this->add_button = new QPushButton("Add", this);
    this->add_button->setToolTip("Add a bookmark at the current frame (Ctrl+B)");
    this->add_keyframe_button = new QPushButton("Add keyframe", this);
    this->add_keyframe_button->setToolTip("Add a keyframe bookmark, which playback returns to without re-emulating (Ctrl+Shift+B)");
    this->range_button = new QPushButton("Start/end range", this);
    this->range_button->setToolTip("Start a range bookmark at the current frame, or end the one started last (Ctrl+Alt+B)");
    this->set_out_button = new QPushButton("Set out to current", this);
    this->set_out_button->setToolTip("End the selected bookmarks at the current frame");
    this->delete_button = new QPushButton("Delete", this);
    for(auto *button : { this->add_button, this->add_keyframe_button, this->range_button }) {
        buttons->addWidget(button);
    }
    buttons->addSpacing(12);
    buttons->addWidget(this->set_out_button);
    buttons->addStretch(1);
    buttons->addWidget(this->delete_button);
    layout->addLayout(buttons);

    this->tree = new QTreeWidget(this);
    this->tree->setColumnCount(ColumnCount);
    this->tree->setHeaderLabels({ "Type", "Name", "In", "Out", "Duration", "KF" });
    this->tree->headerItem()->setToolTip(Keyframe, "Keyframe bookmark");
    this->tree->setRootIsDecorated(false);
    this->tree->setSelectionMode(QAbstractItemView::ExtendedSelection);
    this->tree->setEditTriggers(QAbstractItemView::NoEditTriggers);
    this->tree->setContextMenuPolicy(Qt::CustomContextMenu);
    this->tree->setUniformRowHeights(true);
    this->tree->setAllColumnsShowFocus(true);
    this->tree->header()->setStretchLastSection(false);
    this->tree->header()->setSectionResizeMode(Name, QHeaderView::Stretch);
    for(int column : { Type, In, Out, Duration, Keyframe }) {
        this->tree->header()->setSectionResizeMode(column, QHeaderView::ResizeToContents);
    }
    layout->addWidget(this->tree, 1);

    this->status = new QLabel(this);
    this->status->setWordWrap(true);
    layout->addWidget(this->status);

    connect(this->add_button, &QPushButton::clicked, this->main_window, &MainWindow::do_add_bookmark);
    connect(this->add_keyframe_button, &QPushButton::clicked, this->main_window, &MainWindow::do_add_keyframe_bookmark);
    connect(this->range_button, &QPushButton::clicked, this->main_window, &MainWindow::do_toggle_range_bookmark);
    connect(this->set_out_button, SIGNAL(clicked()), this, SLOT(on_set_out()));
    connect(this->delete_button, SIGNAL(clicked()), this, SLOT(on_delete()));
    connect(this->types_button, SIGNAL(clicked()), this, SLOT(on_types()));
    connect(this->type_combo, SIGNAL(activated(int)), this, SLOT(on_type_chosen(int)));
    connect(this->tree, &QTreeWidget::itemClicked, this, &BookmarkWindow::on_item_clicked);
    connect(this->tree, &QTreeWidget::itemDoubleClicked, this, &BookmarkWindow::on_item_double_clicked);
    connect(this->tree, &QTreeWidget::itemChanged, this, &BookmarkWindow::on_item_changed);
    connect(this->tree, &QTreeWidget::customContextMenuRequested, this, &BookmarkWindow::on_context_menu);
    connect(this->tree, &QTreeWidget::itemSelectionChanged, this, &BookmarkWindow::update_buttons);

    auto *delete_shortcut = new QShortcut(QKeySequence::Delete, this->tree);
    delete_shortcut->setContext(Qt::WidgetShortcut);
    connect(delete_shortcut, &QShortcut::activated, this, &BookmarkWindow::on_delete);
    auto *backspace_shortcut = new QShortcut(QKeySequence(Qt::Key_Backspace), this->tree);
    backspace_shortcut->setContext(Qt::WidgetShortcut);
    connect(backspace_shortcut, &QShortcut::activated, this, &BookmarkWindow::on_delete);

    this->resize(720, 420);
    this->rebuild();
}

void BookmarkWindow::tick() {
    if(supershuckie_frontend_bookmark_generation(this->main_window->frontend) != this->generation) {
        this->rebuild();
    }
}

QJsonObject BookmarkWindow::bookmark(std::uint64_t id) const {
    for(const auto &value : this->state["bookmarks"].toArray()) {
        auto object = value.toObject();
        if(json_id(object["id"]) == id) {
            return object;
        }
    }
    return QJsonObject();
}

std::vector<std::uint64_t> BookmarkWindow::selected_ids() const {
    std::vector<std::uint64_t> ids;
    for(auto *item : this->tree->selectedItems()) {
        ids.push_back(item->data(Name, Qt::UserRole).toULongLong());
    }
    return ids;
}

void BookmarkWindow::rebuild() {
    this->rebuilding = true;
    this->state = BookmarkWindow::read_state(this->main_window);
    this->generation = static_cast<std::uint64_t>(this->state["generation"].toDouble());

    std::set<std::uint64_t> selected;
    for(auto id : this->selected_ids()) {
        selected.insert(id);
    }
    int scroll = this->tree->verticalScrollBar()->value();

    this->tree->clear();
    for(const auto &value : this->state["bookmarks"].toArray()) {
        auto bookmark = value.toObject();
        auto id = json_id(bookmark["id"]);
        auto *item = new QTreeWidgetItem(this->tree);

        auto kind = bookmark["type"];
        if(kind.isObject()) {
            auto type = kind.toObject();
            QColor color(type["color"].toString());
            item->setIcon(Type, bookmark_swatch(color));
            item->setText(Type, type["name"].toString());
            if(!type["saved"].toBool()) {
                item->setToolTip(Type, "This type is not one of yours; its name and color come from the replay.");
            }
            for(int column = 0; column < ColumnCount; column++) {
                item->setBackground(column, tint_for(color));
            }
        }
        else {
            item->setText(Type, "—");
        }

        item->setText(Name, bookmark["name"].toString());
        item->setData(Name, Qt::UserRole, QVariant::fromValue<qulonglong>(id));
        item->setFlags(item->flags() | Qt::ItemIsEditable);

        auto in_frame = static_cast<std::uint64_t>(bookmark["in_frame"].toDouble());
        auto in_millis = static_cast<std::uint64_t>(bookmark["in_millis"].toDouble());
        item->setText(In, QString("%1 · %2").arg(in_frame).arg(format_bookmark_time(in_millis)));
        item->setTextAlignment(In, Qt::AlignRight | Qt::AlignVCenter);

        if(bookmark["out_frame"].isDouble()) {
            auto out_frame = static_cast<std::uint64_t>(bookmark["out_frame"].toDouble());
            auto out_millis = static_cast<std::uint64_t>(bookmark["out_millis"].toDouble());
            item->setText(Out, QString("%1 · %2").arg(out_frame).arg(format_bookmark_time(out_millis)));
            item->setText(Duration, QString("%1 f · %2").arg(out_frame - in_frame).arg(format_bookmark_time(out_millis >= in_millis ? out_millis - in_millis : 0)));
            item->setToolTip(Out, "Click to go to the out frame");
        }
        else if(json_id(this->state["open_range"]) == id && this->state["open_range"].isDouble()) {
            item->setText(Out, "(open)");
            item->setToolTip(Out, "Press Start/end range again to end this range");
        }
        item->setTextAlignment(Out, Qt::AlignRight | Qt::AlignVCenter);
        item->setTextAlignment(Duration, Qt::AlignRight | Qt::AlignVCenter);

        if(bookmark["keyframe"].toBool()) {
            item->setText(Keyframe, "◆");
            item->setToolTip(Keyframe, "Keyframe bookmark: playback returns here without re-emulating");
            item->setTextAlignment(Keyframe, Qt::AlignCenter);
        }

        if(selected.contains(id)) {
            item->setSelected(true);
        }
    }
    this->tree->verticalScrollBar()->setValue(scroll);

    this->rebuild_type_combo();

    auto state_name = this->state["state"].toString();
    QString text;
    if(state_name == "none") {
        text = "Bookmarks belong to a replay. Record or play back a replay to use them.";
    }
    else {
        auto count = this->state["bookmarks"].toArray().size();
        text = QString("%1 bookmark%2 in %3 (%4).")
            .arg(count)
            .arg(count == 1 ? "" : "s")
            .arg(this->state["replay"].toString())
            .arg(state_name == "recording" ? "recording" : "playing back");
        if(state_name == "recording") {
            text += " Seeking to bookmarks works during playback.";
        }
        else if(this->state["needs_upgrade"].toBool()) {
            text += QString(" This replay uses format v%1; saving bookmarks upgrades it.").arg(this->state["replay_version"].toInt());
        }
    }
    if(this->state["problem"].isString()) {
        text += "\n" + this->state["problem"].toString();
    }
    this->status->setText(text);

    this->update_buttons();
    this->rebuilding = false;
}

void BookmarkWindow::rebuild_type_combo() {
    this->type_combo->clear();
    this->type_combo->addItem("Untyped", QString());
    int active_index = 0;
    auto active = this->state["active_type"].toString();
    for(const auto &value : this->state["types"].toArray()) {
        auto type = value.toObject();
        if(!type["saved"].toBool()) {
            continue;
        }
        this->type_combo->addItem(bookmark_swatch(QColor(type["color"].toString())), type["name"].toString(), type["id"].toString());
        if(type["id"].toString() == active) {
            active_index = this->type_combo->count() - 1;
        }
    }
    this->type_combo->setCurrentIndex(active_index);
}

void BookmarkWindow::update_buttons() {
    bool editable = this->state["editable"].toBool();
    bool any_selected = !this->tree->selectedItems().isEmpty();
    this->add_button->setEnabled(editable);
    this->add_keyframe_button->setEnabled(editable);
    this->range_button->setEnabled(editable);
    this->set_out_button->setEnabled(editable && any_selected);
    this->delete_button->setEnabled(editable && any_selected);
}

bool BookmarkWindow::update_bookmark(std::uint64_t id, const QJsonObject &patch) {
    auto *frontend = this->main_window->frontend;
    auto json = json_string(patch).toStdString();
    auto result = BookmarkWindow::run_operation(this->main_window, this, [frontend, id, &json](bool allow_upgrade, char *out, std::size_t out_len) {
        return supershuckie_frontend_bookmark_update_json(frontend, id, json.c_str(), allow_upgrade, out, out_len);
    });
    this->tick();
    return result.has_value();
}

void BookmarkWindow::go_to(std::uint64_t id, bool out_point) {
    if(this->state["state"].toString() != "playback") {
        return;
    }
    char error[512] = {};
    if(!supershuckie_frontend_bookmark_go_to(this->main_window->frontend, id, out_point, error, sizeof(error))) {
        this->status->setText(QString::fromUtf8(error));
    }
}

void BookmarkWindow::on_item_clicked(QTreeWidgetItem *item, int column) {
    if(item == nullptr) {
        return;
    }
    auto id = item->data(Name, Qt::UserRole).toULongLong();
    bool out_point = column == Out && this->bookmark(id)["out_frame"].isDouble();
    this->go_to(id, out_point);
}

void BookmarkWindow::on_item_double_clicked(QTreeWidgetItem *item, int column) {
    if(item != nullptr && column == Name && this->state["editable"].toBool()) {
        this->tree->editItem(item, Name);
    }
}

void BookmarkWindow::on_item_changed(QTreeWidgetItem *item, int column) {
    if(this->rebuilding || item == nullptr || column != Name) {
        return;
    }
    auto id = item->data(Name, Qt::UserRole).toULongLong();
    auto name = item->text(Name).trimmed();
    if(name.isEmpty() || name == this->bookmark(id)["name"].toString()) {
        this->generation = 0;
        this->tick();
        return;
    }
    QJsonObject patch;
    patch["name"] = name;
    if(!this->update_bookmark(id, patch)) {
        this->generation = 0;
        this->tick();
    }
}

void BookmarkWindow::on_context_menu(const QPoint &position) {
    auto *item = this->tree->itemAt(position);
    if(item == nullptr) {
        return;
    }
    auto id = item->data(Name, Qt::UserRole).toULongLong();
    auto bookmark = this->bookmark(id);
    bool editable = this->state["editable"].toBool();
    bool playback = this->state["state"].toString() == "playback";
    bool has_out = bookmark["out_frame"].isDouble();

    QMenu menu(this);
    auto *go_in = menu.addAction("Go to in frame");
    go_in->setEnabled(playback);
    auto *go_out = menu.addAction("Go to out frame");
    go_out->setEnabled(playback && has_out);
    menu.addSeparator();
    auto *rename = menu.addAction("Rename");
    rename->setEnabled(editable);

    auto *type_menu = menu.addMenu("Type");
    type_menu->setEnabled(editable);
    auto current_type = bookmark["type"].isObject() ? bookmark["type"].toObject()["id"].toString() : QString();
    auto *untyped = type_menu->addAction("Untyped");
    untyped->setCheckable(true);
    untyped->setChecked(current_type.isEmpty());
    untyped->setData(QString("none"));
    for(const auto &value : this->state["types"].toArray()) {
        auto type = value.toObject();
        auto *action = type_menu->addAction(bookmark_swatch(QColor(type["color"].toString())), type["name"].toString());
        action->setCheckable(true);
        action->setChecked(type["id"].toString() == current_type);
        action->setData(type["id"].toString());
    }

    menu.addSeparator();
    auto *set_in = menu.addAction("Set in frame to current frame");
    set_in->setEnabled(editable);
    auto *set_out = menu.addAction("Set out frame to current frame");
    set_out->setEnabled(editable);
    auto *clear_out = menu.addAction("Remove out frame");
    clear_out->setEnabled(editable && has_out);
    menu.addSeparator();
    auto *remove = menu.addAction("Delete");
    remove->setEnabled(editable);

    auto *chosen = menu.exec(this->tree->viewport()->mapToGlobal(position));
    if(chosen == nullptr) {
        return;
    }

    if(chosen == go_in) {
        this->go_to(id, false);
    }
    else if(chosen == go_out) {
        this->go_to(id, true);
    }
    else if(chosen == rename) {
        this->tree->editItem(item, Name);
    }
    else if(chosen == set_in || chosen == set_out) {
        std::uint32_t frames = 0;
        supershuckie_frontend_get_elapsed_time(this->main_window->frontend, &frames, nullptr);
        QJsonObject patch;
        if(chosen == set_in) {
            patch["frame"] = static_cast<double>(frames);
        }
        else {
            patch["out"] = "now";
        }
        this->update_bookmark(id, patch);
    }
    else if(chosen == clear_out) {
        QJsonObject patch;
        patch["out"] = "none";
        this->update_bookmark(id, patch);
    }
    else if(chosen == remove) {
        this->on_delete();
    }
    else if(chosen->parent() == type_menu || type_menu->actions().contains(chosen)) {
        auto data = chosen->data().toString();
        QJsonObject patch;
        patch["type_id"] = data == "none" ? QString() : data;
        this->update_bookmark(id, patch);
    }
}

void BookmarkWindow::on_type_chosen(int index) {
    auto id = this->type_combo->itemData(index).toString().toStdString();
    supershuckie_frontend_set_bookmark_active_type(this->main_window->frontend, id.c_str());
    this->tick();
}

void BookmarkWindow::on_set_out() {
    QJsonObject patch;
    patch["out"] = "now";
    for(auto id : this->selected_ids()) {
        if(!this->update_bookmark(id, patch)) {
            break;
        }
    }
}

void BookmarkWindow::on_delete() {
    auto ids = this->selected_ids();
    if(ids.empty() || !this->state["editable"].toBool()) {
        return;
    }

    QString question = ids.size() == 1
        ? QString("Delete \"%1\"?").arg(this->bookmark(ids[0])["name"].toString())
        : QString("Delete %1 bookmarks?").arg(ids.size());
    this->main_window->stop_timer();
    auto answer = QMessageBox::question(this, "Delete bookmarks", question, QMessageBox::Yes | QMessageBox::Cancel, QMessageBox::Cancel);
    this->main_window->start_timer();
    if(answer != QMessageBox::Yes) {
        return;
    }

    auto *frontend = this->main_window->frontend;
    for(auto id : ids) {
        auto result = BookmarkWindow::run_operation(this->main_window, this, [frontend, id](bool allow_upgrade, char *out, std::size_t out_len) {
            return supershuckie_frontend_bookmark_delete(frontend, id, allow_upgrade, out, out_len);
        });
        if(!result.has_value()) {
            break;
        }
    }
    this->tick();
}

void BookmarkWindow::on_types() {
    BookmarkTypesDialog dialog(this->main_window, this);
    dialog.exec();
    this->tick();
}

QString BookmarkWindow::save_state() const {
    return QString("%1|%2").arg(this->isVisible() ? "1" : "0", QString::fromLatin1(this->saveGeometry().toBase64()));
}

void BookmarkWindow::restore_state(const QString &state) {
    auto fields = state.split('|');
    if(fields.size() >= 2) {
        this->restoreGeometry(QByteArray::fromBase64(fields[1].toLatin1()));
    }
    if(fields.size() >= 1 && fields[0] == "1") {
        this->show();
    }
}

// ---------------------------------------------------------------------------------------------
// Types

BookmarkTypesDialog::BookmarkTypesDialog(MainWindow *main_window, QWidget *parent): QDialog(parent), main_window(main_window) {
    this->setWindowTitle("Bookmark types");

    auto *layout = new QHBoxLayout(this);

    this->list = new QListWidget(this);
    this->list->setMinimumWidth(240);
    layout->addWidget(this->list, 1);

    auto *buttons = new QVBoxLayout();
    auto *add = new QPushButton("Add…", this);
    this->rename_button = new QPushButton("Rename…", this);
    this->color_button = new QPushButton("Color…", this);
    this->delete_button = new QPushButton("Delete", this);
    auto *close = new QPushButton("Close", this);
    for(auto *button : { add, this->rename_button, this->color_button, this->delete_button }) {
        buttons->addWidget(button);
    }
    buttons->addStretch(1);
    buttons->addWidget(close);
    layout->addLayout(buttons);

    connect(add, SIGNAL(clicked()), this, SLOT(on_add()));
    connect(this->rename_button, SIGNAL(clicked()), this, SLOT(on_rename()));
    connect(this->color_button, SIGNAL(clicked()), this, SLOT(on_color()));
    connect(this->delete_button, SIGNAL(clicked()), this, SLOT(on_delete()));
    connect(close, SIGNAL(clicked()), this, SLOT(accept()));
    connect(this->list, &QListWidget::itemDoubleClicked, this, [this]() { this->on_color(); });
    connect(this->list, &QListWidget::itemSelectionChanged, this, [this]() {
        bool any = !this->selected_type().isEmpty();
        this->rename_button->setEnabled(any);
        this->color_button->setEnabled(any);
        this->delete_button->setEnabled(any);
    });

    this->rebuild();
    this->resize(420, 300);
}

int BookmarkTypesDialog::exec() {
    this->main_window->stop_timer();
    int result = QDialog::exec();
    this->main_window->start_timer();
    return result;
}

void BookmarkTypesDialog::rebuild(const QString &select_id) {
    char *json = supershuckie_frontend_bookmark_types_json(this->main_window->frontend);
    this->types = QJsonDocument::fromJson(QByteArray(json)).array();
    supershuckie_string_free(json);

    this->list->clear();
    for(const auto &value : this->types) {
        auto type = value.toObject();
        if(!type["saved"].toBool()) {
            continue;
        }
        auto *item = new QListWidgetItem(bookmark_swatch(QColor(type["color"].toString())), type["name"].toString(), this->list);
        item->setData(Qt::UserRole, type["id"].toString());
        if(type["id"].toString() == select_id) {
            item->setSelected(true);
            this->list->setCurrentItem(item);
        }
    }

    bool any = !this->selected_type().isEmpty();
    this->rename_button->setEnabled(any);
    this->color_button->setEnabled(any);
    this->delete_button->setEnabled(any);
}

QJsonObject BookmarkTypesDialog::selected_type() const {
    auto items = this->list->selectedItems();
    if(items.isEmpty()) {
        return QJsonObject();
    }
    auto id = items[0]->data(Qt::UserRole).toString();
    for(const auto &value : this->types) {
        if(value.toObject()["id"].toString() == id) {
            return value.toObject();
        }
    }
    return QJsonObject();
}

std::optional<QJsonObject> BookmarkTypesDialog::upsert(const QJsonObject &type) {
    char out[2048] = {};
    auto json = json_string(type).toStdString();
    if(!supershuckie_frontend_bookmark_type_upsert_json(this->main_window->frontend, json.c_str(), out, sizeof(out))) {
        QMessageBox::warning(this, "Bookmark types", QString::fromUtf8(out));
        return std::nullopt;
    }
    return QJsonDocument::fromJson(QByteArray(out)).object();
}

void BookmarkTypesDialog::on_add() {
    bool ok = false;
    auto name = QInputDialog::getText(this, "New bookmark type", "Name:", QLineEdit::Normal, QString(), &ok).trimmed();
    if(!ok || name.isEmpty()) {
        return;
    }
    QJsonObject request;
    request["name"] = name;
    auto created = this->upsert(request);
    if(!created.has_value()) {
        return;
    }

    // Offer the palette color it was given, so choosing one is a single step.
    auto color = QColorDialog::getColor(QColor((*created)["color"].toString()), this, QString("Color for %1").arg(name));
    if(color.isValid()) {
        QJsonObject change;
        change["id"] = (*created)["id"];
        change["color"] = color.name(QColor::HexRgb).toUpper();
        this->upsert(change);
    }
    this->rebuild((*created)["id"].toString());
}

void BookmarkTypesDialog::on_rename() {
    auto type = this->selected_type();
    if(type.isEmpty()) {
        return;
    }
    bool ok = false;
    auto name = QInputDialog::getText(this, "Rename bookmark type", "Name:", QLineEdit::Normal, type["name"].toString(), &ok).trimmed();
    if(!ok || name.isEmpty() || name == type["name"].toString()) {
        return;
    }
    QJsonObject change;
    change["id"] = type["id"];
    change["name"] = name;
    this->upsert(change);
    this->rebuild(type["id"].toString());
}

void BookmarkTypesDialog::on_color() {
    auto type = this->selected_type();
    if(type.isEmpty()) {
        return;
    }
    auto color = QColorDialog::getColor(QColor(type["color"].toString()), this, QString("Color for %1").arg(type["name"].toString()));
    if(!color.isValid()) {
        return;
    }
    QJsonObject change;
    change["id"] = type["id"];
    change["color"] = color.name(QColor::HexRgb).toUpper();
    this->upsert(change);
    this->rebuild(type["id"].toString());
}

void BookmarkTypesDialog::on_delete() {
    auto type = this->selected_type();
    if(type.isEmpty()) {
        return;
    }
    auto answer = QMessageBox::question(
        this,
        "Delete bookmark type",
        QString("Delete the type \"%1\"?").arg(type["name"].toString()),
        QMessageBox::Yes | QMessageBox::Cancel,
        QMessageBox::Cancel
    );
    if(answer != QMessageBox::Yes) {
        return;
    }
    char error[512] = {};
    auto id = type["id"].toString().toStdString();
    if(!supershuckie_frontend_bookmark_type_delete(this->main_window->frontend, id.c_str(), error, sizeof(error))) {
        QMessageBox::warning(this, "Bookmark types", QString::fromUtf8(error));
    }
    this->rebuild();
}

// ---------------------------------------------------------------------------------------------
// Add at frame

AddBookmarkDialog::AddBookmarkDialog(MainWindow *main_window, QWidget *parent): QDialog(parent), main_window(main_window) {
    this->setWindowTitle("Add bookmark");

    auto state = BookmarkWindow::read_state(main_window);
    auto *frontend = main_window->frontend;
    supershuckie_frontend_get_elapsed_time(frontend, &this->current_frame, nullptr);

    std::uint32_t last_frame = this->current_frame;
    if(state["state"].toString() == "playback") {
        supershuckie_frontend_get_replay_playback_time(frontend, &last_frame, nullptr);
    }
    int maximum = static_cast<int>(std::min<std::uint32_t>(last_frame, std::numeric_limits<int>::max()));

    auto *layout = new QVBoxLayout(this);
    auto *form = new QFormLayout();

    this->name = new QLineEdit(this);
    this->name->setPlaceholderText("Generic name");
    form->addRow("Name:", this->name);

    this->type = new QComboBox(this);
    this->type->addItem("Untyped", QString("none"));
    auto active = state["active_type"].toString();
    for(const auto &value : state["types"].toArray()) {
        auto type = value.toObject();
        if(!type["saved"].toBool()) {
            continue;
        }
        this->type->addItem(bookmark_swatch(QColor(type["color"].toString())), type["name"].toString(), type["id"].toString());
        if(type["id"].toString() == active) {
            this->type->setCurrentIndex(this->type->count() - 1);
        }
    }
    form->addRow("Type:", this->type);

    this->in_frame = new QSpinBox(this);
    this->in_frame->setRange(0, maximum);
    this->in_frame->setValue(static_cast<int>(std::min<std::uint32_t>(this->current_frame, maximum)));
    form->addRow("In frame:", this->in_frame);

    auto *out_row = new QHBoxLayout();
    this->has_out = new QCheckBox("Out frame:", this);
    this->out_frame = new QSpinBox(this);
    this->out_frame->setRange(0, maximum);
    this->out_frame->setValue(this->in_frame->value());
    out_row->addWidget(this->out_frame, 1);
    form->addRow(this->has_out, out_row);

    this->keyframe = new QCheckBox("Keyframe bookmark (at the current frame)", this);
    this->keyframe->setToolTip("Playback returns to a keyframe bookmark without re-emulating. It is always placed at the current frame.");
    form->addRow(QString(), this->keyframe);

    layout->addLayout(form);

    auto *buttons = new QDialogButtonBox(QDialogButtonBox::Ok | QDialogButtonBox::Cancel, this);
    buttons->button(QDialogButtonBox::Ok)->setText("Add");
    layout->addWidget(buttons);

    connect(buttons, &QDialogButtonBox::accepted, this, &QDialog::accept);
    connect(buttons, &QDialogButtonBox::rejected, this, &QDialog::reject);
    connect(this->has_out, SIGNAL(toggled(bool)), this, SLOT(update_fields()));
    connect(this->keyframe, SIGNAL(toggled(bool)), this, SLOT(update_fields()));
    connect(this->in_frame, SIGNAL(valueChanged(int)), this, SLOT(update_fields()));

    this->update_fields();
    this->setFixedSize(this->sizeHint());
}

void AddBookmarkDialog::update_fields() {
    this->out_frame->setEnabled(this->has_out->isChecked());
    this->out_frame->setMinimum(this->in_frame->value());
    this->in_frame->setEnabled(!this->keyframe->isChecked());
    if(this->keyframe->isChecked()) {
        this->in_frame->setValue(static_cast<int>(std::min<std::uint32_t>(this->current_frame, this->in_frame->maximum())));
    }
}

int AddBookmarkDialog::exec() {
    this->main_window->stop_timer();
    int result = QDialog::exec();
    this->main_window->start_timer();
    return result;
}

QJsonObject AddBookmarkDialog::request() const {
    QJsonObject request;
    auto name = this->name->text().trimmed();
    if(!name.isEmpty()) {
        request["name"] = name;
    }
    auto type_id = this->type->currentData().toString();
    request["type_id"] = type_id == "none" ? QString() : type_id;
    if(this->keyframe->isChecked()) {
        request["keyframe"] = true;
    }
    else {
        request["frame"] = static_cast<double>(this->in_frame->value());
    }
    if(this->has_out->isChecked()) {
        request["out"] = static_cast<double>(this->out_frame->value());
    }
    return request;
}
