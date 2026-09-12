#include <QCheckBox>
#include <QComboBox>
#include <QDialogButtonBox>
#include <QFormLayout>
#include <QHBoxLayout>
#include <QJsonArray>
#include <QJsonDocument>
#include <QLabel>
#include <QLineEdit>
#include <QSpinBox>
#include <QVBoxLayout>
#include <algorithm>
#include <set>

#include "watch_edit_dialog.hpp"
#include "memory_tools_controller.hpp"
#include "main_window.hpp"

using namespace SuperShuckie64;

namespace {
    const char *TYPE_NAMES[] = { "u8", "i8", "u16", "i16", "u32", "i32", "f32", "bcd", "bytes", "text" };
    const char *TYPE_LABELS[] = { "u8", "i8", "u16", "i16", "u32", "i32", "f32", "BCD", "Bytes", "Text" };

    struct PauseInfo {
        const char *when;
        const char *label;
        bool value;
    };
    const PauseInfo PAUSES[] = {
        { nullptr, "Never", false },
        { "changes", "When it changes", false },
        { "equals", "When it becomes", true },
        { "not_equals", "When it stops being", true },
        { "greater_than", "When it rises above", true },
        { "less_than", "When it falls below", true },
        { "increased_by", "When it goes up by", true },
        { "decreased_by", "When it goes down by", true },
    };

    int type_index(const QString &name) {
        for(int i = 0; i < 10; i++) {
            if(name == TYPE_NAMES[i]) {
                return i;
            }
        }
        return 0;
    }

    int fixed_size(int type) {
        switch(type) {
            case 0: case 1: return 1;
            case 2: case 3: return 2;
            case 4: case 5: case 6: return 4;
            default: return 0;
        }
    }
}

QJsonObject WatchEditDialog::new_watch(std::uint32_t address, std::uint32_t value_type, std::uint8_t size, bool big_endian, const QString &label) {
    QJsonObject format;
    format["type"] = TYPE_NAMES[std::min<std::uint32_t>(value_type, 9)];
    format["size"] = size;
    format["big_endian"] = big_endian;
    QJsonObject address_object;
    address_object["base"] = QString::asprintf("0x%08X", address);
    QJsonObject watch;
    watch["id"] = 0;
    watch["label"] = label;
    watch["address"] = address_object;
    watch["format"] = format;
    watch["display"] = "decimal";
    return watch;
}

WatchEditDialog::WatchEditDialog(MemoryToolsController *controller, QWidget *parent, const QJsonObject &watch): QDialog(parent), controller(controller), watch(watch) {
    bool adding = watch["id"].toInt() == 0;
    this->setWindowTitle(adding ? "Add watch" : "Edit watch");

    auto *layout = new QVBoxLayout(this);
    auto *form = new QFormLayout();
    layout->addLayout(form);

    this->label_edit = new QLineEdit(watch["label"].toString(), this);
    form->addRow("Label", this->label_edit);

    auto *address_row = new QHBoxLayout();
    this->address_edit = new QLineEdit(this);
    this->address_edit->setToolTip("0x02024284, EWRAM:24284, or a pointer path like [0x02101D2C]+0xC or [[EWRAM:1D2C]+4]+8 (offsets in hexadecimal)");
    {
        QByteArray json = QJsonDocument(watch["address"].toObject()).toJson(QJsonDocument::Compact);
        char text[256];
        supershuckie_frontend_watch_format_address(controller->frontend(), json.constData(), text, sizeof(text));
        this->address_edit->setText(QString::fromUtf8(text));
    }
    address_row->addWidget(this->address_edit, 1);
    this->region_combo = new QComboBox(this);
    this->region_combo->addItem("Region…");
    for(auto &region : controller->regions()) {
        this->region_combo->addItem(region.short_name);
    }
    this->region_combo->setToolTip("Rewrite the address as an offset into a region");
    address_row->addWidget(this->region_combo);
    form->addRow("Address", address_row);

    auto *type_row = new QHBoxLayout();
    this->type_combo = new QComboBox(this);
    for(auto *label : TYPE_LABELS) {
        this->type_combo->addItem(label);
    }
    auto format = watch["format"].toObject();
    this->type_combo->setCurrentIndex(type_index(format["type"].toString()));
    type_row->addWidget(this->type_combo);
    type_row->addWidget(new QLabel("Size", this));
    this->size_spin = new QSpinBox(this);
    this->size_spin->setRange(1, SUPERSHUCKIE_MEMORY_MAX_VALUE_SIZE);
    this->size_spin->setValue(std::max(1, format["size"].toInt()));
    type_row->addWidget(this->size_spin);
    this->endian_combo = new QComboBox(this);
    this->endian_combo->addItems({ "Little-endian", "Big-endian" });
    this->endian_combo->setCurrentIndex(format["big_endian"].toBool() ? 1 : 0);
    type_row->addWidget(this->endian_combo);
    type_row->addStretch(1);
    form->addRow("Type", type_row);

    auto *display_row = new QHBoxLayout();
    this->display_combo = new QComboBox(this);
    this->display_combo->addItems({ "Decimal", "Hexadecimal", "Binary" });
    QString display = watch["display"].toString();
    this->display_combo->setCurrentIndex(display == "hex" ? 1 : display == "binary" ? 2 : 0);
    display_row->addWidget(this->display_combo);
    display_row->addWidget(new QLabel("Table", this));
    this->table_combo = new QComboBox(this);
    this->table_combo->addItems(controller->table_names());
    int table = this->table_combo->findText(watch["table"].toString());
    this->table_combo->setCurrentIndex(table >= 0 ? table : 0);
    display_row->addWidget(this->table_combo);
    display_row->addStretch(1);
    form->addRow("Show as", display_row);

    this->group_combo = new QComboBox(this);
    this->group_combo->setEditable(true);
    {
        std::set<QString> groups;
        char *list = supershuckie_frontend_watch_list_json(controller->frontend());
        auto array = QJsonDocument::fromJson(QByteArray(list)).array();
        supershuckie_string_free(list);
        for(auto value : array) {
            auto group = value.toObject()["group"].toString();
            if(!group.isEmpty()) {
                groups.insert(group);
            }
        }
        this->group_combo->addItem("");
        for(auto &group : groups) {
            this->group_combo->addItem(group);
        }
        this->group_combo->setCurrentText(watch["group"].toString());
    }
    form->addRow("Group", this->group_combo);

    this->notes_edit = new QLineEdit(watch["notes"].toString(), this);
    form->addRow("Notes", this->notes_edit);

    this->trace_check = new QCheckBox("Log every change (checked every frame)", this);
    this->trace_check->setChecked(watch["trace"].toBool());
    form->addRow("", this->trace_check);

    auto *pause_row = new QHBoxLayout();
    this->pause_combo = new QComboBox(this);
    for(auto &pause : PAUSES) {
        this->pause_combo->addItem(pause.label);
    }
    this->pause_value = new QLineEdit(this);
    this->pause_value->setPlaceholderText("value");
    auto pause_when = watch["pause_when"].toObject();
    for(int i = 1; i < static_cast<int>(sizeof(PAUSES) / sizeof(PAUSES[0])); i++) {
        if(pause_when["when"].toString() == PAUSES[i].when) {
            this->pause_combo->setCurrentIndex(i);
            this->pause_value->setText(QString::number(pause_when["value"].toInteger()));
        }
    }
    pause_row->addWidget(this->pause_combo);
    pause_row->addWidget(this->pause_value, 1);
    form->addRow("Pause emulation", pause_row);

    auto *freeze_row = new QHBoxLayout();
    this->freeze_check = new QCheckBox("Freeze at", this);
    this->freeze_value = new QLineEdit(this);
    this->freeze_value->setPlaceholderText("value to hold");
    auto freeze = watch["freeze"].toObject();
    if(!freeze.isEmpty()) {
        this->freeze_check->setChecked(freeze["active"].toBool());
        std::uint8_t bytes[SUPERSHUCKIE_MEMORY_MAX_VALUE_SIZE];
        std::size_t length = 0;
        char ignored[64];
        if(supershuckie_memory_parse_hex_bytes(freeze["value"].toString().toUtf8().constData(), bytes, sizeof(bytes), &length, ignored, sizeof(ignored))) {
            int type = type_index(format["type"].toString());
            QString display = watch["display"].toString();
            std::uint32_t display_kind = display == "hex" ? SuperShuckieMemoryDisplay__Hex : display == "binary" ? SuperShuckieMemoryDisplay__Binary : SuperShuckieMemoryDisplay__Decimal;
            std::size_t table_index = 0;
            auto names = controller->table_names();
            if(names.contains(watch["table"].toString())) {
                table_index = static_cast<std::size_t>(names.indexOf(watch["table"].toString()));
            }
            QString text = controller->format_value(table_index, static_cast<std::uint32_t>(type), static_cast<std::uint8_t>(length), format["big_endian"].toBool(), type == SuperShuckieMemoryValueType__Bytes || type == SuperShuckieMemoryValueType__Text ? 0 : display_kind, bytes, length);
            this->freeze_value->setText(text.remove(QChar(0xB7)));
        }
    }
    freeze_row->addWidget(this->freeze_check);
    freeze_row->addWidget(this->freeze_value, 1);
    this->freeze_row = new QWidget(this);
    this->freeze_row->setLayout(freeze_row);
    form->addRow("", this->freeze_row);

    this->error_label = new QLabel(this);
    this->error_label->setWordWrap(true);
    this->error_label->setStyleSheet("color: #d04040");
    layout->addWidget(this->error_label);

    auto *buttons = new QDialogButtonBox(QDialogButtonBox::Ok | QDialogButtonBox::Cancel, this);
    layout->addWidget(buttons);
    connect(buttons, SIGNAL(accepted()), this, SLOT(accept()));
    connect(buttons, SIGNAL(rejected()), this, SLOT(reject()));
    connect(this->type_combo, SIGNAL(currentIndexChanged(int)), this, SLOT(on_type_changed()));
    connect(this->region_combo, SIGNAL(activated(int)), this, SLOT(on_region_chosen(int)));
    connect(this->pause_combo, SIGNAL(currentIndexChanged(int)), this, SLOT(on_pause_changed()));
    connect(this->freeze_check, SIGNAL(toggled(bool)), this, SLOT(on_freeze_toggled()));

    this->on_type_changed();
    this->on_pause_changed();
    this->on_freeze_toggled();
    this->resize(520, this->sizeHint().height());
}

std::uint32_t WatchEditDialog::selected_type() const {
    return static_cast<std::uint32_t>(std::max(0, this->type_combo->currentIndex()));
}

void WatchEditDialog::on_type_changed() {
    auto type = this->selected_type();
    int fixed = fixed_size(static_cast<int>(type));
    this->size_spin->setEnabled(fixed == 0);
    this->size_spin->setRange(1, type == SuperShuckieMemoryValueType__BCD ? 4 : SUPERSHUCKIE_MEMORY_MAX_VALUE_SIZE);
    if(fixed != 0) {
        this->size_spin->setValue(fixed);
    }
    bool has_order = type != SuperShuckieMemoryValueType__U8 && type != SuperShuckieMemoryValueType__I8 && type != SuperShuckieMemoryValueType__Bytes && type != SuperShuckieMemoryValueType__Text;
    this->endian_combo->setEnabled(has_order);
    this->table_combo->setEnabled(type == SuperShuckieMemoryValueType__Text);
    this->display_combo->setEnabled(type != SuperShuckieMemoryValueType__Bytes && type != SuperShuckieMemoryValueType__Text);
}

void WatchEditDialog::on_region_chosen(int index) {
    if(index <= 0) {
        return;
    }
    auto &regions = this->controller->regions();
    auto &region = regions[index - 1];
    QString error;
    auto address = this->controller->parse_address(this->address_edit->text(), &error);
    std::uint32_t offset = address && region.contains(*address) ? *address - region.base : 0;
    this->address_edit->setText(QString("%1:%2").arg(region.short_name, QString::number(offset, 16).toUpper()));
    this->region_combo->setCurrentIndex(0);
}

void WatchEditDialog::on_pause_changed() {
    int index = std::max(0, this->pause_combo->currentIndex());
    this->pause_value->setEnabled(PAUSES[index].value);
}

void WatchEditDialog::on_freeze_toggled() {
    this->freeze_value->setEnabled(this->freeze_check->isChecked());
}

void WatchEditDialog::accept() {
    auto frontend = this->controller->frontend();
    char error[512] = {};

    char *address_json = supershuckie_frontend_watch_parse_address(frontend, this->address_edit->text().toUtf8().constData(), error, sizeof(error));
    if(address_json == nullptr) {
        this->error_label->setText(QString("Address: %1").arg(QString::fromUtf8(error)));
        return;
    }
    auto address = QJsonDocument::fromJson(QByteArray(address_json)).object();
    supershuckie_string_free(address_json);

    auto type = this->selected_type();
    QJsonObject format;
    format["type"] = TYPE_NAMES[type];
    format["size"] = this->size_spin->value();
    format["big_endian"] = this->endian_combo->isEnabled() && this->endian_combo->currentIndex() == 1;

    QJsonObject result = this->watch;
    result["label"] = this->label_edit->text().trimmed().isEmpty() ? this->address_edit->text().trimmed() : this->label_edit->text().trimmed();
    result["address"] = address;
    result["format"] = format;
    static const char *DISPLAYS[] = { "decimal", "hex", "binary" };
    result["display"] = DISPLAYS[std::clamp(this->display_combo->currentIndex(), 0, 2)];
    result["table"] = type == SuperShuckieMemoryValueType__Text && this->table_combo->currentIndex() > 0 ? this->table_combo->currentText() : QString();
    result["group"] = this->group_combo->currentText().trimmed();
    result["notes"] = this->notes_edit->text();
    result["trace"] = this->trace_check->isChecked();

    int pause = std::max(0, this->pause_combo->currentIndex());
    if(pause == 0) {
        result.remove("pause_when");
    }
    else {
        QJsonObject pause_when;
        pause_when["when"] = PAUSES[pause].when;
        if(PAUSES[pause].value) {
            bool ok = false;
            QString text = this->pause_value->text().trimmed();
            qlonglong value = text.startsWith("0x", Qt::CaseInsensitive) ? text.mid(2).toLongLong(&ok, 16) : text.toLongLong(&ok, 10);
            if(!ok) {
                this->error_label->setText("Pause emulation: enter a whole number (decimal or 0x hexadecimal)");
                return;
            }
            pause_when["value"] = value;
        }
        result["pause_when"] = pause_when;
    }

    if(this->freeze_check->isChecked()) {
        std::uint8_t bytes[SUPERSHUCKIE_MEMORY_MAX_VALUE_SIZE];
        std::size_t length = 0;
        std::size_t table = static_cast<std::size_t>(std::max(0, this->table_combo->currentIndex()));
        if(!supershuckie_frontend_memory_parse_value(frontend, table, type, static_cast<std::uint8_t>(this->size_spin->value()), format["big_endian"].toBool(), this->freeze_value->text().toUtf8().constData(), bytes, sizeof(bytes), &length, error, sizeof(error))) {
            this->error_label->setText(QString("Freeze: %1").arg(QString::fromUtf8(error)));
            return;
        }
        // Text shorter than the watch is padded with zeros.
        std::size_t size = static_cast<std::size_t>(this->size_spin->value());
        QString hex;
        for(std::size_t i = 0; i < size; i++) {
            hex += QString::asprintf(i == 0 ? "%02X" : " %02X", i < length ? bytes[i] : 0);
        }
        QJsonObject freeze;
        freeze["value"] = hex;
        freeze["active"] = true;
        result["freeze"] = freeze;
    }
    else if(result.contains("freeze")) {
        auto freeze = result["freeze"].toObject();
        freeze["active"] = false;
        result["freeze"] = freeze;
    }

    QByteArray json = QJsonDocument(result).toJson(QJsonDocument::Compact);
    std::uint32_t id = this->controller->upsert_watch(json, error, sizeof(error));
    if(id == 0) {
        this->error_label->setText(QString::fromUtf8(error));
        return;
    }
    this->watch = result;
    this->watch["id"] = static_cast<qint64>(id);
    QDialog::accept();
}

std::optional<std::uint32_t> WatchEditDialog::edit(MemoryToolsController *controller, QWidget *parent, const QJsonObject &watch) {
    WatchEditDialog dialog(controller, parent, watch);
    if(dialog.exec() != QDialog::Accepted) {
        return std::nullopt;
    }
    return static_cast<std::uint32_t>(dialog.watch["id"].toInteger());
}
