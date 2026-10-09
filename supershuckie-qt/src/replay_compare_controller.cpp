#include "replay_compare_controller.hpp"
#include "screen_canvas.hpp"
#include "select_replay_dialog.hpp"
#include "main_window.hpp"

#include <QCloseEvent>
#include <QContextMenuEvent>
#include <QFileDialog>
#include <QHBoxLayout>
#include <QJsonArray>
#include <QJsonDocument>
#include <QLabel>
#include <QMenu>
#include <QMessageBox>
#include <QPushButton>
#include <QRect>
#include <QScreen>
#include <QVBoxLayout>
#include <cmath>
#include <cstdio>
#include <cstdlib>

using namespace SuperShuckie64;

/** `h:mm:ss.t` / `m:ss.t`, with a minus sign before zero. */
static QString format_timer(long long millis) {
    bool negative = millis < 0;
    unsigned long long value = static_cast<unsigned long long>(negative ? -millis : millis);
    unsigned long long sec = value / 1000;
    unsigned long long min = sec / 60;
    unsigned long long hr = min / 60;
    char text[64];
    if(hr > 0) {
        std::snprintf(text, sizeof(text), "%s%llu:%02llu:%02llu.%llu", negative ? "-" : "", hr, min % 60, sec % 60, (value % 1000) / 100);
    }
    else {
        std::snprintf(text, sizeof(text), "%s%llu:%02llu.%llu", negative ? "-" : "", min, sec % 60, (value % 1000) / 100);
    }
    return QString(text);
}

/** `+1:02.3` / `-0:04.5` / `±0:00.0`. */
static QString format_difference(long long millis) {
    if(millis == 0) {
        return QString("±0:00.0");
    }
    QString text = format_timer(millis < 0 ? -millis : millis);
    return (millis < 0 ? QString("-") : QString("+")) + text;
}

CompareWindow::CompareWindow(ReplayCompareController *controller, std::uint16_t id, const QJsonObject &replay):
    QWidget(controller->main_window(), Qt::Window | Qt::MSWindowsFixedSizeDialogHint),
    controller(controller),
    id(id)
{
    this->name = replay["name"].toString();
    this->rom_name = replay["rom_name"].toString();
    this->setWindowTitle(QString("%1 — %2 — Compare").arg(this->name, this->rom_name));

    auto *layout = new QVBoxLayout(this);
    layout->setSpacing(0);
    layout->setContentsMargins(0, 0, 0, 0);

    this->screen = new ScreenCanvas(this);
    this->screen->setFocusPolicy(Qt::NoFocus);
    layout->addWidget(this->screen, 0, Qt::AlignHCenter);

    auto *strip = new QWidget(this);
    auto *strip_layout = new QHBoxLayout(strip);
    strip_layout->setContentsMargins(6, 2, 6, 2);
    strip_layout->setSpacing(10);

    this->name_label = new QLabel(strip);
    this->name_label->setText(QString("<b>%1</b> · %2").arg(this->name.toHtmlEscaped(), this->rom_name.toHtmlEscaped()));
    this->name_label->setToolTip(replay["path"].toString());
    strip_layout->addWidget(this->name_label);

    this->sync_label = new QLabel(strip);
    strip_layout->addWidget(this->sync_label);

    this->time_label = new QLabel(strip);
    strip_layout->addWidget(this->time_label);

    strip_layout->addStretch(1);

    this->fps_label = new QLabel(strip);
    strip_layout->addWidget(this->fps_label);

    layout->addWidget(strip);
    layout->setSizeConstraint(QLayout::SetFixedSize);

    this->update(replay, QJsonObject());
}

QString CompareWindow::settings_key() const {
    QString key = "qt__compare_window_";
    for(auto c : this->name) {
        key += (c.isLetterOrNumber() ? c : QChar('_'));
    }
    return key;
}

void CompareWindow::set_layout(unsigned screen_count, const SuperShuckieScreenData *screen_data, unsigned scale) {
    bool horizontal = this->controller->main_window()->horizontal_nds->isChecked();
    this->screen->set_layout(screen_count, screen_data, scale, horizontal, false);
    this->laid_out = true;
}

void CompareWindow::update(const QJsonObject &replay, const QJsonObject &state) {
    QString status = replay["status"].toString();
    bool has_timer = replay["has_timer"].toBool();
    bool main_has_timer = state["main_has_timer"].toBool();

    QString sync;
    if(status == "in_sync") {
        sync = "In sync";
    }
    else if(status == "catching_up") {
        sync = "Catching up…";
    }
    else if(status == "not_started") {
        sync = "Not started yet";
    }
    else if(status == "ended") {
        sync = "Finished";
    }
    else if(status == "error") {
        sync = "Error";
    }
    this->sync_label->setText(sync);
    // What the replays are lined up by.
    if(has_timer && main_has_timer) {
        this->sync_label->setToolTip("Lined up by both replays' start/stop timers");
    }
    else if(has_timer) {
        this->sync_label->setToolTip("Lined up by this replay's timer against the main replay's time (the main replay has no timer)");
    }
    else if(main_has_timer) {
        this->sync_label->setToolTip("Lined up by this replay's time against the main replay's timer (this replay has no timer)");
    }
    else {
        this->sync_label->setToolTip("Lined up by replay time (neither replay has a timer)");
    }

    auto timer = static_cast<long long>(replay["timer_ms"].toDouble());
    QString time = QString("%1 %2").arg(has_timer ? "Timer" : "Time", format_timer(timer));
    if(replay["final_ms"].isDouble()) {
        auto final_ms = static_cast<long long>(replay["final_ms"].toDouble());
        time += QString(" · final %1").arg(format_timer(final_ms));
        if(state["main_final_ms"].isDouble()) {
            auto main_final = static_cast<long long>(state["main_final_ms"].toDouble());
            time += QString(" (%1)").arg(format_difference(final_ms - main_final));
        }
    }
    auto counters = replay["counters"].toObject();
    for(auto it = counters.begin(); it != counters.end(); ++it) {
        time += QString(" · %1: %2").arg(it.key(), QString::number(static_cast<long long>(it.value().toDouble())));
    }
    this->time_label->setText(time);
    this->time_label->setToolTip(replay["final_ms"].isDouble() && state["main_final_ms"].isDouble()
        ? QString("Final timer reading, and how it compares with the main replay's (%1)").arg(format_timer(static_cast<long long>(state["main_final_ms"].toDouble())))
        : QString());

    double fps = replay["fps"].toDouble();
    this->fps_label->setText(QString("%1 FPS").arg(static_cast<int>(fps + 0.5)));
}

void CompareWindow::save_geometry() {
    auto *frontend = this->controller->main_window()->frontend;
    if(frontend == nullptr || !this->laid_out) {
        return;
    }
    auto geometry = this->geometry();
    char xy[64];
    std::snprintf(xy, sizeof(xy), "%d|%d", geometry.x(), geometry.y());
    supershuckie_frontend_set_custom_setting(frontend, this->settings_key().toUtf8().constData(), xy);
}

void CompareWindow::restore_geometry() {
    auto *frontend = this->controller->main_window()->frontend;
    if(frontend == nullptr) {
        return;
    }
    const char *xy = supershuckie_frontend_get_custom_setting(frontend, this->settings_key().toUtf8().constData());
    int x, y;
    if(xy != nullptr && std::sscanf(xy, "%d|%d", &x, &y) == 2) {
        QPoint wanted(x, y);
        this->move(this->controller->place_window(this, &wanted));
    }
    else {
        this->move(this->controller->place_window(this));
    }
}

void CompareWindow::closeEvent(QCloseEvent *event) {
    this->save_geometry();
    if(!this->removed) {
        // Closing the window is how one replay stops being compared; the controller deletes the
        // window once the frontend reports it gone.
        supershuckie_frontend_replay_compare_remove(this->controller->main_window()->frontend, this->id);
    }
    this->hide();
    event->ignore();
}

void CompareWindow::contextMenuEvent(QContextMenuEvent *event) {
    auto *frontend = this->controller->main_window()->frontend;
    QMenu menu(this);
    auto *scale_menu = menu.addMenu("Scale");
    for(int scale = 1; scale <= static_cast<int>(MainWindow::PEER_SCALE_COUNT); scale++) {
        auto *action = scale_menu->addAction(QString("%1x").arg(scale));
        action->setCheckable(true);
        action->setChecked(static_cast<unsigned>(scale) == this->screen->scale());
        connect(action, &QAction::triggered, this, [this, frontend, scale]() {
            supershuckie_frontend_replay_compare_set_video_scale(frontend, this->id, static_cast<std::uint8_t>(scale));
        });
    }

    menu.addSeparator();
    auto *remove = menu.addAction("Stop comparing this replay");
    connect(remove, &QAction::triggered, this, [this]() {
        this->close();
    });

    this->controller->main_window()->stop_timer();
    menu.exec(event->globalPos());
    this->controller->main_window()->start_timer();
}

ReplayCompareController::ReplayCompareController(MainWindow *main_window): QObject(main_window), main(main_window) {}

ReplayCompareController::~ReplayCompareController() {
    for(auto &[id, window] : this->windows) {
        window->forget();
        delete window;
    }
    this->windows.clear();
}

QJsonObject ReplayCompareController::read_state() const {
    char *json = supershuckie_frontend_replay_compare_state_json(this->main->frontend);
    auto state = QJsonDocument::fromJson(QByteArray(json)).object();
    supershuckie_string_free(json);
    return state;
}

QJsonObject ReplayCompareController::replay(std::uint16_t id) const {
    for(auto value : this->last_state["replays"].toArray()) {
        auto object = value.toObject();
        if(static_cast<std::uint16_t>(object["id"].toInt()) == id) {
            return object;
        }
    }
    return QJsonObject();
}

void ReplayCompareController::tick() {
    if(this->main->frontend == nullptr) {
        return;
    }

    auto generation = supershuckie_frontend_replay_compare_generation(this->main->frontend);
    bool roster_changed = generation != this->last_generation;
    bool any = !this->windows.empty() || supershuckie_frontend_replay_compare_count(this->main->frontend) > 0;
    bool strip_due = false;
    if(any && --this->strip_countdown <= 0) {
        // The timers are shown to a tenth of a second.
        this->strip_countdown = 50;
        strip_due = true;
    }
    if(!roster_changed && !strip_due) {
        return;
    }

    this->last_generation = generation;
    this->last_state = this->read_state();
    this->apply_state(roster_changed);
}

void ReplayCompareController::apply_state(bool roster_changed) {
    auto replays = this->last_state["replays"].toArray();

    std::map<std::uint16_t, bool> present;
    for(auto value : replays) {
        auto object = value.toObject();
        auto id = static_cast<std::uint16_t>(object["id"].toInt());
        present[id] = true;
        auto it = this->windows.find(id);
        if(it != this->windows.end()) {
            it->second->update(object, this->last_state);
        }
    }
    for(auto it = this->windows.begin(); it != this->windows.end();) {
        if(present.find(it->first) == present.end()) {
            it->second->forget();
            it->second->save_geometry();
            it->second->deleteLater();
            it = this->windows.erase(it);
        }
        else {
            ++it;
        }
    }

    if(roster_changed) {
        this->main->stop_comparing_replays->setEnabled(!replays.isEmpty());
    }
}

QPoint ReplayCompareController::place_window(const CompareWindow *window, const QPoint *wanted) const {
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
            pos = available.topLeft() + QPoint(step * (attempt % 8), step);
        }
    }
    return pos;
}

void ReplayCompareController::add_from_list() {
    auto name = SelectReplayDialog::ask(this->main, "Compare with another replay", "Select a replay to play beside this one, lined up by its timer.");
    if(name == std::nullopt) {
        return;
    }
    this->add(*name, true);
}

void ReplayCompareController::add_from_file() {
    QFileDialog opener(this->main);
    opener.setFileMode(QFileDialog::FileMode::ExistingFile);
    opener.setNameFilters(QStringList({ "Replays (*.replay)", "Any files (*)" }));
    opener.setWindowTitle("Compare with a replay file");
    this->main->stop_timer();
    opener.exec();
    this->main->start_timer();
    auto files = opener.selectedFiles();
    if(files.size() != 1) {
        return;
    }
    this->add(files[0].toUtf8().toStdString(), false);
}

void ReplayCompareController::add(const std::string &replay, bool by_name) {
    std::string rom_path;
    bool override_errors = false;

    // At most: locate the ROM, then confirm a mismatch, then add.
    for(int attempt = 0; attempt < 4; attempt++) {
        char error[1024] = {};
        std::uint16_t id = 0;
        const char *rom = rom_path.empty() ? nullptr : rom_path.c_str();
        auto result = by_name
            ? supershuckie_frontend_replay_compare_add_by_name(this->main->frontend, replay.c_str(), rom, override_errors, &id, error, sizeof(error))
            : supershuckie_frontend_replay_compare_add(this->main->frontend, replay.c_str(), rom, override_errors, &id, error, sizeof(error));

        if(result == SUPERSHUCKIE_REPLAY_COMPARE_ADDED) {
            return;
        }

        if(result == SUPERSHUCKIE_REPLAY_COMPARE_NEEDS_ROM && rom_path.empty()) {
            QMessageBox box(this->main);
            box.setWindowTitle("Locate ROM");
            box.setIcon(QMessageBox::Icon::Information);
            box.setText(QString::fromUtf8(error));
            QPushButton *choose = box.addButton("Choose file…", QMessageBox::AcceptRole);
            box.addButton(QMessageBox::Cancel);
            box.setDefaultButton(choose);
            this->main->stop_timer();
            box.exec();
            this->main->start_timer();
            if(box.clickedButton() != static_cast<QAbstractButton *>(choose)) {
                return;
            }

            QFileDialog opener(this->main);
            opener.setFileMode(QFileDialog::FileMode::ExistingFile);
            opener.setNameFilters(QStringList({ "All compatible ROM files (*.gb *.gbc *.gba *.nds)", "Any files (*)" }));
            opener.setWindowTitle("Select the replay's ROM");
            this->main->stop_timer();
            opener.exec();
            this->main->start_timer();
            auto files = opener.selectedFiles();
            if(files.size() != 1) {
                return;
            }
            rom_path = files[0].toUtf8().toStdString();
            continue;
        }

        if(result == SUPERSHUCKIE_REPLAY_COMPARE_MISMATCH && !override_errors) {
            QMessageBox box(this->main);
            box.setWindowTitle("Replay file issues detected");
            box.setIcon(QMessageBox::Icon::Warning);
            box.setText(QString::fromUtf8(error));
            box.setInformativeText("Compare it anyway?");
            box.setStandardButtons(QMessageBox::Yes | QMessageBox::No);
            box.setDefaultButton(QMessageBox::No);
            this->main->stop_timer();
            auto answer = box.exec();
            this->main->start_timer();
            if(answer != QMessageBox::Yes) {
                return;
            }
            override_errors = true;
            continue;
        }

        this->main->show_error("Cannot compare this replay", "%s", error);
        return;
    }
}

void ReplayCompareController::clear() {
    supershuckie_frontend_replay_compare_clear(this->main->frontend);
}

void ReplayCompareController::save_windows() {
    for(auto &[id, window] : this->windows) {
        window->save_geometry();
    }
}

void ReplayCompareController::on_compare_refresh_screens(void *user_data, std::uint16_t id, std::size_t screen_count, const uint32_t *const *pixels) {
    auto *self = reinterpret_cast<MainWindow *>(user_data)->replay_compare;
    if(self == nullptr) {
        return;
    }
    auto it = self->windows.find(id);
    if(it == self->windows.end()) {
        return;
    }
    it->second->canvas()->refresh_screen(static_cast<unsigned>(screen_count), pixels);
}

void ReplayCompareController::on_compare_change_video_mode(void *user_data, std::uint16_t id, std::size_t screen_count, const SuperShuckieScreenData *screen_data, std::uint8_t scaling) {
    auto *self = reinterpret_cast<MainWindow *>(user_data)->replay_compare;
    if(self == nullptr) {
        return;
    }
    auto it = self->windows.find(id);
    CompareWindow *window;
    bool fresh = false;
    if(it == self->windows.end()) {
        // Read-only getters are allowed here (no lock is held): fetch the replay's name.
        self->last_state = self->read_state();
        window = new CompareWindow(self, id, self->replay(id));
        self->windows.emplace(id, window);
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
