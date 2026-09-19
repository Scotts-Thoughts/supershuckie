#include "play_together_controller.hpp"
#include "play_together_dialog.hpp"
#include "peer_window.hpp"
#include "screen_canvas.hpp"
#include "landing_widget.hpp"
#include "main_window.hpp"

#include <QFileDialog>
#include <QJsonArray>
#include <QRect>
#include <QScreen>
#include <cstdlib>
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
    this->close_link_prompt();
    for(auto &[id, window] : this->windows) {
        delete window;
    }
    this->windows.clear();
}

bool PlayTogetherController::is_link_cable_plugged() const {
    return this->main->frontend != nullptr && supershuckie_frontend_play_together_is_link_cable_plugged(this->main->frontend);
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
    auto link = state["link"].toObject();

    // Windows of players who left go; the rest get their strip refreshed.
    QSet<std::uint16_t> present;
    for(auto value : participants) {
        auto object = value.toObject();
        auto id = static_cast<std::uint16_t>(object["peer_id"].toInt());
        present.insert(id);
        auto it = this->windows.find(id);
        if(it != this->windows.end()) {
            it->second->update(object, link);
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

    // The main window carries the local player's name in its title while the session lasts.
    std::string local_name = active ? state["local_name"].toString().toStdString() : std::string();
    if(local_name != this->main->play_together_name) {
        this->main->play_together_name = local_name;
        this->main->refresh_title();
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
            QString paused_by = state["paused_by"].toString();
            if(!paused_by.isEmpty()) {
                text = QString("Play Together: PAUSED by %1 ").arg(paused_by);
            }
            QString link_phase = link["phase"].toString();
            if(link_phase == "linked") {
                text += QString("· Link cable: %1%2 ").arg(link["peer_name"].toString(), link["stalled"].toBool() ? " (waiting)" : "");
            }
            else if(link_phase == "starting") {
                text += QString("· Link cable: connecting to %1 ").arg(link["peer_name"].toString());
            }
        }
        this->status_label->setText(text);
        this->status_label->show();
    }
    else {
        this->status_label->hide();
    }

    this->apply_link_state(link, active);

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

void PlayTogetherController::apply_link_state(const QJsonObject &link, bool active) {
    QString phase = active ? link["phase"].toString() : QString("none");
    auto nonce = static_cast<std::uint32_t>(link["nonce"].toDouble());

    // An incoming request gets a prompt that does not stop the tick: the handshake (and both
    // games) keep running behind it. It goes away by itself when the request does.
    if(phase == "incoming") {
        if(this->link_prompt == nullptr || this->link_prompt_nonce != nonce) {
            this->close_link_prompt();
            auto *box = new QMessageBox(this->main);
            box->setAttribute(Qt::WA_DeleteOnClose);
            box->setWindowTitle("Link cable");
            box->setIcon(QMessageBox::Icon::Question);
            box->setText(QString("%1 wants to plug a link cable into your game.\n\nBoth games will run in step with a small input delay while linked, at the host's game speed; save states and replays are off until it is unplugged.").arg(link["peer_name"].toString()));
            auto *plug = box->addButton("Plug in", QMessageBox::AcceptRole);
            box->addButton("Decline", QMessageBox::RejectRole);
            box->setDefaultButton(plug);
            box->setModal(false);
            connect(box, &QMessageBox::finished, this, [this, box, plug, nonce](int) {
                bool accept = box->clickedButton() == static_cast<QAbstractButton *>(plug);
                if(this->link_prompt == box) {
                    this->link_prompt = nullptr;
                }
                if(this->main->frontend == nullptr) {
                    return;
                }
                char error[512] = {};
                if(!supershuckie_frontend_play_together_link_respond(this->main->frontend, nonce, accept, reinterpret_cast<uint8_t *>(error), sizeof(error)) && accept) {
                    this->main->show_error("Link cable", "%s", error);
                }
            });
            this->link_prompt = box;
            this->link_prompt_nonce = nonce;
            box->open();
        }
    }
    else {
        this->close_link_prompt();
    }

    // Why the last cable came out (or a request came to nothing): once, in the status bar, and
    // in a box of its own when it was not this player's doing.
    QString reason = active ? link["last_reason"].toString() : QString();
    if(reason != this->last_link_reason) {
        this->last_link_reason = reason;
        if(!reason.isEmpty()) {
            this->main->status_bar->showMessage(QString("Link cable: %1").arg(reason), 10000);
            if(!reason.startsWith("You ")) {
                auto *box = new QMessageBox(this->main);
                box->setAttribute(Qt::WA_DeleteOnClose);
                box->setWindowTitle("Link cable");
                box->setIcon(QMessageBox::Icon::Information);
                box->setText(reason);
                box->setModal(false);
                box->open();
            }
        }
    }
}

void PlayTogetherController::close_link_prompt() {
    if(this->link_prompt != nullptr) {
        auto *box = this->link_prompt;
        this->link_prompt = nullptr;
        box->disconnect(this);
        box->close();
    }
}

void PlayTogetherController::unlink() {
    supershuckie_frontend_play_together_unlink(this->main->frontend);
    this->tick();
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

QPoint PlayTogetherController::place_window(const PeerWindow *window, const QPoint *wanted) const {
    // Windows count as overlapping when their top-left corners are within a title bar of each other.
    constexpr int step = 40;
    constexpr int slack = 16;
    auto *screen = this->main->screen();
    QRect available = screen != nullptr ? screen->availableGeometry() : QRect(0, 0, 1 << 15, 1 << 15);

    QPoint pos;
    if(wanted != nullptr) {
        pos = *wanted;
    }
    else {
        // Beside the main window; if that would run off the screen, over it instead.
        auto frame = this->main->frameGeometry();
        pos = QPoint(frame.right() + 1, frame.top());
        if(pos.x() + window->frameGeometry().width() > available.right()) {
            pos = frame.topLeft() + QPoint(step, step);
        }
    }

    auto taken = [&](const QPoint &candidate) {
        for(auto &[id, other] : this->windows) {
            if(other == window || !other->isVisible()) {
                continue;
            }
            auto delta = other->pos() - candidate;
            if(std::abs(delta.x()) < slack && std::abs(delta.y()) < slack) {
                return true;
            }
        }
        return false;
    };

    for(int attempt = 0; attempt < 32 && taken(pos); attempt++) {
        pos += QPoint(step, step);
        if(pos.x() + window->frameGeometry().width() > available.right() || pos.y() + window->frameGeometry().height() > available.bottom()) {
            // Wrap to the top-left of the screen and keep cascading from there.
            pos = available.topLeft() + QPoint(step * (attempt % 8), step);
        }
    }
    return pos;
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
    if(this->is_link_cable_plugged()) {
        box.setText(QString("You're in a Play Together session with a link cable plugged in. %1 will unplug it and leave the session; the other players' games close and their replay files are finished.").arg(because));
    }
    else {
        box.setText(QString("You're in a Play Together session. %1 will leave it; the other players' games close and their replay files are finished.").arg(because));
    }
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
