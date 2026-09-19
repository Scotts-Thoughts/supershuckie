#include "peer_window.hpp"
#include "play_together_controller.hpp"
#include "screen_canvas.hpp"
#include "audio_output.hpp"
#include "main_window.hpp"

#include <QCloseEvent>
#include <QContextMenuEvent>
#include <QJsonObject>
#include <QLabel>
#include <QMenu>
#include <QVBoxLayout>
#include <QHBoxLayout>
#include <cstdio>

using namespace SuperShuckie64;

static QString format_peer_time(std::uint64_t millis) {
    std::uint64_t sec = millis / 1000;
    std::uint64_t min = sec / 60;
    std::uint64_t hr = min / 60;
    char text[64];
    if(hr > 0) {
        std::snprintf(text, sizeof(text), "%llu:%02llu:%02llu.%llu", static_cast<unsigned long long>(hr), static_cast<unsigned long long>(min % 60), static_cast<unsigned long long>(sec % 60), static_cast<unsigned long long>((millis % 1000) / 100));
    }
    else {
        std::snprintf(text, sizeof(text), "%llu:%02llu.%llu", static_cast<unsigned long long>(min), static_cast<unsigned long long>(sec % 60), static_cast<unsigned long long>((millis % 1000) / 100));
    }
    return QString(text);
}

PeerWindow::PeerWindow(PlayTogetherController *controller, std::uint16_t peer_id, const QJsonObject &participant):
    QWidget(controller->main_window(), Qt::Window | Qt::MSWindowsFixedSizeDialogHint),
    controller(controller),
    id(peer_id)
{
    this->name = participant["name"].toString();
    this->rom_name = participant["rom_name"].toString();
    this->player_color = QColor(participant["color_rgb"].toString());
    this->setWindowTitle(QString("%1 — %2 — Play Together").arg(this->name, this->rom_name));

    auto *layout = new QVBoxLayout(this);
    layout->setSpacing(0);
    // The player's colour frames the screen and fills the status strip, so whose window this is
    // can be told from across the room.
    if(this->player_color.isValid()) {
        layout->setContentsMargins(4, 4, 4, 4);
        QPalette frame = this->palette();
        frame.setColor(QPalette::Window, this->player_color);
        this->setPalette(frame);
        this->setAutoFillBackground(true);
    }
    else {
        layout->setContentsMargins(0, 0, 0, 0);
    }

    this->screen = new ScreenCanvas(this);
    this->screen->setFocusPolicy(Qt::NoFocus);
    layout->addWidget(this->screen, 0, Qt::AlignHCenter);

    auto *strip = new QWidget(this);
    auto *strip_layout = new QHBoxLayout(strip);
    strip_layout->setContentsMargins(6, 2, 6, 2);
    strip_layout->setSpacing(10);
    if(this->player_color.isValid()) {
        QPalette tinted = strip->palette();
        tinted.setColor(QPalette::Window, this->player_color);
        tinted.setColor(QPalette::WindowText, contrasting_text(this->player_color));
        strip->setPalette(tinted);
        strip->setAutoFillBackground(true);
    }

    this->name_label = new QLabel(strip);
    this->name_label->setText(QString("<b>%1</b> · %2").arg(this->name.toHtmlEscaped(), this->rom_name.toHtmlEscaped()));
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

    this->update(participant, QJsonObject());
}

PeerWindow::~PeerWindow() {
    this->audio.reset();
}

QColor PeerWindow::contrasting_text(const QColor &background) {
    // Relative luminance (sRGB, roughly): dark backgrounds get white text.
    double luminance = 0.2126 * background.redF() + 0.7152 * background.greenF() + 0.0722 * background.blueF();
    return luminance > 0.5 ? QColor(Qt::black) : QColor(Qt::white);
}

QString PeerWindow::settings_key() const {
    QString key = "qt__play_together_window_";
    for(auto c : this->name) {
        key += (c.isLetterOrNumber() ? c : QChar('_'));
    }
    return key;
}

void PeerWindow::set_layout(unsigned screen_count, const SuperShuckieScreenData *screen_data, unsigned scale) {
    bool horizontal = this->controller->main_window()->horizontal_nds->isChecked();
    this->screen->set_layout(screen_count, screen_data, scale, horizontal, false);
    this->laid_out = true;
}

void PeerWindow::update(const QJsonObject &participant, const QJsonObject &link) {
    QString status = participant["status"].toString();
    QString status_text = participant["status_text"].toString();
    auto behind = static_cast<std::uint64_t>(participant["frames_behind"].toDouble());
    auto mismatches = static_cast<std::uint64_t>(participant["hash_mismatches"].toDouble());
    this->linkable = participant["can_link"].toBool();

    // The link cable, when it is (going) into this player's game.
    QString link_phase = link["phase"].toString();
    this->link_peer = link_phase != "none" && !link_phase.isEmpty() && static_cast<std::uint16_t>(link["peer_id"].toInt()) == this->id;
    QString linked_with;
    if(participant["linked_with"].isDouble()) {
        linked_with = QString(" · linked with player %1").arg(static_cast<int>(participant["linked_with"].toDouble()));
    }

    QString sync;
    if(this->link_peer && link_phase == "linked") {
        if(link["stalled"].toBool()) {
            sync = QString("\U0001F517 Waiting for %1…").arg(this->name);
        }
        else {
            sync = QString("\U0001F517 Linked · %1 frame%2 delay").arg(link["input_delay"].toInt()).arg(link["input_delay"].toInt() == 1 ? "" : "s");
        }
    }
    else if(this->link_peer && link_phase == "starting") {
        sync = "\U0001F517 Connecting cable…";
    }
    else if(this->link_peer && link_phase == "requesting") {
        sync = QString("\U0001F517 Asking %1…").arg(this->name);
    }
    else if(this->link_peer && link_phase == "incoming") {
        sync = QString("\U0001F517 %1 wants to link").arg(this->name);
    }
    else if(status == "needs_rom") {
        sync = "ROM needed (right-click to locate)";
    }
    else if(status == "starting") {
        sync = QString("Waiting for %1's game…").arg(this->name);
    }
    else if(status == "waiting") {
        sync = QString("Waiting for %1…").arg(this->name);
    }
    else if(status == "resyncing") {
        sync = "Resyncing…";
    }
    else if(status == "ended") {
        sync = "Stream ended";
    }
    else if(status == "error") {
        sync = QString("Error: %1").arg(status_text);
    }
    else if(behind <= 1) {
        sync = "In sync";
    }
    else {
        sync = QString("%1 frames behind").arg(behind);
    }
    if(mismatches > 0 && status != "error") {
        sync += QString(" · desynced ×%1").arg(mismatches);
    }
    if(!this->link_peer) {
        sync += linked_with;
    }
    this->sync_label->setText(sync);
    this->sync_label->setToolTip(status_text);

    auto elapsed_ms = static_cast<std::uint64_t>(participant["elapsed_ms"].toDouble());
    QString time = format_peer_time(elapsed_ms);
    auto counters = participant["counters"].toObject();
    for(auto it = counters.begin(); it != counters.end(); ++it) {
        time += QString(" · %1: %2").arg(it.key(), QString::number(static_cast<long long>(it.value().toDouble())));
    }
    this->time_label->setText(time);

    double fps = participant["fps"].toDouble();
    this->fps_label->setText(QString("%1 FPS").arg(static_cast<int>(fps + 0.5)));

    auto pokeabyte_port = participant["pokeabyte_port"];
    this->name_label->setToolTip(pokeabyte_port.isDouble()
        ? QString("Served to Poke-A-Byte on port %1 (/instances/%1/)").arg(static_cast<int>(pokeabyte_port.toDouble()))
        : QString("Not served to Poke-A-Byte (right-click to enable)"));
}

bool PeerWindow::set_audio(bool enabled, std::string *error) {
    auto *frontend = this->controller->main_window()->frontend;
    char buf[512] = {};
    if(!supershuckie_frontend_play_together_set_peer_audio_enabled(frontend, this->id, enabled, reinterpret_cast<uint8_t *>(buf), sizeof(buf))) {
        if(error) {
            *error = buf;
        }
        return false;
    }
    if(!enabled) {
        this->audio.reset();
        return true;
    }
    auto *ring = supershuckie_frontend_play_together_retain_peer_audio_output(frontend, this->id);
    if(ring == nullptr) {
        if(error) {
            *error = "No audio ring for this player.";
        }
        return false;
    }
    this->audio = std::make_unique<AudioOutput>(ring);
    this->audio->set_gain(static_cast<float>(supershuckie_frontend_get_audio_volume(frontend)) / 100.0f);
    if(!this->audio->open()) {
        if(error) {
            *error = this->audio->last_error();
        }
        this->audio.reset();
        supershuckie_frontend_play_together_set_peer_audio_enabled(frontend, this->id, false, nullptr, 0);
        return false;
    }
    return true;
}

bool PeerWindow::audio_on() const noexcept {
    return this->audio != nullptr && this->audio->is_open();
}

void PeerWindow::save_geometry() {
    auto *frontend = this->controller->main_window()->frontend;
    if(frontend == nullptr || !this->laid_out) {
        return;
    }
    auto geometry = this->geometry();
    char xy[64];
    std::snprintf(xy, sizeof(xy), "%d|%d", geometry.x(), geometry.y());
    supershuckie_frontend_set_custom_setting(frontend, this->settings_key().toUtf8().constData(), xy);
}

void PeerWindow::restore_geometry() {
    auto *frontend = this->controller->main_window()->frontend;
    if(frontend == nullptr) {
        return;
    }
    const char *xy = supershuckie_frontend_get_custom_setting(frontend, this->settings_key().toUtf8().constData());
    int x, y;
    if(xy != nullptr && std::sscanf(xy, "%d|%d", &x, &y) == 2) {
        // A remembered spot, unless another player's window already sits there.
        QPoint wanted(x, y);
        this->move(this->controller->place_window(this, &wanted));
    }
    else {
        this->move(this->controller->place_window(this));
    }
}

void PeerWindow::closeEvent(QCloseEvent *event) {
    // Hiding keeps the game running; the controller decides when the window goes for good.
    this->save_geometry();
    this->hide();
    supershuckie_frontend_play_together_set_window_hidden(this->controller->main_window()->frontend, this->id, true);
    event->ignore();
}

void PeerWindow::contextMenuEvent(QContextMenuEvent *event) {
    QMenu menu(this);
    auto *scale_menu = menu.addMenu("Scale");
    for(int scale = 1; scale <= 6; scale++) {
        auto *action = scale_menu->addAction(QString("%1x").arg(scale));
        action->setCheckable(true);
        action->setChecked(static_cast<unsigned>(scale) == this->screen->scale());
        connect(action, &QAction::triggered, this, [this, scale]() {
            supershuckie_frontend_play_together_set_video_scale(this->controller->main_window()->frontend, this->id, static_cast<std::uint8_t>(scale));
        });
    }

    auto *audio_action = menu.addAction("Friend audio");
    audio_action->setCheckable(true);
    audio_action->setChecked(this->audio_on());
    connect(audio_action, &QAction::triggered, this, [this](bool on) {
        std::string error;
        if(!this->set_audio(on, &error)) {
            this->controller->main_window()->show_error("Friend audio", "%s", error.c_str());
        }
    });

    auto *frontend = this->controller->main_window()->frontend;
    auto pokeabyte_port = supershuckie_frontend_play_together_get_peer_pokeabyte_port(frontend, this->id);
    auto *pokeabyte_action = menu.addAction(pokeabyte_port == 0 ? QString("Poke-A-Byte integration") : QString("Poke-A-Byte integration (port %1)").arg(pokeabyte_port));
    pokeabyte_action->setCheckable(true);
    pokeabyte_action->setChecked(pokeabyte_port != 0);
    pokeabyte_action->setToolTip("Serve this game to Poke-A-Byte on its own UDP port (Poke-A-Byte reads it through /instances/<port>/)");
    connect(pokeabyte_action, &QAction::triggered, this, [this, frontend](bool on) {
        char error[512] = {};
        if(!supershuckie_frontend_play_together_set_peer_pokeabyte_enabled(frontend, this->id, on, reinterpret_cast<uint8_t *>(error), sizeof(error))) {
            this->controller->main_window()->show_error("Poke-A-Byte integration", "%s", error);
        }
    });

    auto *locate = menu.addAction("Locate ROM…");
    connect(locate, &QAction::triggered, this, [this]() {
        this->controller->locate_rom_for(this->id);
    });

    menu.addSeparator();
    if(this->link_peer) {
        auto *unlink = menu.addAction("Unplug link cable");
        unlink->setToolTip("Pull the link cable; both games go on on their own");
        connect(unlink, &QAction::triggered, this, [frontend]() {
            supershuckie_frontend_play_together_unlink(frontend);
        });
    }
    else {
        auto *plug = menu.addAction("Plug in link cable");
        plug->setToolTip(this->linkable
            ? QString("Ask %1 to plug a link cable between your games, to trade or battle").arg(this->name)
            : QString("%1's game has to be followed here, in sync, on the same console family as yours, with no other cable in").arg(this->name));
        plug->setEnabled(this->linkable);
        connect(plug, &QAction::triggered, this, [this, frontend]() {
            char error[512] = {};
            if(!supershuckie_frontend_play_together_link_request(frontend, this->id, reinterpret_cast<uint8_t *>(error), sizeof(error))) {
                this->controller->main_window()->show_error("Link cable", "%s", error);
            }
        });
    }

    this->controller->main_window()->stop_timer();
    menu.exec(event->globalPos());
    this->controller->main_window()->start_timer();
}
