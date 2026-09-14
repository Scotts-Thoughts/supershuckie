#include "shortcuts_settings_window.hpp"
#include "main_window.hpp"

#include <QAction>
#include <QDialogButtonBox>
#include <QGridLayout>
#include <QHeaderView>
#include <QKeyEvent>
#include <QLabel>
#include <QLineEdit>
#include <QMenu>
#include <QMessageBox>
#include <QPushButton>
#include <QSet>
#include <QSignalBlocker>
#include <QTreeWidget>
#include <QVBoxLayout>
#include <algorithm>
#include <functional>
#include <memory>

#include <supershuckie/control_settings.h>

using namespace SuperShuckie64;

static const char *const PATH_SEPARATOR = " › ";

void SuperShuckie64::resolve_shortcuts(std::vector<ShortcutBinding> &bindings) {
    QSet<QKeySequence> claimed;
    auto take = [&claimed](const QList<QKeySequence> &wanted) {
        QList<QKeySequence> granted;
        for(const auto &sequence : wanted) {
            if(!sequence.isEmpty() && granted.size() < SHORTCUT_SLOTS && !claimed.contains(sequence)) {
                claimed.insert(sequence);
                granted.append(sequence);
            }
        }
        return granted;
    };

    for(auto &binding : bindings) {
        if(binding.custom.has_value()) {
            binding.current = take(*binding.custom);
        }
    }
    for(auto &binding : bindings) {
        if(!binding.custom.has_value()) {
            binding.current = take(binding.defaults);
        }
    }
}

static QString shortcuts_text(const QList<QKeySequence> &shortcuts) {
    if(shortcuts.isEmpty()) {
        return "none";
    }
    QStringList names;
    for(const auto &sequence : shortcuts) {
        names.append(sequence.toString(QKeySequence::NativeText));
    }
    return names.join(" or ");
}

ShortcutSequenceEdit::ShortcutSequenceEdit(QWidget *parent): QKeySequenceEdit(parent) {
    this->setMaximumSequenceLength(1);
    this->setClearButtonEnabled(true);

    // Skip QKeySequenceEdit's own clear when recording starts and its re-emit on every focus loss.
    connect(this, &QKeySequenceEdit::keySequenceChanged, this, [this](const QKeySequence &sequence) {
        if(!this->recording && sequence.isEmpty()) {
            emit this->cleared();
        }
    });
    connect(this, &QKeySequenceEdit::editingFinished, this, [this]() {
        if(this->recording) {
            this->recording = false;
            emit this->recorded(this->keySequence());
        }
    });
}

void ShortcutSequenceEdit::keyPressEvent(QKeyEvent *event) {
    // Escape is never recorded: it cancels a recording, and otherwise goes to the dialog.
    if(event->key() == Qt::Key_Escape && event->modifiers() == Qt::NoModifier) {
        if(this->recording) {
            this->cancel_recording();
        }
        else {
            event->ignore();
        }
        return;
    }
    this->recording = true;
    QKeySequenceEdit::keyPressEvent(event);
}

void ShortcutSequenceEdit::focusOutEvent(QFocusEvent *event) {
    QKeySequenceEdit::focusOutEvent(event);
    // A modifier pressed on its own blanks the field but never finishes the recording.
    if(this->recording) {
        this->cancel_recording();
    }
}

void ShortcutSequenceEdit::cancel_recording() {
    this->recording = false;
    emit this->recorded(QKeySequence());
}

ShortcutsSettingsWindow::ShortcutsSettingsWindow(MainWindow *parent, std::vector<ShortcutBinding> bindings): QDialog(parent), parent(parent), bindings(std::move(bindings)) {
    this->setWindowTitle("Shortcuts");

    for(const auto &binding : this->bindings) {
        this->initial_custom.push_back(binding.custom);
    }

    auto *layout = new QVBoxLayout(this);

    this->filter = new QLineEdit(this);
    this->filter->setPlaceholderText("Search by function or shortcut");
    this->filter->setClearButtonEnabled(true);
    this->filter->installEventFilter(this);
    layout->addWidget(this->filter);

    this->tree = new QTreeWidget(this);
    this->tree->setColumnCount(1 + SHORTCUT_SLOTS);
    this->tree->setHeaderLabels({ "Function", "Shortcut", "Alternate" });
    this->tree->setUniformRowHeights(true);
    this->tree->setAllColumnsShowFocus(true);
    this->tree->setContextMenuPolicy(Qt::CustomContextMenu);
    this->tree->installEventFilter(this);
    this->tree->header()->setStretchLastSection(false);
    this->tree->header()->setSectionResizeMode(0, QHeaderView::Stretch);
    for(int slot = 0; slot < SHORTCUT_SLOTS; slot++) {
        this->tree->setColumnWidth(1 + slot, 140);
    }
    layout->addWidget(this->tree, 1);

    auto *details = new QGridLayout();

    this->function_label = new QLabel(this);
    this->function_label->setTextFormat(Qt::PlainText);
    auto bold = this->function_label->font();
    bold.setBold(true);
    this->function_label->setFont(bold);
    details->addWidget(this->function_label, 0, 0, 1, 3);

    const char *slot_names[SHORTCUT_SLOTS] = { "Shortcut:", "Alternate:" };
    for(int slot = 0; slot < SHORTCUT_SLOTS; slot++) {
        auto *edit = new ShortcutSequenceEdit(this);
        this->edits[slot] = edit;
        details->addWidget(new QLabel(slot_names[slot], this), 1 + slot, 0);
        details->addWidget(edit, 1 + slot, 1);
        connect(edit, &ShortcutSequenceEdit::recorded, this, [this, slot](const QKeySequence &sequence) {
            this->record_shortcut(slot, sequence);
        });
        connect(edit, &ShortcutSequenceEdit::cleared, this, [this, slot]() {
            // Queued: refilling the edit from inside its own textChanged leaves its clear button hidden.
            if(auto index = this->selected) {
                QMetaObject::invokeMethod(this, [this, index = *index, slot]() {
                    this->clear_shortcut(index, slot);
                }, Qt::QueuedConnection);
            }
        });
    }

    this->reset_button = new QPushButton("Reset to default", this);
    this->reset_button->setAutoDefault(false);
    details->addWidget(this->reset_button, 1, 2);
    connect(this->reset_button, &QPushButton::clicked, this, [this]() {
        if(this->selected.has_value()) {
            this->reset_binding(*this->selected);
        }
    });

    this->default_label = new QLabel(this);
    this->default_label->setTextFormat(Qt::PlainText);
    details->addWidget(this->default_label, 1 + SHORTCUT_SLOTS, 1, 1, 2);

    this->notes_label = new QLabel(this);
    this->notes_label->setTextFormat(Qt::PlainText);
    this->notes_label->setWordWrap(true);
    this->notes_label->setAlignment(Qt::AlignLeft | Qt::AlignTop);
    this->notes_label->setMinimumHeight(this->notes_label->fontMetrics().lineSpacing() * 2);
    details->addWidget(this->notes_label, 2 + SHORTCUT_SLOTS, 0, 1, 3);

    details->setColumnStretch(1, 1);
    layout->addLayout(details);

    auto *controls_note = new QLabel("Game buttons and hotkeys such as Turbo are set in Settings › Controls.", this);
    controls_note->setAttribute(Qt::WA_MacSmallSize);
    layout->addWidget(controls_note);

    auto *buttons = new QDialogButtonBox(QDialogButtonBox::RestoreDefaults | QDialogButtonBox::Cancel | QDialogButtonBox::Ok, this);
    auto *reset_all = buttons->button(QDialogButtonBox::RestoreDefaults);
    reset_all->setText("Reset all");
    connect(reset_all, &QPushButton::clicked, this, &ShortcutsSettingsWindow::reset_all);
    connect(buttons, &QDialogButtonBox::accepted, this, &QDialog::accept);
    connect(buttons, &QDialogButtonBox::rejected, this, &QDialog::reject);
    layout->addWidget(buttons);

    this->collect_game_controls();
    this->build_tree();
    this->refresh_items();

    connect(this->filter, &QLineEdit::textChanged, this, &ShortcutsSettingsWindow::apply_filter);
    connect(this->tree, &QTreeWidget::currentItemChanged, this, [this](QTreeWidgetItem *current) {
        this->selected = this->binding_for_item(current);
        this->refresh_details();
    });
    connect(this->tree, &QTreeWidget::itemActivated, this, [this](QTreeWidgetItem *item) {
        if(this->binding_for_item(item).has_value()) {
            this->edits[0]->setFocus(Qt::OtherFocusReason);
        }
    });
    connect(this->tree, &QTreeWidget::customContextMenuRequested, this, &ShortcutsSettingsWindow::show_context_menu);

    if(!this->items.empty()) {
        this->tree->setCurrentItem(this->items.front());
    }
    this->refresh_details();
    this->resize(720, 640);
}

int ShortcutsSettingsWindow::exec() {
    this->parent->stop_timer();
    int return_value = QDialog::exec();
    this->parent->start_timer();
    return return_value;
}

void ShortcutsSettingsWindow::reject() {
    if(this->has_changes()) {
        QMessageBox box(this);
        box.setIcon(QMessageBox::Question);
        box.setWindowTitle("Shortcuts");
        box.setText("Discard your changes to the shortcuts?");
        auto *discard = box.addButton("Discard changes", QMessageBox::DestructiveRole);
        auto *keep = box.addButton("Keep editing", QMessageBox::RejectRole);
        box.setDefaultButton(keep);
        box.exec();
        if(box.clickedButton() != discard) {
            return;
        }
    }
    QDialog::reject();
}

bool ShortcutsSettingsWindow::eventFilter(QObject *object, QEvent *event) {
    if(event->type() == QEvent::KeyPress) {
        auto key = static_cast<QKeyEvent *>(event)->key();
        bool enter = key == Qt::Key_Return || key == Qt::Key_Enter;

        // Enter in the search box or the list would otherwise reach the dialog and press OK.
        if(object == this->filter) {
            if(enter || key == Qt::Key_Down) {
                this->select_first_visible();
                return true;
            }
            if(key == Qt::Key_Escape && !this->filter->text().isEmpty()) {
                this->filter->clear();
                return true;
            }
        }
        else if(object == this->tree && enter) {
            if(this->selected.has_value()) {
                this->edits[0]->setFocus(Qt::OtherFocusReason);
            }
            return true;
        }
    }
    return QDialog::eventFilter(object, event);
}

void ShortcutsSettingsWindow::collect_game_controls() {
    const char *core_name = nullptr;
    for(std::uint8_t type = 0; type < 255 && (core_name = supershuckie_frontend_get_emulator_type_name(type)) != nullptr; type++) {
        if(supershuckie_frontend_emulator_type_uses_shared_config(type)) {
            continue;
        }

        std::unique_ptr<SuperShuckieControlSettingsRaw, decltype(&supershuckie_control_settings_free)> settings(
            supershuckie_frontend_get_control_settings(this->parent->frontend, type),
            supershuckie_control_settings_free
        );
        auto core = QString::fromUtf8(core_name);

        const char *control_name = nullptr;
        for(SuperShuckieControlType control = 0; (control_name = supershuckie_control_settings_control_name(control)) != nullptr; control++) {
            if(!supershuckie_control_settings_is_control_available_for_emulator_type(control, type)) {
                continue;
            }

            const char *modifier_name = nullptr;
            for(SuperShuckieControlModifier modifier = 0; (modifier_name = supershuckie_control_settings_modifier_name(modifier)) != nullptr; modifier++) {
                if(modifier != 0 && !supershuckie_control_settings_control_is_button(control)) {
                    break;
                }

                auto count = supershuckie_control_settings_get_controls_for_device(settings.get(), nullptr, false, control, modifier, nullptr, 0);
                if(count == 0) {
                    continue;
                }
                std::vector<std::int32_t> keys(count);
                supershuckie_control_settings_get_controls_for_device(settings.get(), nullptr, false, control, modifier, keys.data(), keys.size());

                auto label = modifier == 0
                    ? QString("%1 (%2)").arg(QString::fromUtf8(control_name), core)
                    : QString("%1 %2 (%3)").arg(QString::fromUtf8(modifier_name), QString::fromUtf8(control_name), core);
                for(auto key : keys) {
                    this->game_controls[key].append(label);
                }
            }
        }
    }
}

void ShortcutsSettingsWindow::build_tree() {
    auto bold = this->tree->font();
    bold.setBold(true);

    QHash<QString, QTreeWidgetItem *> groups;
    for(std::size_t i = 0; i < this->bindings.size(); i++) {
        const auto &binding = this->bindings[i];

        QTreeWidgetItem *parent = nullptr;
        QString group_key;
        for(const auto &title : binding.path) {
            group_key += title + '\n';
            auto *group = groups.value(group_key);
            if(group == nullptr) {
                group = parent == nullptr ? new QTreeWidgetItem(this->tree) : new QTreeWidgetItem(parent);
                group->setText(0, title);
                group->setFont(0, bold);
                group->setFlags(Qt::ItemIsEnabled);
                group->setFirstColumnSpanned(true);
                groups.insert(group_key, group);
            }
            parent = group;
        }

        auto *item = parent == nullptr ? new QTreeWidgetItem(this->tree) : new QTreeWidgetItem(parent);
        item->setText(0, binding.name);
        item->setData(0, Qt::UserRole, QVariant::fromValue<qulonglong>(i));
        this->items.push_back(item);
    }

    this->tree->expandAll();
}

void ShortcutsSettingsWindow::refresh_items() {
    for(std::size_t i = 0; i < this->bindings.size(); i++) {
        const auto &binding = this->bindings[i];
        auto *item = this->items[i];
        bool changed = binding.custom.has_value();

        for(int column = 0; column < 1 + SHORTCUT_SLOTS; column++) {
            if(column > 0) {
                item->setText(column, binding.current.value(column - 1).toString(QKeySequence::NativeText));
            }
            auto font = item->font(column);
            font.setBold(changed);
            item->setFont(column, font);
        }
        item->setToolTip(0, changed ? QString("Changed from the default (%1)").arg(shortcuts_text(binding.defaults)) : QString());
    }

    this->apply_filter();
}

void ShortcutsSettingsWindow::refresh_details() {
    if(!this->selected.has_value()) {
        this->function_label->setText("Select a function to change its shortcuts.");
        for(auto *edit : this->edits) {
            QSignalBlocker blocker(edit);
            edit->clear();
            edit->setEnabled(false);
        }
        this->reset_button->setEnabled(false);
        this->default_label->clear();
        this->notes_label->clear();
        return;
    }

    const auto &binding = this->bindings[*this->selected];
    this->function_label->setText((binding.path + QStringList(binding.name)).join(PATH_SEPARATOR));

    for(int slot = 0; slot < SHORTCUT_SLOTS; slot++) {
        QSignalBlocker blocker(this->edits[slot]);
        this->edits[slot]->setKeySequence(binding.current.value(slot));
        // Slots fill in order, so an alternate needs a shortcut in front of it.
        this->edits[slot]->setEnabled(slot <= binding.current.size());
    }

    this->reset_button->setEnabled(this->can_reset(*this->selected));
    this->default_label->setText(QString("Default: %1").arg(shortcuts_text(binding.defaults)));

    QStringList notes;
    if(!binding.custom.has_value()) {
        for(const auto &sequence : binding.defaults) {
            if(binding.current.contains(sequence)) {
                continue;
            }
            for(std::size_t other = 0; other < this->bindings.size(); other++) {
                if(this->bindings[other].current.contains(sequence)) {
                    notes.append(QString("Its default shortcut, %1, is in use by %2.").arg(sequence.toString(QKeySequence::NativeText), this->describe(other)));
                    break;
                }
            }
        }
    }
    if(binding.playback_control) {
        notes.append("Works while watching a replay when Replays › Allow keyboard to control replay playback is on.");
    }
    else {
        for(const auto &sequence : binding.current) {
            auto combination = sequence[0];
            if(combination.keyboardModifiers() & (Qt::ControlModifier | Qt::AltModifier | Qt::MetaModifier)) {
                continue;
            }
            auto controls = this->game_controls.value(combination.key());
            if(!controls.isEmpty()) {
                notes.append(QString("%1 is also a game control: %2. Pressing it runs this shortcut instead of reaching the game.")
                    .arg(sequence.toString(QKeySequence::NativeText), controls.join(", ")));
            }
        }
    }
    this->notes_label->setText(notes.join("\n"));
}

void ShortcutsSettingsWindow::apply_filter() {
    auto needle = this->filter->text().trimmed();
    auto matches = [&needle](const ShortcutBinding &binding) {
        if(needle.isEmpty() || binding.name.contains(needle, Qt::CaseInsensitive)) {
            return true;
        }
        for(const auto &title : binding.path) {
            if(title.contains(needle, Qt::CaseInsensitive)) {
                return true;
            }
        }
        for(const auto &sequence : binding.current) {
            if(sequence.toString(QKeySequence::NativeText).contains(needle, Qt::CaseInsensitive) ||
               sequence.toString(QKeySequence::PortableText).contains(needle, Qt::CaseInsensitive)) {
                return true;
            }
        }
        return false;
    };

    std::function<bool(QTreeWidgetItem *)> update = [&](QTreeWidgetItem *item) {
        bool visible = false;
        if(auto index = this->binding_for_item(item)) {
            visible = matches(this->bindings[*index]);
        }
        else {
            for(int i = 0; i < item->childCount(); i++) {
                visible = update(item->child(i)) || visible;
            }
        }
        item->setHidden(!visible);
        return visible;
    };

    for(int i = 0; i < this->tree->topLevelItemCount(); i++) {
        update(this->tree->topLevelItem(i));
    }
}

void ShortcutsSettingsWindow::select_first_visible() {
    for(auto *item : this->items) {
        if(!item->isHidden()) {
            this->tree->setCurrentItem(item);
            break;
        }
    }
    this->tree->setFocus(Qt::OtherFocusReason);
}

std::optional<std::size_t> ShortcutsSettingsWindow::binding_for_item(QTreeWidgetItem *item) const {
    if(item == nullptr) {
        return std::nullopt;
    }
    auto data = item->data(0, Qt::UserRole);
    if(!data.isValid()) {
        return std::nullopt;
    }
    return static_cast<std::size_t>(data.toULongLong());
}

QString ShortcutsSettingsWindow::describe(std::size_t index) const {
    const auto &binding = this->bindings[index];
    return QString("“%1” (%2)").arg(binding.name, binding.path.join(PATH_SEPARATOR));
}

bool ShortcutsSettingsWindow::has_changes() const {
    for(std::size_t i = 0; i < this->bindings.size(); i++) {
        if(this->bindings[i].custom != this->initial_custom[i]) {
            return true;
        }
    }
    return false;
}

void ShortcutsSettingsWindow::record_shortcut(int slot, const QKeySequence &sequence) {
    if(!this->selected.has_value()) {
        return;
    }
    auto index = *this->selected;
    auto shortcuts = this->bindings[index].current;
    auto existing = shortcuts.indexOf(sequence);

    bool unchanged = sequence.isEmpty() || existing == slot || (existing >= 0 && slot >= shortcuts.size());
    if(unchanged || (existing < 0 && !this->take_from_others(index, { sequence }))) {
        this->refresh_details();
        if(existing >= 0 && existing != slot) {
            this->notes_label->setText(QString("%1 is already this function's shortcut.").arg(sequence.toString(QKeySequence::NativeText)));
        }
        return;
    }

    if(existing >= 0) {
        shortcuts.swapItemsAt(existing, slot);
    }
    else if(slot < shortcuts.size()) {
        shortcuts[slot] = sequence;
    }
    else {
        shortcuts.append(sequence);
    }
    this->set_shortcuts(index, shortcuts);
}

void ShortcutsSettingsWindow::clear_shortcut(std::size_t index, int slot) {
    auto shortcuts = this->bindings[index].current;
    if(slot >= shortcuts.size()) {
        return;
    }
    shortcuts.removeAt(slot);
    this->set_shortcuts(index, shortcuts);
}

void ShortcutsSettingsWindow::set_shortcuts(std::size_t index, QList<QKeySequence> shortcuts) {
    auto &binding = this->bindings[index];
    shortcuts.removeAll(QKeySequence());
    if(shortcuts == binding.defaults) {
        binding.custom.reset();
    }
    else {
        binding.custom = shortcuts;
    }

    resolve_shortcuts(this->bindings);
    this->refresh_items();
    this->refresh_details();
}

bool ShortcutsSettingsWindow::take_from_others(std::size_t index, const QList<QKeySequence> &sequences) {
    std::vector<std::pair<std::size_t, QKeySequence>> conflicts;
    for(std::size_t other = 0; other < this->bindings.size(); other++) {
        if(other == index) {
            continue;
        }
        for(const auto &sequence : sequences) {
            if(this->bindings[other].current.contains(sequence)) {
                conflicts.emplace_back(other, sequence);
            }
        }
    }
    if(conflicts.empty()) {
        return true;
    }

    QStringList lines;
    for(const auto &[other, sequence] : conflicts) {
        lines.append(QString("%1 is already assigned to %2.").arg(sequence.toString(QKeySequence::NativeText), this->describe(other)));
    }

    QMessageBox box(this);
    box.setIcon(QMessageBox::Question);
    box.setWindowTitle("Shortcut in use");
    box.setText(lines.join("\n"));
    box.setInformativeText(QString("Reassign %1 to %2?").arg(conflicts.size() == 1 ? "it" : "them", this->describe(index)));
    auto *reassign = box.addButton("Reassign", QMessageBox::AcceptRole);
    box.addButton(QMessageBox::Cancel);
    box.setDefaultButton(reassign);
    box.exec();
    if(box.clickedButton() != reassign) {
        return false;
    }

    // Only customized bindings need editing; a default simply loses the resolution to the new custom
    // and comes back on its own if that custom is later cleared.
    for(const auto &[other, sequence] : conflicts) {
        auto &binding = this->bindings[other];
        if(binding.custom.has_value()) {
            binding.custom->removeAll(sequence);
            if(*binding.custom == binding.defaults) {
                binding.custom.reset();
            }
        }
    }
    return true;
}

bool ShortcutsSettingsWindow::can_reset(std::size_t index) const {
    const auto &binding = this->bindings[index];
    return binding.custom.has_value() || binding.current != binding.defaults;
}

void ShortcutsSettingsWindow::reset_binding(std::size_t index) {
    const auto defaults = this->bindings[index].defaults;
    if(this->take_from_others(index, defaults)) {
        this->set_shortcuts(index, defaults);
    }
}

void ShortcutsSettingsWindow::reset_all() {
    bool customized = std::any_of(this->bindings.begin(), this->bindings.end(), [](const ShortcutBinding &binding) {
        return binding.custom.has_value();
    });
    if(!customized) {
        return;
    }
    if(QMessageBox::question(this, "Reset all shortcuts", "Reset every shortcut to its default?") != QMessageBox::Yes) {
        return;
    }

    for(auto &binding : this->bindings) {
        binding.custom.reset();
    }
    resolve_shortcuts(this->bindings);
    this->refresh_items();
    this->refresh_details();
}

void ShortcutsSettingsWindow::show_context_menu(const QPoint &position) {
    auto *item = this->tree->itemAt(position);
    auto index = this->binding_for_item(item);
    if(!index.has_value()) {
        return;
    }
    this->tree->setCurrentItem(item);
    const auto &binding = this->bindings[*index];

    QMenu menu(this);
    auto *clear = menu.addAction("Clear shortcuts");
    clear->setEnabled(!binding.current.isEmpty());
    auto *reset = menu.addAction("Reset to default");
    reset->setEnabled(this->can_reset(*index));

    auto *chosen = menu.exec(this->tree->viewport()->mapToGlobal(position));
    if(chosen == clear) {
        this->set_shortcuts(*index, {});
    }
    else if(chosen == reset) {
        this->reset_binding(*index);
    }
}
