#include "nds_date_dialog.hpp"
#include "main_window.hpp"
#include "ask_for_text_dialog.hpp"

#include <QDate>
#include <QGroupBox>
#include <QHBoxLayout>
#include <QLabel>
#include <QListWidget>
#include <QMessageBox>
#include <QSignalBlocker>
#include <QSpinBox>
#include <QGridLayout>
#include <QPushButton>
#include <QVBoxLayout>

using namespace SuperShuckie64;

static const int PRESET_NAME_ROLE = Qt::UserRole;
static const int PRESET_DATE_ROLE = Qt::UserRole + 1;

static QDate to_qdate(const SuperShuckieNintendoDSDate &date) {
    return QDate(date.year, date.month, date.day);
}

QString NDSDateDialog::describe_date(const SuperShuckieNintendoDSDate &date) {
    auto text = QString("%1 %2:%3")
        .arg(to_qdate(date).toString("ddd yyyy-MM-dd"))
        .arg(int(date.hour), 2, 10, QChar('0'))
        .arg(int(date.minute), 2, 10, QChar('0'));
    if(date.second != 0) {
        text += QString(":%1").arg(int(date.second), 2, 10, QChar('0'));
    }
    return text;
}

bool NDSDateDialog::same_date(const SuperShuckieNintendoDSDate &a, const SuperShuckieNintendoDSDate &b) {
    return a.year == b.year && a.month == b.month && a.day == b.day && a.hour == b.hour && a.minute == b.minute && a.second == b.second;
}

std::vector<NDSDatePreset> NDSDateDialog::load_presets(const SuperShuckieFrontendRaw *frontend) {
    std::vector<NDSDatePreset> presets;
    auto count = supershuckie_frontend_get_nds_date_preset_count(frontend);
    for(std::size_t i = 0; i < count; i++) {
        NDSDatePreset preset = {};
        const char *name = supershuckie_frontend_get_nds_date_preset(frontend, i, &preset.date);
        preset.name = QString::fromUtf8(name);
        presets.push_back(std::move(preset));
    }
    return presets;
}

NDSDateDialog::NDSDateDialog(MainWindow *main_window): QDialog(main_window), main_window(main_window) {
    this->setWindowTitle("Set Nintendo DS date");
    auto *layout = new QGridLayout(this);

    auto *presets_box = new QGroupBox("Presets", this);
    auto *presets_layout = new QHBoxLayout(presets_box);
    this->presets = new QListWidget(presets_box);
    // The order here is the order of Gameplay > Reload core with date.
    this->presets->setDragDropMode(QAbstractItemView::InternalMove);
    this->presets->setToolTip("Click a preset to fill in its date below, or double-click it to use it now. Drag to reorder.");
    presets_layout->addWidget(this->presets);

    auto *preset_buttons = new QVBoxLayout();
    this->add_preset = new QPushButton("Add…", presets_box);
    this->add_preset->setToolTip("Save the date below as a new preset");
    this->update_preset = new QPushButton("Update", presets_box);
    this->update_preset->setToolTip("Change the selected preset to the date below");
    this->rename_preset = new QPushButton("Rename…", presets_box);
    this->remove_preset = new QPushButton("Remove", presets_box);
    for(auto *button : { this->add_preset, this->update_preset, this->rename_preset, this->remove_preset }) {
        // Return stays with the dialog's own buttons.
        button->setAutoDefault(false);
        preset_buttons->addWidget(button);
    }
    preset_buttons->addStretch();
    presets_layout->addLayout(preset_buttons);
    layout->addWidget(presets_box, 0, 0, 1, 3);

    int year_row = 100;
    int month_row = 101;
    int day_row = 102;
    int hour_row = 103;
    int minute_row = 104;
    int second_row = 105;

    layout->addWidget(new QLabel("Year", this), year_row, 0);
    this->year = new QSpinBox(this);
    layout->addWidget(year, year_row, 1);

    layout->addWidget(new QLabel("Month", this), month_row, 0);
    this->month = new QSpinBox(this);
    layout->addWidget(month, month_row, 1);

    layout->addWidget(new QLabel("Day", this), day_row, 0);
    this->day = new QSpinBox(this);
    layout->addWidget(day, day_row, 1);
    this->weekday = new QLabel(this);
    layout->addWidget(this->weekday, day_row, 2);

    layout->addWidget(new QLabel("Hour", this), hour_row, 0);
    this->hour = new QSpinBox(this);
    layout->addWidget(hour, hour_row, 1);

    layout->addWidget(new QLabel("Minute", this), minute_row, 0);
    this->minute = new QSpinBox(this);
    layout->addWidget(minute, minute_row, 1);

    layout->addWidget(new QLabel("Second", this), second_row, 0);
    this->second = new QSpinBox(this);
    layout->addWidget(second, second_row, 1);

    layout->setColumnStretch(2, 1);

    QLabel *note;
    bool is_nds = supershuckie_frontend_get_emulator_type(this->main_window->frontend) == SuperShuckieEmulatorType::SuperShuckieEmulatorType__NintendoDS;

    if(is_nds) {
        note = new QLabel("Notes:\n• Changes will apply upon reloading the core or loading a save.\n• Replays and save states will ignore this setting.\n• Gameplay › Reload core with date switches to a preset in one step.", this);
    }
    else {
        note = new QLabel("Notes:\n• Replays and save states will ignore this setting.\n• In a DS game, Gameplay › Reload core with date switches to a preset in one step.", this);
    }

    note->setAttribute(Qt::WA_MacSmallSize);
    layout->addWidget(note, 200, 0, 1, 3);

    auto *buttons = new QHBoxLayout();
    auto *save = new QPushButton("Save and close", this);
    connect(save, SIGNAL(clicked()), this, SLOT(accept()));
    buttons->addWidget(save);
    this->default_button = save;

    if(
        supershuckie_frontend_get_replay_state(this->main_window->frontend) == SuperShuckieReplayState::SuperShuckieReplayState__NoReplay && is_nds
    ) {
        auto *save_reload = new QPushButton("Save and reload core", this);
        connect(save_reload, SIGNAL(clicked()), this, SLOT(save_and_reload()));
        buttons->addWidget(save_reload);
        this->default_button = save_reload;
    }

    this->default_button->setDefault(true);
    layout->addLayout(buttons, 201, 0, 1, 3);

    SuperShuckieNintendoDSDate date = {};
    supershuckie_frontend_get_nds_date(this->main_window->frontend, &date);

    this->year->setMinimum(2000);
    this->year->setMaximum(2099);

    this->month->setMinimum(1);
    this->month->setMaximum(12);

    this->day->setMinimum(1);
    this->day->setMaximum(31);

    this->hour->setMinimum(0);
    this->hour->setMaximum(23);

    this->minute->setMinimum(0);
    this->minute->setMaximum(59);

    this->second->setMinimum(0);
    this->second->setMaximum(59);

    this->enter_date(date);

    this->original_presets = NDSDateDialog::load_presets(this->main_window->frontend);
    for(const auto &preset : this->original_presets) {
        auto *item = new QListWidgetItem(this->presets);
        this->set_preset(item, preset);

        // Show which preset (if any) is the date currently set.
        if(this->presets->selectedItems().isEmpty() && NDSDateDialog::same_date(preset.date, date)) {
            this->presets->setCurrentItem(item);
        }
    }

    for(auto *spin_box : { this->year, this->month, this->day, this->hour, this->minute, this->second }) {
        connect(spin_box, SIGNAL(valueChanged(int)), this, SLOT(on_date_changed()));
    }
    connect(this->presets, SIGNAL(itemSelectionChanged()), this, SLOT(on_preset_selected()));
    // Clicking the selected preset again puts back its date after editing the fields.
    connect(this->presets, SIGNAL(itemClicked(QListWidgetItem *)), this, SLOT(on_preset_selected()));
    connect(this->presets, SIGNAL(itemDoubleClicked(QListWidgetItem *)), this, SLOT(on_preset_double_clicked(QListWidgetItem *)));
    connect(this->add_preset, SIGNAL(clicked()), this, SLOT(on_add_preset()));
    connect(this->update_preset, SIGNAL(clicked()), this, SLOT(on_update_preset()));
    connect(this->rename_preset, SIGNAL(clicked()), this, SLOT(on_rename_preset()));
    connect(this->remove_preset, SIGNAL(clicked()), this, SLOT(on_remove_preset()));

    this->refresh_preset_buttons();

    this->setFixedSize(this->sizeHint());
}

int NDSDateDialog::exec() {
    this->main_window->stop_timer();
    int return_value = QDialog::exec();
    this->main_window->start_timer();
    return return_value;
}

SuperShuckieNintendoDSDate NDSDateDialog::entered_date() const {
    SuperShuckieNintendoDSDate date = {};

    date.year = this->year->value();
    date.month = this->month->value();
    date.day = this->day->value();
    date.hour = this->hour->value();
    date.minute = this->minute->value();
    date.second = this->second->value();

    return date;
}

void NDSDateDialog::enter_date(const SuperShuckieNintendoDSDate &date) {
    {
        // One on_date_changed() for the whole date rather than one per field, some of which
        // would be for impossible dates (e.g. the 31st of the old month).
        const QSignalBlocker blockers[] = {
            QSignalBlocker(this->year), QSignalBlocker(this->month), QSignalBlocker(this->day),
            QSignalBlocker(this->hour), QSignalBlocker(this->minute), QSignalBlocker(this->second)
        };
        this->year->setValue(date.year);
        this->month->setValue(date.month);
        this->day->setMaximum(31);
        this->day->setValue(date.day);
        this->hour->setValue(date.hour);
        this->minute->setValue(date.minute);
        this->second->setValue(date.second);
    }
    this->on_date_changed();
}

void NDSDateDialog::on_date_changed() {
    // Keep the day within the month (as the frontend would anyway), so the weekday shown and
    // what gets saved are the same date.
    this->day->setMaximum(QDate(this->year->value(), this->month->value(), 1).daysInMonth());
    this->weekday->setText(to_qdate(this->entered_date()).toString("dddd"));
    this->refresh_preset_buttons();
}

void NDSDateDialog::set_preset(QListWidgetItem *item, const NDSDatePreset &preset) {
    const auto &date = preset.date;
    item->setData(PRESET_NAME_ROLE, preset.name);
    item->setData(PRESET_DATE_ROLE, QVariantList { int(date.year), int(date.month), int(date.day), int(date.hour), int(date.minute), int(date.second) });
    item->setText(QString("%1 — %2").arg(preset.name, NDSDateDialog::describe_date(date)));
}

NDSDatePreset NDSDateDialog::preset_at(const QListWidgetItem *item) const {
    NDSDatePreset preset = {};
    preset.name = item->data(PRESET_NAME_ROLE).toString();

    auto fields = item->data(PRESET_DATE_ROLE).toList();
    preset.date.year = fields.value(0).toInt();
    preset.date.month = fields.value(1).toInt();
    preset.date.day = fields.value(2).toInt();
    preset.date.hour = fields.value(3).toInt();
    preset.date.minute = fields.value(4).toInt();
    preset.date.second = fields.value(5).toInt();

    return preset;
}

std::vector<NDSDatePreset> NDSDateDialog::current_presets() const {
    std::vector<NDSDatePreset> presets;
    for(int i = 0; i < this->presets->count(); i++) {
        presets.push_back(this->preset_at(this->presets->item(i)));
    }
    return presets;
}

bool NDSDateDialog::presets_changed() const {
    auto presets = this->current_presets();
    if(presets.size() != this->original_presets.size()) {
        return true;
    }
    for(std::size_t i = 0; i < presets.size(); i++) {
        if(presets[i].name != this->original_presets[i].name || !NDSDateDialog::same_date(presets[i].date, this->original_presets[i].date)) {
            return true;
        }
    }
    return false;
}

static QListWidgetItem *selected_item(QListWidget *list) {
    auto selected = list->selectedItems();
    return selected.isEmpty() ? nullptr : selected.first();
}

void NDSDateDialog::refresh_preset_buttons() {
    auto *item = selected_item(this->presets);
    this->update_preset->setEnabled(item != nullptr && !NDSDateDialog::same_date(this->preset_at(item).date, this->entered_date()));
    this->rename_preset->setEnabled(item != nullptr);
    this->remove_preset->setEnabled(item != nullptr);
}

void NDSDateDialog::on_preset_selected() {
    if(auto *item = selected_item(this->presets)) {
        this->enter_date(this->preset_at(item).date);
    }
    this->refresh_preset_buttons();
}

void NDSDateDialog::on_preset_double_clicked(QListWidgetItem *item) {
    this->enter_date(this->preset_at(item).date);
    this->default_button->click();
}

void NDSDateDialog::on_add_preset() {
    NDSDatePreset preset = {};
    preset.date = this->entered_date();

    auto suggestion = QString("%1 %2:%3")
        .arg(to_qdate(preset.date).toString("dddd"))
        .arg(int(preset.date.hour), 2, 10, QChar('0'))
        .arg(int(preset.date.minute), 2, 10, QChar('0'));
    auto name = AskForTextDialog::ask(this->main_window, "Add date preset", "Enter a name for this date", NDSDateDialog::describe_date(preset.date), suggestion);
    if(!name.has_value() || QString::fromStdString(*name).trimmed().isEmpty()) {
        return;
    }
    preset.name = QString::fromStdString(*name).trimmed();

    auto *item = new QListWidgetItem(this->presets);
    this->set_preset(item, preset);
    this->presets->setCurrentItem(item);
    this->refresh_preset_buttons();
}

void NDSDateDialog::on_update_preset() {
    auto *item = selected_item(this->presets);
    if(item == nullptr) {
        return;
    }

    auto preset = this->preset_at(item);
    preset.date = this->entered_date();
    this->set_preset(item, preset);
    this->refresh_preset_buttons();
}

void NDSDateDialog::on_rename_preset() {
    auto *item = selected_item(this->presets);
    if(item == nullptr) {
        return;
    }

    auto preset = this->preset_at(item);
    auto name = AskForTextDialog::ask(this->main_window, "Rename date preset", "Enter a new name for this date", NDSDateDialog::describe_date(preset.date), preset.name);
    if(!name.has_value() || QString::fromStdString(*name).trimmed().isEmpty()) {
        return;
    }
    preset.name = QString::fromStdString(*name).trimmed();
    this->set_preset(item, preset);
}

void NDSDateDialog::on_remove_preset() {
    auto *item = selected_item(this->presets);
    if(item == nullptr) {
        return;
    }

    {
        // Removing the selected row would otherwise select a neighbour and replace the date
        // being edited with its date.
        const QSignalBlocker blocker(this->presets);
        delete this->presets->takeItem(this->presets->row(item));
        this->presets->clearSelection();
    }
    this->refresh_preset_buttons();
}

void NDSDateDialog::accept() {
    auto date = this->entered_date();
    supershuckie_frontend_set_nds_date(this->main_window->frontend, &date);

    if(this->presets_changed()) {
        auto presets = this->current_presets();

        std::vector<QByteArray> names;
        std::vector<SuperShuckieNintendoDSDate> dates;
        for(const auto &preset : presets) {
            names.push_back(preset.name.toUtf8());
            dates.push_back(preset.date);
        }

        std::vector<const char *> name_pointers;
        for(const auto &name : names) {
            name_pointers.push_back(name.constData());
        }

        supershuckie_frontend_set_nds_date_presets(this->main_window->frontend, name_pointers.data(), dates.data(), presets.size());
        supershuckie_frontend_write_settings(this->main_window->frontend);
    }

    if(this->should_reload_core) {
        supershuckie_frontend_reload_core(this->main_window->frontend);
    }

    QDialog::accept();
}

void NDSDateDialog::reject() {
    if(this->presets_changed()) {
        QMessageBox box(this);
        box.setIcon(QMessageBox::Question);
        box.setWindowTitle("Set Nintendo DS date");
        box.setText("Discard your changes to the date presets?");
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

void NDSDateDialog::save_and_reload() {
    this->should_reload_core = true;
    this->accept();
}
