#include "play_together_dialog.hpp"
#include "play_together_controller.hpp"
#include "main_window.hpp"

#include <QApplication>
#include <QCheckBox>
#include <QClipboard>
#include <QFormLayout>
#include <QHeaderView>
#include <QJsonArray>
#include <QJsonDocument>
#include <QLabel>
#include <QLineEdit>
#include <QPushButton>
#include <QSpinBox>
#include <QTabWidget>
#include <QTableWidget>
#include <QVBoxLayout>
#include <QHBoxLayout>

using namespace SuperShuckie64;

static QString read_string(size_t (*getter)(const SuperShuckieFrontendRaw *, uint8_t *, size_t), const SuperShuckieFrontendRaw *frontend) {
    char buf[256] = {};
    getter(frontend, reinterpret_cast<uint8_t *>(buf), sizeof(buf));
    return QString::fromUtf8(buf);
}

PlayTogetherDialog::PlayTogetherDialog(PlayTogetherController *controller): QDialog(controller->main_window()), controller(controller) {
    this->setWindowTitle("Play Together");
    this->setModal(false);
    auto *frontend = controller->main_window()->frontend;

    auto *layout = new QVBoxLayout(this);

    this->tabs = new QTabWidget(this);
    layout->addWidget(this->tabs);

    // Host tab
    auto *host_tab = new QWidget(this->tabs);
    auto *host_layout = new QFormLayout(host_tab);
    this->host_name = new QLineEdit(read_string(supershuckie_frontend_play_together_get_display_name, frontend), host_tab);
    this->host_name->setMaxLength(32);
    host_layout->addRow("Your name", this->host_name);
    this->host_port = new QSpinBox(host_tab);
    this->host_port->setRange(1, 65535);
    this->host_port->setValue(supershuckie_frontend_play_together_get_host_port(frontend));
    host_layout->addRow("Port", this->host_port);
    this->host_button = new QPushButton("Start hosting", host_tab);
    connect(this->host_button, SIGNAL(clicked()), this, SLOT(do_host()));
    host_layout->addRow(this->host_button);
    auto *code_row = new QWidget(host_tab);
    auto *code_layout = new QHBoxLayout(code_row);
    code_layout->setContentsMargins(0, 0, 0, 0);
    this->code_label = new QLabel(code_row);
    this->code_label->setTextInteractionFlags(Qt::TextSelectableByMouse);
    QFont big = this->code_label->font();
    big.setPointSize(big.pointSize() + 4);
    big.setBold(true);
    this->code_label->setFont(big);
    code_layout->addWidget(this->code_label, 1);
    this->copy_button = new QPushButton("Copy", code_row);
    this->copy_button->setEnabled(false);
    connect(this->copy_button, SIGNAL(clicked()), this, SLOT(do_copy_code()));
    code_layout->addWidget(this->copy_button);
    host_layout->addRow("Code to share", code_row);
    this->addresses_label = new QLabel(host_tab);
    this->addresses_label->setWordWrap(true);
    this->addresses_label->setText("The code is your address on the local network. Players elsewhere on the internet need your public address with TCP port forwarded to this machine, or a VPN such as Tailscale. Share it only with people you trust.");
    host_layout->addRow(this->addresses_label);
    this->tabs->addTab(host_tab, "Host");

    // Join tab
    auto *join_tab = new QWidget(this->tabs);
    auto *join_layout = new QFormLayout(join_tab);
    this->join_name = new QLineEdit(read_string(supershuckie_frontend_play_together_get_display_name, frontend), join_tab);
    this->join_name->setMaxLength(32);
    join_layout->addRow("Your name", this->join_name);
    this->join_code = new QLineEdit(read_string(supershuckie_frontend_play_together_get_last_join_code, frontend), join_tab);
    this->join_code->setPlaceholderText("host:port");
    join_layout->addRow("Code", this->join_code);
    this->join_button = new QPushButton("Join", join_tab);
    connect(this->join_button, SIGNAL(clicked()), this, SLOT(do_join()));
    join_layout->addRow(this->join_button);
    this->tabs->addTab(join_tab, "Join");

    // Session
    this->session_label = new QLabel(this);
    layout->addWidget(this->session_label);

    this->participants = new QTableWidget(0, 6, this);
    this->participants->setHorizontalHeaderLabels(QStringList({ "Player", "ROM", "Console", "Status", "Behind", "Replay file" }));
    this->participants->horizontalHeader()->setStretchLastSection(true);
    this->participants->verticalHeader()->hide();
    this->participants->setEditTriggers(QAbstractItemView::NoEditTriggers);
    this->participants->setSelectionMode(QAbstractItemView::NoSelection);
    this->participants->setMinimumHeight(140);
    layout->addWidget(this->participants);

    auto *buttons = new QWidget(this);
    auto *buttons_layout = new QHBoxLayout(buttons);
    buttons_layout->setContentsMargins(0, 0, 0, 0);
    this->save_replays = new QCheckBox("Save friends' games as replays", buttons);
    this->save_replays->setChecked(supershuckie_frontend_play_together_get_save_peer_replays(frontend));
    connect(this->save_replays, SIGNAL(toggled(bool)), this, SLOT(do_toggle_save_replays(bool)));
    buttons_layout->addWidget(this->save_replays);
    buttons_layout->addStretch(1);
    this->reset_button = new QPushButton("Reset everyone (3 s)", buttons);
    this->reset_button->setToolTip("Race start: every player's console resets after a 3 second countdown");
    connect(this->reset_button, SIGNAL(clicked()), this, SLOT(do_reset_all()));
    buttons_layout->addWidget(this->reset_button);
    this->leave_button = new QPushButton("Leave", buttons);
    connect(this->leave_button, SIGNAL(clicked()), this, SLOT(do_leave()));
    buttons_layout->addWidget(this->leave_button);
    layout->addWidget(buttons);

    this->errors_label = new QLabel(this);
    this->errors_label->setWordWrap(true);
    layout->addWidget(this->errors_label);

    this->resize(640, 420);
    this->refresh(controller->read_state());
}

void PlayTogetherDialog::refresh(const QJsonObject &state) {
    bool active = state["active"].toBool();
    QString role = state["role"].toString();
    bool host = role == "host";

    this->host_button->setEnabled(!active);
    this->host_name->setEnabled(!active);
    this->host_port->setEnabled(!active);
    this->join_button->setEnabled(!active);
    this->join_name->setEnabled(!active);
    this->join_code->setEnabled(!active);
    this->reset_button->setEnabled(active && host);
    this->leave_button->setEnabled(active);
    this->leave_button->setText(host ? "Stop hosting" : "Leave");

    if(active) {
        this->shown_code = state["code"].toString();
        this->code_label->setText(host ? this->shown_code : QString());
        this->copy_button->setEnabled(host);
        if(role == "connecting") {
            this->session_label->setText(QString("Connecting to %1…").arg(state["code"].toString()));
        }
        else {
            this->session_label->setText(QString("%1 as <b>%2</b> (%3)").arg(host ? "Hosting" : "Joined", state["local_name"].toString().toHtmlEscaped(), state["code"].toString().toHtmlEscaped()));
        }
    }
    else {
        this->shown_code.clear();
        this->code_label->setText("(not hosting)");
        this->copy_button->setEnabled(false);
        this->session_label->setText("Not in a session. Load your game, then host or join.");
    }

    auto participants = state["participants"].toArray();
    this->participants->setRowCount(participants.size());
    int row = 0;
    for(auto value : participants) {
        auto p = value.toObject();
        QString status = p["status"].toString();
        QString status_text = status;
        auto mismatches = static_cast<int>(p["hash_mismatches"].toDouble());
        if(status == "error" || status == "needs_rom") {
            status_text = QString("%1: %2").arg(status, p["status_text"].toString());
        }
        else if(mismatches > 0) {
            status_text = QString("%1 (desynced ×%2)").arg(status).arg(mismatches);
        }
        const QString cells[6] = {
            p["name"].toString(),
            p["rom_name"].toString(),
            p["console"].toString(),
            status_text,
            QString::number(static_cast<long long>(p["frames_behind"].toDouble())),
            p["replay_file"].isString() ? p["replay_file"].toString() : QString("—")
        };
        for(int column = 0; column < 6; column++) {
            auto *item = this->participants->item(row, column);
            if(item == nullptr) {
                item = new QTableWidgetItem();
                this->participants->setItem(row, column, item);
            }
            item->setText(cells[column]);
        }
        row++;
    }
    this->participants->resizeColumnsToContents();

    QStringList errors;
    for(auto e : state["errors"].toArray()) {
        errors.append(e.toString());
    }
    this->errors_label->setText(errors.join("\n"));
    this->errors_label->setVisible(!errors.isEmpty());
}

void PlayTogetherDialog::do_host() {
    auto *frontend = this->controller->main_window()->frontend;
    char code[128] = {};
    char error[1024] = {};
    auto name = this->host_name->text().toUtf8();
    if(!supershuckie_frontend_play_together_host(frontend, static_cast<uint16_t>(this->host_port->value()), name.constData(), reinterpret_cast<uint8_t *>(code), sizeof(code), reinterpret_cast<uint8_t *>(error), sizeof(error))) {
        this->controller->main_window()->show_error("Can't host", "%s", error);
        return;
    }
    this->controller->tick();
}

void PlayTogetherDialog::do_join() {
    auto *frontend = this->controller->main_window()->frontend;
    char error[1024] = {};
    auto name = this->join_name->text().toUtf8();
    auto code = this->join_code->text().trimmed().toUtf8();
    if(!supershuckie_frontend_play_together_join(frontend, code.constData(), name.constData(), reinterpret_cast<uint8_t *>(error), sizeof(error))) {
        this->controller->main_window()->show_error("Can't join", "%s", error);
        return;
    }
    this->controller->tick();
}

void PlayTogetherDialog::do_leave() {
    this->controller->leave();
}

void PlayTogetherDialog::do_reset_all() {
    this->controller->reset_all();
}

void PlayTogetherDialog::do_copy_code() {
    if(!this->shown_code.isEmpty()) {
        QApplication::clipboard()->setText(this->shown_code);
    }
}

void PlayTogetherDialog::do_toggle_save_replays(bool on) {
    supershuckie_frontend_play_together_set_save_peer_replays(this->controller->main_window()->frontend, on);
}
