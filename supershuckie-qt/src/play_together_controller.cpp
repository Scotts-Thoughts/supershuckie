#include "play_together_controller.hpp"
#include "play_together_dialog.hpp"
#include "peer_window.hpp"
#include "screen_canvas.hpp"
#include "landing_widget.hpp"
#include "main_window.hpp"

#include <QFileDialog>
#include <QJsonArray>
#include <QJsonDocument>
#include <QLabel>
#include <QMessageBox>
#include <QPushButton>
#include <QStatusBar>

using namespace SuperShuckie64;

PlayTogetherController::PlayTogetherController(MainWindow *main_window): QObject(main_window), main(main_window) {
    this->status_label = new QLabel(main_window->status_bar);
    this->status_label->setToolTip("Play Together (click for the session window)");
    main_window->status_bar->addPermanentWidget(this->status_label);
    this->status_label->hide();

    // The favourites are ROMs the user has on hand: good places to look for another player's ROM.
    QJsonArray candidates;
    for(const auto &path : main_window->landing_widget->favorite_paths()) {
        candidates.append(path);
    }
    if(!candidates.isEmpty()) {
        supershuckie_frontend_play_together_add_rom_candidates_json(main_window->frontend, QJsonDocument(candidates).toJson(QJsonDocument::Compact).constData());
    }
}

PlayTogetherController::~PlayTogetherController() {
    for(auto &[id, window] : this->windows) {
        delete window;
    }
    this->windows.clear();
}

QJsonObject PlayTogetherController::read_state() const {
    char *json = supershuckie_frontend_play_together_state_json(this->main->frontend);
    auto state = QJsonDocument::fromJson(QByteArray(json)).object();
    supershuckie_string_free(json);
    return state;
}

bool PlayTogetherController::is_active() const {
    return this->main->frontend != nullptr && supershuckie_frontend_play_together_is_active(this->main->frontend);
}

bool PlayTogetherController::is_host() const {
    return this->last_state["role"].toString() == "host";
}

QJsonObject PlayTogetherController::participant(std::uint16_t peer) const {
    for(auto value : this->last_state["participants"].toArray()) {
        auto object = value.toObject();
        if(static_cast<std::uint16_t>(object["peer_id"].toInt()) == peer) {
            return object;
        }
    }
    return QJsonObject();
}

void PlayTogetherController::tick() {
    if(this->main->frontend == nullptr) {
        return;
    }

    bool active = this->is_active();
    auto generation = supershuckie_frontend_play_together_generation(this->main->frontend);
    bool roster_changed = generation != this->last_generation || active != this->was_active;
    bool strip_due = false;
    if(active) {
        // The numbers change every frame; a few times a second is plenty for a status strip.
        if(--this->strip_countdown <= 0) {
            this->strip_countdown = 250;
            strip_due = true;
        }
    }
    if(!roster_changed && !strip_due && !active) {
        return;
    }
    if(!roster_changed && !strip_due) {
        // Only the countdown label needs refreshing every tick.
        if(supershuckie_frontend_play_together_reset_countdown_ms(this->main->frontend) > 0) {
            this->apply_state(this->last_state, false);
        }
        return;
    }

    this->last_generation = generation;
    this->was_active = active;
    this->last_state = this->read_state();
    this->apply_state(this->last_state, roster_changed);
}

void PlayTogetherController::apply_state(const QJsonObject &state, bool roster_changed) {
    bool active = state["active"].toBool();
    auto participants = state["participants"].toArray();

    // Windows of players who left go; the rest get their strip refreshed.
    QSet<std::uint16_t> present;
    for(auto value : participants) {
        auto object = value.toObject();
        auto id = static_cast<std::uint16_t>(object["peer_id"].toInt());
        present.insert(id);
        auto it = this->windows.find(id);
        if(it != this->windows.end()) {
            it->second->update(object);
        }
        if(roster_changed && object["status"].toString() == "needs_rom" && !this->prompted_for_rom.contains(id)) {
            this->prompted_for_rom.insert(id);
            this->locate_rom_for(id);
        }
    }
    for(auto it = this->windows.begin(); it != this->windows.end();) {
        if(!present.contains(it->first)) {
            it->second->save_geometry();
            it->second->deleteLater();
            it = this->windows.erase(it);
        }
        else {
            ++it;
        }
    }
    if(!active) {
        this->prompted_for_rom.clear();
    }

    // Status bar.
    if(active) {
        auto countdown = supershuckie_frontend_play_together_reset_countdown_ms(this->main->frontend);
        QString text;
        if(countdown > 0) {
            text = QString("RESET IN %1 ").arg((countdown + 999) / 1000);
        }
        else {
            int others = participants.size();
            text = QString("Play Together: %1 %2 ").arg(others).arg(others == 1 ? "friend" : "friends");
        }
        this->status_label->setText(text);
        this->status_label->show();
    }
    else {
        this->status_label->hide();
    }

    if(this->dialog != nullptr) {
        this->dialog->refresh(state);
    }

    if(roster_changed) {
        this->main->refresh_action_states();
        for(const auto &error : state["errors"].toArray()) {
            (void) error;
        }
    }
}

void PlayTogetherController::open_dialog() {
    if(this->dialog == nullptr) {
        this->dialog = new PlayTogetherDialog(this);
    }
    this->dialog->refresh(this->last_state);
    this->dialog->show();
    this->dialog->raise();
    this->dialog->activateWindow();
}

void PlayTogetherController::leave() {
    this->save_windows();
    supershuckie_frontend_play_together_leave(this->main->frontend);
    this->tick();
}

void PlayTogetherController::reset_all() {
    char error[512] = {};
    if(!supershuckie_frontend_play_together_reset_all(this->main->frontend, 3, reinterpret_cast<uint8_t *>(error), sizeof(error))) {
        this->main->show_error("Reset everyone", "%s", error);
    }
}

void PlayTogetherController::show_windows() {
    for(auto &[id, window] : this->windows) {
        window->show();
        window->raise();
        supershuckie_frontend_play_together_set_window_hidden(this->main->frontend, id, false);
    }
}

void PlayTogetherController::set_scale(std::uint8_t scale) {
    supershuckie_frontend_play_together_set_video_scale(this->main->frontend, 0, scale);
}

bool PlayTogetherController::confirm_leave(const char *because) {
    if(!this->is_active()) {
        return true;
    }
    QMessageBox box(this->main);
    box.setWindowTitle("Leave Play Together?");
    box.setIcon(QMessageBox::Icon::Question);
    box.setText(QString("You're in a Play Together session. %1 will leave it; the other players' games close and their replay files are finished.").arg(because));
    QPushButton *leave = box.addButton("Leave", QMessageBox::AcceptRole);
    box.addButton("Cancel", QMessageBox::RejectRole);
    box.setDefaultButton(leave);
    this->main->stop_timer();
    box.exec();
    this->main->start_timer();
    return box.clickedButton() == static_cast<QAbstractButton *>(leave);
}

void PlayTogetherController::locate_rom_for(std::uint16_t peer) {
    auto object = this->participant(peer);
    if(object.isEmpty()) {
        object = this->read_state();
        this->last_state = object;
        object = this->participant(peer);
        if(object.isEmpty()) {
            return;
        }
    }
    QString name = object["name"].toString();
    QString rom = object["rom_name"].toString();
    QString console = object["console"].toString();

    QString filter;
    if(console.startsWith("Game Boy Advance")) {
        filter = "GBA ROM dumps (*.gba)";
    }
    else if(console.startsWith("Game Boy")) {
        filter = "GB/GBC ROM dumps (*.gb *.gbc)";
    }
    else if(console.startsWith("Nintendo DS")) {
        filter = "NDS ROM files (*.nds)";
    }
    else {
        filter = "Any files (*)";
    }

    while(true) {
        QMessageBox box(this->main);
        box.setWindowTitle("Locate ROM");
        box.setIcon(QMessageBox::Icon::Information);
        box.setText(QString("%1 is playing \"%2\" (%3). To follow their game, choose your copy of the same ROM.").arg(name, rom, console));
        QPushButton *choose = box.addButton("Choose file…", QMessageBox::AcceptRole);
        box.addButton("Not now", QMessageBox::RejectRole);
        box.setDefaultButton(choose);
        this->main->stop_timer();
        box.exec();
        this->main->start_timer();
        if(box.clickedButton() != static_cast<QAbstractButton *>(choose)) {
            return;
        }

        QFileDialog opener(this->main);
        opener.setFileMode(QFileDialog::FileMode::ExistingFile);
        opener.setNameFilters(QStringList({ filter, "Any files (*)" }));
        opener.setWindowTitle(QString("Select %1's ROM: %2").arg(name, rom));
        this->main->stop_timer();
        opener.exec();
        this->main->start_timer();
        auto files = opener.selectedFiles();
        if(files.size() != 1) {
            return;
        }

        char error[1024] = {};
        auto path = files[0].toUtf8();
        if(supershuckie_frontend_play_together_locate_rom(this->main->frontend, peer, path.constData(), reinterpret_cast<uint8_t *>(error), sizeof(error))) {
            return;
        }
        this->main->show_error("Wrong ROM", "%s", error);
    }
}

void PlayTogetherController::save_windows() {
    for(auto &[id, window] : this->windows) {
        window->save_geometry();
    }
}

void PlayTogetherController::on_peer_refresh_screens(void *user_data, std::uint16_t peer, std::size_t screen_count, const uint32_t *const *pixels) {
    auto *self = reinterpret_cast<MainWindow *>(user_data)->play_together;
    if(self == nullptr) {
        return;
    }
    auto it = self->windows.find(peer);
    if(it == self->windows.end()) {
        return;
    }
    it->second->canvas()->refresh_screen(static_cast<unsigned>(screen_count), pixels);
}

void PlayTogetherController::on_peer_change_video_mode(void *user_data, std::uint16_t peer, std::size_t screen_count, const SuperShuckieScreenData *screen_data, std::uint8_t scaling) {
    auto *self = reinterpret_cast<MainWindow *>(user_data)->play_together;
    if(self == nullptr) {
        return;
    }
    auto it = self->windows.find(peer);
    PeerWindow *window;
    bool fresh = false;
    if(it == self->windows.end()) {
        // Read-only getters are allowed here (no lock is held): fetch the participant's name.
        self->last_state = self->read_state();
        auto object = self->participant(peer);
        window = new PeerWindow(self, peer, object);
        self->windows.emplace(peer, window);
        fresh = true;
    }
    else {
        window = it->second;
    }
    window->set_layout(static_cast<unsigned>(screen_count), screen_data, scaling);
    if(fresh) {
        window->restore_geometry();
        window->show();
    }
}
