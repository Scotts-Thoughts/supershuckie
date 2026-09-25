// FIXME: we need this to be somewhere else
#define SUPERSHUCKIE_VERSION "0.4.15stp"

#include <cstdio>
#include <cstdint>
#include <cstdarg>
#include <string>
#include <QLayout>
#include <SDL3/SDL.h>
#include <QMenuBar>
#include <QCloseEvent>
#include <QStatusBar>
#include <QFileDialog>
#include <QFontDatabase>
#include <QLabel>
#include <QStandardPaths>
#include <QDesktopServices>
#include <QGridLayout>
#include <QImage>
#include <QDateTime>
#include <QDir>
#include <QFileInfo>

#ifdef _WIN32
#include <windows.h>
#include <dwmapi.h>
#endif

#include <supershuckie/supershuckie.h>

#include "ask_for_text_dialog.hpp"
#include "audio_output.hpp"
#include "nds_date_dialog.hpp"
#include "gb_palette_dialog.hpp"
#include "select_item_dialog.hpp"
#include "error.hpp"
#include "game_speed_dialog.hpp"
#include "render_widget.hpp"
#include "main_window.hpp"
#include "controller_settings_window.hpp"
#include "replay_playback_controls.hpp"
#include "video_export_dialog.hpp"
#include "memory_tools_controller.hpp"
#include "bookmark_window.hpp"
#include "landing_widget.hpp"
#include "play_together_controller.hpp"

#include <QProgressDialog>
#include <QToolButton>
#include <QMessageBox>
#include <QThread>
#include <QPushButton>
#include <QCoreApplication>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QKeyEvent>
#include <QSet>

using namespace SuperShuckie64;

static const char *USE_NUMBER_KEYS_FOR_QUICK_SLOTS = "qt__number_keys_for_quick_slots";
static const char *WINDOW_XY = "qt__window_xy";
static const char *DISPLAY_STATUS_BAR = "qt__display_status_bar";
static const char *SYNC_DISPLAY_TO_REFRESH = "qt__sync_display_to_refresh";
static const char *KEYBOARD_REPLAY_CONTROLS_DISABLED = "qt__replay_controls_disabled";
static const char *HORIZONTAL_NDS = "qt__horizontal_nds";
static const char *BOOKMARK_WINDOW_STATE = "qt__bookmark_window";
static const char *SHORTCUTS = "qt__shortcuts";

// An action's name in the Shortcuts window when its menu text isn't it (see collect_shortcut_bindings()).
static const char *SHORTCUT_NAME_PROPERTY = "shortcut_name";

// Followed by the ROM's path; see rebuild_favorite_roms_menu().
static const QString FAVORITE_ROM_SHORTCUT_PREFIX = "open-favorite-rom:";

class SuperShuckie64::SuperShuckieTimestamp: public QWidget {
public:
    SuperShuckieTimestamp(QWidget *parent): QWidget(parent) {
        QHBoxLayout *layout = new QHBoxLayout(this);
        layout->setSpacing(0);
        layout->setContentsMargins(0,0,0,0);

        this->timestamp = new QLabel("99:99:99", this);
        this->timestamp->setAlignment(Qt::AlignRight);

        this->ms = new QLabel(".999", this);
        this->ms->setFixedSize(this->ms->sizeHint());
        this->ms->setAlignment(Qt::AlignLeft);

        this->ds = new QLabel(".9", this);
        this->ds->setFixedSize(this->ds->sizeHint());
        this->ds->setAlignment(Qt::AlignLeft);

        layout->addWidget(this->timestamp);
        layout->addWidget(this->ms);
        layout->addWidget(this->ds);

        this->ds->hide();

        this->setFixedSize(this->sizeHint());
        this->setMaximumHeight(this->ds->sizeHint().height());
        this->setMaximumWidth(10000);
    }

    void set_timestamp(std::uint32_t ms_total) {
        std::uint32_t ms = ms_total;
        std::uint32_t sec = ms_total / 1000;
        std::uint32_t min = sec / 60;
        std::uint32_t hr = min / 60;

        min %= 60;
        sec %= 60;
        ms %= 1000;

        char timer[256];
        std::snprintf(timer, sizeof(timer), "%02d:%02d:%02d", hr, min, sec);
        this->timestamp->setText(timer);

        std::snprintf(timer, sizeof(timer), ".%.03d", ms);
        this->ms->setText(timer);

        std::snprintf(timer, sizeof(timer), ".%.01d", ms / 100);
        this->ds->setText(timer);

        this->ms->setVisible(hr < 100);
        this->ds->setVisible(!this->ms->isVisible());
    }
private:
    QLabel *timestamp;
    QLabel *ms;
    QLabel *ds;
};

MainWindow::MainWindow(): QMainWindow() {
    // Remove rounded corners (Windows)
    #ifdef _WIN32
    DWORD one = 1;
    DwmSetWindowAttribute(reinterpret_cast<HWND>(this->winId()), 33, &one, sizeof(one));
    #endif

    auto *center_widget = new QWidget(this);
    auto *layout = new QGridLayout(center_widget);
    layout->setVerticalSpacing(0);
    layout->setContentsMargins(0,0,0,0);
    layout->setHorizontalSpacing(0);
    this->setCentralWidget(center_widget);

    this->render_widget = new GameRenderWidget(this, center_widget);
    layout->addWidget(this->render_widget, 0, 0);

    // Shares the game view's cell; exactly one of the two is visible at a time.
    this->landing_widget = new LandingWidget(this, center_widget);
    layout->addWidget(this->landing_widget, 0, 0);
    this->landing_widget->hide();

    this->playback_bar = new ReplayPlaybackControls(this, center_widget);
    layout->addWidget(this->playback_bar, 1, 0);
    this->playback_bar->hide();

    this->status_bar = new QStatusBar(this);
    this->setStatusBar(this->status_bar);

    this->paused_state = new QLabel("PAUSED");
    this->paused_state->setFixedSize(this->paused_state->sizeHint());
    this->status_bar->addPermanentWidget(this->paused_state);
    this->paused_state->hide();

    this->status_bar_time = new SuperShuckieTimestamp(this);
    this->status_bar->addPermanentWidget(this->status_bar_time);
    this->status_bar_time->hide();

    this->frozen_state = new QToolButton(this->status_bar);
    this->frozen_state->setAutoRaise(true);
    this->frozen_state->setToolTip("Values frozen by the RAM tools (click to open RAM watch)");
    this->status_bar->addPermanentWidget(this->frozen_state);
    this->frozen_state->hide();
    connect(this->frozen_state, &QToolButton::clicked, this, &MainWindow::do_open_ram_watch);

    this->ram_modified_state = new QLabel("RAM MODIFIED");
    this->ram_modified_state->setFixedSize(this->ram_modified_state->sizeHint());
    this->status_bar->addPermanentWidget(this->ram_modified_state);
    this->ram_modified_state->hide();

    this->current_state = new QLabel("RECORDING");
    this->current_state->setFixedSize(this->current_state->sizeHint());
    this->status_bar->addPermanentWidget(this->current_state);
    this->current_state->hide();

    this->status_bar_fps = new QLabel("999+ FPS ", this->status_bar);
    this->status_bar_fps->setToolTip("Emulated frames per second (hover for frame-time details)");
    this->status_bar_fps->setFixedSize(this->status_bar_fps->sizeHint());
    this->status_bar_fps->setAlignment(Qt::AlignRight);
    this->status_bar_fps->setText("0 FPS ");
    this->status_bar->addPermanentWidget(this->status_bar_fps);

    this->setWindowFlags(Qt::MSWindowsFixedSizeDialogHint);
    this->layout()->setSizeConstraint(QLayout::SetFixedSize);

    this->ticker.setInterval(1);
    this->ticker.callOnTimeout(this, &MainWindow::tick);

    this->set_up_menu();

    SuperShuckieFrontendCallbacks callbacks = {};
    callbacks.user_data = this;
    callbacks.refresh_screens = MainWindow::on_refresh_screens;
    callbacks.change_video_mode = MainWindow::on_change_video_mode;
    callbacks.peer_refresh_screens = PlayTogetherController::on_peer_refresh_screens;
    callbacks.peer_change_video_mode = PlayTogetherController::on_peer_change_video_mode;

    QString config_path;

    #ifdef _WIN32
    this->app_dir = QString("./UserData");
    config_path = this->app_dir;
    #else
    this->app_dir = QStandardPaths::writableLocation(QStandardPaths::AppDataLocation);
    config_path = QStandardPaths::writableLocation(QStandardPaths::AppConfigLocation);
    QDir().mkpath(this->app_dir);
    QDir().mkpath(config_path);
    #endif

    this->frontend = supershuckie_frontend_new(
        this->app_dir.toStdString().c_str(),
        config_path.toStdString().c_str(),
        &callbacks
    );

    const char *status_bar_visible_setting = supershuckie_frontend_get_custom_setting(this->frontend, DISPLAY_STATUS_BAR);
    bool status_bar_visible = status_bar_visible_setting != nullptr && *status_bar_visible_setting == '1';
    this->status_bar->setVisible(status_bar_visible);
    this->show_status_bar->setChecked(status_bar_visible);

    const char *sync_display_setting = supershuckie_frontend_get_custom_setting(this->frontend, SYNC_DISPLAY_TO_REFRESH);
    bool sync_display = sync_display_setting != nullptr && *sync_display_setting == '1';
    this->sync_display_to_refresh->setChecked(sync_display);
    this->apply_display_sync(sync_display);

    char buf[256];
    if(supershuckie_frontend_is_pokeabyte_enabled(this->frontend, buf, sizeof(buf))) {
        this->enable_pokeabyte_integration->setChecked(true);
    }
    else if(buf[0] != 0) {
        this->show_error("Failed to automatically start Poke-A-Byte integration", "An error occurred on startup when trying to enable Poke-A-Byte integration:\n\n%s", buf);
    }
    this->pokeabyte_port->setText(QString("Port (%1)…").arg(supershuckie_frontend_get_pokeabyte_port(this->frontend)));
    this->pokeabyte_serve_friends->setChecked(supershuckie_frontend_get_pokeabyte_serve_friends(this->frontend));
    if(supershuckie_frontend_get_external_commands_enabled(this->frontend, buf, sizeof(buf))) {
        this->enable_external_commands->setChecked(true);
    }
    else if(buf[0] != 0) {
        this->show_error("Failed to automatically start external commands", "An error occurred on startup when trying to enable external commands:\n\n%s", buf);
    }

    const char *quick_slots = supershuckie_frontend_get_custom_setting(this->frontend, USE_NUMBER_KEYS_FOR_QUICK_SLOTS);
    if(quick_slots != nullptr && quick_slots[0] == '1') {
        this->use_number_keys_for_quick_slots = true;
        this->use_number_row_for_quick_slots->setChecked(true);
    }
    this->load_shortcuts();
    this->set_quick_load_shortcuts();

    const char *horizontal_nds = supershuckie_frontend_get_custom_setting(this->frontend, HORIZONTAL_NDS);
    if(horizontal_nds != nullptr && horizontal_nds[0] == '1') {
        this->horizontal_nds->setChecked(true);
    }

    const char *disable_keyboard_controls_replays = supershuckie_frontend_get_custom_setting(this->frontend, KEYBOARD_REPLAY_CONTROLS_DISABLED);
    if(disable_keyboard_controls_replays != nullptr && disable_keyboard_controls_replays[0] == '1') {
        this->keyboard_replay_controls->setChecked(false);
    }

    const char *xy = supershuckie_frontend_get_custom_setting(this->frontend, WINDOW_XY);
    if(xy != nullptr) {
        int x;
        int y;
        if(std::sscanf(xy, "%d|%d", &x, &y) == 2) {
            auto geometry = this->geometry();
            geometry.setX(x);
            geometry.setY(y);
            this->setGeometry(geometry);
        }
    }

    this->pause->setChecked(supershuckie_frontend_is_paused(this->frontend));
    this->auto_stop_replay_on_input->setChecked(supershuckie_frontend_get_auto_stop_playback_on_input_setting(this->frontend));
    this->auto_unpause_on_input->setChecked(supershuckie_frontend_get_auto_unpause_on_input_setting(this->frontend));
    this->auto_pause_on_record->setChecked(supershuckie_frontend_get_auto_pause_on_record_setting(this->frontend));
    this->sgb_enabled->setChecked(supershuckie_frontend_is_sgb_enabled(this->frontend));
    {
        SuperShuckieGBCustomColors colors = {};
        supershuckie_frontend_get_gb_custom_colors(this->frontend, &colors);
        this->gb_custom_colors->setChecked(colors.enabled);
    }
    this->nds_jit->setChecked(supershuckie_frontend_get_nds_jit(this->frontend));
    this->swap_nds_screens->setChecked(supershuckie_frontend_get_swap_nds_screens(this->frontend));
    this->ignore_speed_changes_in_replay->setChecked(supershuckie_frontend_get_ignore_speed_changes_in_replay(this->frontend));
    this->auto_resync_keyframes_in_replay->setChecked(supershuckie_frontend_get_auto_resync_keyframes_in_replay(this->frontend));
    this->disable_save_states_when_recording->setChecked(supershuckie_frontend_get_disable_save_states_when_recording(this->frontend));
    this->disable_speed_changes_when_recording->setChecked(supershuckie_frontend_get_disable_speed_changes_when_recording(this->frontend));

    // The ring outlives every core, so one handle and (when enabled) one device for the life of
    // the window. Audio is off unless the user turned it on; then it starts with the app.
    this->audio = std::make_unique<AudioOutput>(supershuckie_frontend_retain_audio_output(this->frontend));
    this->audio_enabled->setChecked(supershuckie_frontend_get_audio_enabled(this->frontend));
    this->audio_muted->setChecked(supershuckie_frontend_get_audio_muted(this->frontend));
    this->audio_mute_when_sped_up->setChecked(supershuckie_frontend_get_audio_mute_when_sped_up(this->frontend));
    this->apply_audio_gain();
    if(supershuckie_frontend_get_audio_enabled(this->frontend) && !this->audio->open()) {
        supershuckie_frontend_set_audio_enabled(this->frontend, false);
        this->audio_enabled->setChecked(false);
        this->show_error("Failed to open the audio device", "Audio has been turned off. Enable it again from the Audio menu to retry.\n\n%s", this->audio->last_error().c_str());
    }

    this->sdl.frontend = this->frontend;
    this->render_widget->setFocus(Qt::OtherFocusReason);
    this->rebuild_recent_roms_menu();
    this->rebuild_nds_date_menu();

    // The frontend exists now, so the favorites list can be read; the video-mode callback that
    // fired during supershuckie_frontend_new already chose which view to show.
    this->landing_widget->reload();
    this->rebuild_favorite_roms_menu();
    this->landing_widget->rebuild_tiles(); // again, now that their shortcuts are known

    this->memory_tools = new MemoryToolsController(this);
    this->memory_tools->restore_windows();

    this->play_together = new PlayTogetherController(this);
    this->pt_save_replays->setChecked(supershuckie_frontend_play_together_get_save_peer_replays(this->frontend));
    this->refresh_play_together_actions();

    const char *bookmark_window_state = supershuckie_frontend_get_custom_setting(this->frontend, BOOKMARK_WINDOW_STATE);
    if(bookmark_window_state != nullptr) {
        // Copy out of the FFI buffer before constructing the window: its own constructor makes
        // many further API calls, any of which can invalidate the pointer returned above.
        QString bookmark_window_state_copy = QString::fromUtf8(bookmark_window_state);
        this->bookmark_window = new BookmarkWindow(this);
        this->bookmark_window->restore_state(bookmark_window_state_copy);
    }
    this->confirm_ram_writes->setChecked(supershuckie_frontend_memory_get_confirm_writes_while_recording(this->frontend));

    this->ticker.start();
}

void MainWindow::set_title(const char *title) {
    std::strncpy(this->title_text, title, sizeof(this->title_text) - 1);
    this->status_bar->showMessage(title);
    this->refresh_title();
}

void MainWindow::refresh_title() {
    char fmt[512];

    const char *rom_name = this->frontend ? supershuckie_frontend_get_rom_name(this->frontend) : "(Frontend not yet loaded)";
    if(rom_name == nullptr) {
        rom_name = "No ROM Loaded";
    };

    // While playing with friends, lead with the player's name so each window is identifiable
    // (peer windows are titled the same way).
    char prefix[160] = {};
    if(!this->play_together_name.empty()) {
        std::snprintf(prefix, sizeof(prefix), "%s — ", this->play_together_name.c_str());
    }

    if(this->status_bar->isVisible()) {
        std::snprintf(fmt, sizeof(fmt), "%sSuper Shuckie " SUPERSHUCKIE_VERSION " - %s", prefix, rom_name);
    }
    else if(this->title_text[0] == 0) {
        std::snprintf(fmt, sizeof(fmt), "%sSuper Shuckie " SUPERSHUCKIE_VERSION " - %s - %.00f FPS", prefix, rom_name, this->current_fps);
    }
    else {
        std::snprintf(fmt, sizeof(fmt), "%sSuper Shuckie " SUPERSHUCKIE_VERSION " - %s - %s - %.00f FPS", prefix, rom_name, this->title_text, this->current_fps);
    }

    this->setWindowTitle(fmt);
}

void MainWindow::tick() {
    while(true) {
        auto sdl_event = this->sdl.next();
        switch(sdl_event.discriminator) {
            case SDLEventWrapperAction::SDLEventWrapper_NoOp:
                goto break_sdl_loop;
            case SDLEventWrapperAction::SDLEventWrapper_Quit:
                this->close();
                // If the window wasn't closed, warn
                if(this->isVisible()) {
                    std::fputs("Can't close the main window. Finish what you're doing, first!\n", stderr);
                    break;
                }
                else {
                    return;
                }
            case SDLEventWrapperAction::SDLEventWrapper_Axis:
                supershuckie_frontend_axis(this->frontend, sdl_event.axis.controller->mapping, sdl_event.axis.axis, sdl_event.axis.value);
                break;
            case SDLEventWrapperAction::SDLEventWrapper_Button:
                supershuckie_frontend_button_press(this->frontend, sdl_event.button.controller->mapping, sdl_event.button.button, sdl_event.button.pressed);
                break;
        }
    }
    break_sdl_loop:
    for(auto &i : this->sdl.events_to_print) {
        this->set_title(i.c_str());
    }
    this->sdl.events_to_print.clear();

    auto now = clock::now();
    auto time_since_last_second_us = std::chrono::duration_cast<std::chrono::microseconds>(now - this->second_start).count();

    // The emulation rate counts every emulated frame, drawn or not; the display rate counts the
    // frames that reached the screen (at most ~60/s when fast-forwarding).
    double emulation_fps = supershuckie_frontend_get_emulation_fps(this->frontend);

    if(time_since_last_second_us > 1000000) {
        this->current_display_fps = 1000000.0 * static_cast<double>(this->frames_in_last_second) / static_cast<double>(time_since_last_second_us);
        this->current_fps = emulation_fps;
        this->frames_in_last_second = 0;
        this->second_start = now;

        std::uint32_t average_us = 0, max_us = 0, budget_us = 0;
        std::uint64_t over_budget = 0;
        supershuckie_frontend_get_frame_time_stats(this->frontend, &average_us, nullptr, &max_us, &budget_us, &over_budget);

        char fps_text[64];
        if(this->current_fps > 999) {
            std::snprintf(fps_text, sizeof(fps_text), "999+ FPS ");
        }
        else if(this->current_fps > 0.0 && this->current_fps < 1.0) {
            std::snprintf(fps_text, sizeof(fps_text), "<1 FPS ");
        }
        else {
            std::snprintf(fps_text, sizeof(fps_text), "%d FPS ", static_cast<int>(this->current_fps + 0.5));
        }
        this->status_bar_fps->setText(fps_text);

        char detail[256];
        if(budget_us > 0) {
            std::snprintf(
                detail, sizeof(detail),
                "Emulation: %.1f frames/s (display %.0f/s)\nFrame time: %.2f ms average, %.2f ms worst, budget %.2f ms\nFrames over budget since last speed change: %llu",
                this->current_fps, this->current_display_fps,
                average_us / 1000.0, max_us / 1000.0, budget_us / 1000.0,
                static_cast<unsigned long long>(over_budget)
            );
        }
        else {
            std::snprintf(detail, sizeof(detail), "Emulation: %.1f frames/s (display %.0f/s)\nFrame time: %.2f ms average, %.2f ms worst", this->current_fps, this->current_display_fps, average_us / 1000.0, max_us / 1000.0);
        }
        QString tooltip = detail;
        if(this->display_sync) {
            std::uint64_t shown_for[4] = {};
            std::uint64_t never_shown = 0;
            std::uint32_t refreshes_per_frame = 0;
            supershuckie_frontend_get_present_cadence_stats(this->frontend, shown_for, &never_shown, &refreshes_per_frame);
            std::snprintf(
                detail, sizeof(detail),
                "\nDisplay sync: holding %u refresh(es) per frame\nFrames shown for 1 / 2 / 3 / 4+ refreshes: %llu / %llu / %llu / %llu (never shown: %llu)\nPresent after refresh: %.2f ms worst, %llu late",
                refreshes_per_frame,
                static_cast<unsigned long long>(shown_for[0]), static_cast<unsigned long long>(shown_for[1]),
                static_cast<unsigned long long>(shown_for[2]), static_cast<unsigned long long>(shown_for[3]),
                static_cast<unsigned long long>(never_shown),
                this->worst_present_us / 1000.0,
                static_cast<unsigned long long>(this->late_presents)
            );
            tooltip += detail;
        }
        this->status_bar_fps->setToolTip(tooltip);

        this->refresh_title();
    }

    std::uint32_t total_frames = 0;
    auto state = supershuckie_frontend_get_replay_state(this->frontend);

    if(state != SuperShuckieReplayState::SuperShuckieReplayState__NoReplay) {
        std::uint32_t ms_total = 0;
        std::uint32_t frames_total = 0;
        supershuckie_frontend_get_elapsed_time(this->frontend, &frames_total, &ms_total);
        this->status_bar_time->set_timestamp(ms_total);
        this->status_bar_time->show();
        this->replay_time_shown = true;
    }
    else {
        this->status_bar_time->hide();
        this->temporarily_paused = false;
    }

    if(state == SuperShuckieReplayState::SuperShuckieReplayState__Playback) {
        this->playback_bar->show();
    }
    else {
        this->playback_bar->hide();
    }

    char buf[1024];
    if(!supershuckie_frontend_is_pokeabyte_enabled(this->frontend, buf, sizeof(buf)) && buf[0] != 0) {
        this->set_title("Poke-A-Byte integration server error!");
    }

    if(!supershuckie_frontend_tick(this->frontend, buf, sizeof(buf))) {
        // The dialog's own event loop would otherwise let the 1 ms ticker re-enter tick() while
        // this error box is up.
        this->stop_timer();
        DISPLAY_ERROR_DIALOG_P(this, "Error!", "%s", buf);
        this->start_timer();
    }

    if(this->play_together != nullptr) {
        this->play_together->tick();
    }

    this->pause->setChecked(supershuckie_frontend_is_paused(this->frontend));

    // Keep the menu checkbox in sync with the setting, which may be toggled via a bound hotkey.
    this->swap_nds_screens->setChecked(supershuckie_frontend_get_swap_nds_screens(this->frontend));

    // The GBA and DS cores emit `speed` times more samples per second when sped up; the Game Boy
    // core already pitches its own. Only matters while sped-up audio is not muted (then the
    // emulator drops those samples and the device never sees them).
    if(this->audio->is_open()) {
        float ratio = 1.0f;
        if(!this->audio_mute_when_sped_up->isChecked() && this->audio->fast_forward_scales_pitch()) {
            ratio = this->audio->emulation_speed();
        }
        if(ratio != this->audio_frequency_ratio) {
            this->audio_frequency_ratio = ratio;
            this->audio->set_frequency_ratio(ratio);
        }
    }

    if(supershuckie_frontend_is_paused(this->frontend)) {
        this->paused_state->show();
    }
    else {
        this->paused_state->hide();
    }

    if(this->last_known_replay_state != state || this->last_known_replay_stopped != supershuckie_frontend_is_replay_playback_stopped(this->frontend)) {
        this->refresh_action_states();
    }

    this->playback_bar->tick();

    if(this->bookmark_window != nullptr && this->bookmark_window->isVisible()) {
        this->bookmark_window->tick();
    }

    if(--this->memory_status_countdown <= 0) {
        this->memory_status_countdown = 100;
        this->update_memory_status();
    }
}

void MainWindow::update_memory_status() {
    auto frozen = supershuckie_frontend_memory_frozen_count(this->frontend);
    if(frozen == 0) {
        this->frozen_state->hide();
    }
    else {
        this->frozen_state->setText(QString("%1 FROZEN").arg(frozen));
        this->frozen_state->show();
    }
    this->unfreeze_all->setEnabled(frozen > 0);
    bool confirm = supershuckie_frontend_memory_get_confirm_writes_while_recording(this->frontend);
    if(this->confirm_ram_writes->isChecked() != confirm) {
        this->confirm_ram_writes->setChecked(confirm);
    }

    auto writes = supershuckie_frontend_memory_writes_this_recording(this->frontend);
    this->ram_modified_state->setVisible(writes > 0);
    if(writes > 0) {
        this->ram_modified_state->setToolTip(QString("The RAM tools wrote to memory %1 time%2 in this recording (edits and freeze restores are part of the replay)").arg(writes).arg(writes == 1 ? "" : "s"));
    }
}

bool MainWindow::check_freezes_before_recording() {
    auto frozen = supershuckie_frontend_memory_frozen_count(this->frontend);
    if(frozen == 0) {
        return true;
    }
    QMessageBox box(this);
    box.setWindowTitle("Values are frozen");
    box.setIcon(QMessageBox::Question);
    box.setText(QString("%1 value%2 frozen by the RAM tools.").arg(frozen).arg(frozen == 1 ? " is" : "s are"));
    box.setInformativeText("While recording, the game changing a frozen value and the freeze restoring it are written into the replay.");
    auto *keep = box.addButton("Keep freezes", QMessageBox::AcceptRole);
    auto *unfreeze = box.addButton("Unfreeze all and record", QMessageBox::DestructiveRole);
    box.addButton(QMessageBox::Cancel);
    this->stop_timer();
    box.exec();
    this->start_timer();
    if(box.clickedButton() == unfreeze) {
        supershuckie_frontend_memory_unfreeze_all(this->frontend);
        return true;
    }
    return box.clickedButton() == keep;
}

void MainWindow::set_up_menu() {
    this->menu_bar = new QMenuBar(this);
    this->setMenuBar(this->menu_bar);

    // Add base menus
    this->set_up_file_menu();
    this->set_up_gameplay_menu();
    this->set_up_save_states_menu();
    this->set_up_replays_menu();
    this->set_up_audio_menu();
    this->set_up_tools_menu();
    this->set_up_play_together_menu();
    this->set_up_settings_menu();

    this->refresh_action_states();
    this->set_up_shortcuts();
}

void MainWindow::set_up_file_menu() {
    this->file_menu = this->menu_bar->addMenu("File");

    this->open_rom = this->file_menu->addAction("Open ROM…");
    this->open_rom->setObjectName("open-rom");
    this->open_rom->setShortcut(QKeyCombination(Qt::ControlModifier, Qt::Key_O));
    connect(this->open_rom, SIGNAL(triggered()), this, SLOT(do_open_rom()));

    this->recent_roms_menu = this->file_menu->addMenu("Open recent ROM");

    this->favorite_roms_menu = this->file_menu->addMenu("Open favorite ROM");
    this->favorite_roms_none = this->favorite_roms_menu->addAction("No favorites yet (add them on the start screen)");
    this->favorite_roms_none->setEnabled(false);

    this->close_rom = this->file_menu->addAction("Close ROM");
    this->close_rom->setObjectName("close-rom");
    this->close_rom->setShortcut(QKeyCombination(Qt::ControlModifier, Qt::Key_W));
    connect(this->close_rom, SIGNAL(triggered()), this, SLOT(do_close_rom()));

    this->unload_rom = this->file_menu->addAction("Unload ROM without saving");
    this->unload_rom->setObjectName("unload-rom");
    this->unload_rom->setShortcut(QKeyCombination(Qt::ControlModifier | Qt::ShiftModifier, Qt::Key_W));
    connect(this->unload_rom, SIGNAL(triggered()), this, SLOT(do_unload_rom()));

    this->file_menu->addSeparator();
    this->screenshot = this->file_menu->addAction("Screenshot");
    this->screenshot->setObjectName("screenshot");
    this->screenshot->setShortcut(QKeyCombination(Qt::Key_F12));
    connect(this->screenshot, SIGNAL(triggered()), this, SLOT(do_screenshot()));

    this->file_menu->addSeparator();
    auto *open_user_dir = this->file_menu->addAction("Open data directory");
    open_user_dir->setObjectName("open-data-directory");
    connect(open_user_dir, SIGNAL(triggered()), this, SLOT(do_open_user_dir()));

    this->quit = this->file_menu->addAction("Quit");
    this->quit->setObjectName("quit");
    this->quit->setShortcut(QKeyCombination(Qt::ControlModifier, Qt::Key_Q));
    connect(this->quit, SIGNAL(triggered()), this, SLOT(close()));
}

void MainWindow::set_up_gameplay_menu() {
    this->gameplay_menu = this->menu_bar->addMenu("Gameplay");

    this->new_game = this->gameplay_menu->addAction("New game…");
    this->new_game->setObjectName("new-game");
    this->new_game->setShortcut(QKeyCombination(Qt::ControlModifier, Qt::Key_N));
    connect(this->new_game, SIGNAL(triggered()), this, SLOT(do_new_game()));

    this->load_game = this->gameplay_menu->addAction("Load game…");
    this->load_game->setObjectName("load-game");
    connect(this->load_game, SIGNAL(triggered()), this, SLOT(do_load_game()));

    this->save_game = this->gameplay_menu->addAction("Save game");
    this->save_game->setObjectName("save-game");
    this->save_game->setShortcut(QKeyCombination(Qt::ControlModifier, Qt::Key_S));
    connect(this->save_game, SIGNAL(triggered()), this, SLOT(do_save_game()));

    this->save_new_game = this->gameplay_menu->addAction("Save as new game…");
    this->save_new_game->setObjectName("save-as-new-game");
    this->save_new_game->setShortcut(QKeyCombination(Qt::ControlModifier | Qt::ShiftModifier, Qt::Key_S));
    connect(this->save_new_game, SIGNAL(triggered()), this, SLOT(do_save_new_game()));

    this->gameplay_menu->addSeparator();

    this->reset_console = this->gameplay_menu->addAction("Reset console");
    this->reset_console->setObjectName("reset-console");
    connect(this->reset_console, SIGNAL(triggered()), this, SLOT(do_reset_console()));

    this->reload_core = this->gameplay_menu->addAction("Reload core");
    this->reload_core->setObjectName("reload-core");
    connect(this->reload_core, SIGNAL(triggered()), this, SLOT(do_reload_core()));

    // DS only: each preset sets the date and reloads the core so the game sees it at once
    // (see rebuild_nds_date_menu()).
    this->nds_date_menu = this->gameplay_menu->addMenu("Reload core with date");
    for(std::size_t i = 0; i < MainWindow::NDS_DATE_PRESET_SLOTS; i++) {
        auto *slot = this->nds_date_menu->addAction(QString("Date preset %1").arg(i + 1));
        slot->setObjectName(QString("nds-date-preset-%1").arg(i + 1));
        // The menu shows the preset's name instead; the Shortcuts window lists the slot.
        slot->setProperty(SHORTCUT_NAME_PROPERTY, slot->text());
        slot->setCheckable(true);
        slot->setVisible(false);
        connect(slot, &QAction::triggered, this, [this, i]() { this->apply_nds_date_preset(i); });
        this->nds_date_preset_slots[i] = slot;
    }
    this->nds_date_no_presets = this->nds_date_menu->addAction("No presets yet");
    this->nds_date_no_presets->setEnabled(false);
    this->nds_date_menu->addSeparator();
    auto *edit_nds_date_presets = this->nds_date_menu->addAction("Edit presets…");
    connect(edit_nds_date_presets, SIGNAL(triggered()), this, SLOT(do_open_nds_date_dialog()));

    this->pause = this->gameplay_menu->addAction("Pause");
    this->pause->setObjectName("pause");
    this->pause->setCheckable(true);
    this->pause->setShortcut(QKeyCombination(Qt::ControlModifier, Qt::Key_P));
    connect(this->pause, SIGNAL(triggered()), this, SLOT(do_toggle_pause()));

    this->gameplay_menu->addSeparator();
    this->auto_unpause_on_input = this->gameplay_menu->addAction("Unpause on input");
    this->auto_unpause_on_input->setObjectName("unpause-on-input");
    this->auto_unpause_on_input->setCheckable(true);
    connect(this->auto_unpause_on_input, SIGNAL(triggered()), this, SLOT(do_toggle_auto_unpause_on_input()));
}

void MainWindow::set_up_save_states_menu() {
    this->save_states_menu = this->menu_bar->addMenu("Save states");

    // One submenu per verb rather than one per slot: a slot is two levels down instead of three,
    // and every slot's shortcut is visible at once.
    auto *load_menu = this->save_states_menu->addMenu("Load quick slot");
    auto *save_menu = this->save_states_menu->addMenu("Save quick slot");
    for(std::size_t i = 1; i <= MainWindow::QUICK_SAVE_STATE_COUNT; i++) {
        char fmt[64];
        std::snprintf(fmt, sizeof(fmt), "Slot #%zu", i);

        auto *quick_load = new NumberedAction(this, fmt, i, &MainWindow::quick_load);
        quick_load->setObjectName(QString("quick-load-%1").arg(i));
        this->quick_load_save_states[i - 1] = quick_load;
        load_menu->addAction(quick_load);

        auto *quick_save = new NumberedAction(this, fmt, i, &MainWindow::quick_save);
        quick_save->setObjectName(QString("quick-save-%1").arg(i));
        this->quick_save_save_states[i - 1] = quick_save;
        save_menu->addAction(quick_save);
    }

    this->save_states_menu->addSeparator();

    this->undo_load_save_state = this->save_states_menu->addAction("Undo load save state");
    this->undo_load_save_state->setObjectName("undo-load-save-state");
    this->undo_load_save_state->setShortcut(QKeyCombination(Qt::ControlModifier, Qt::Key_U));
    connect(this->undo_load_save_state, SIGNAL(triggered()), this, SLOT(do_undo_load_save_state()));

    this->redo_load_save_state = this->save_states_menu->addAction("Redo load save state");
    this->redo_load_save_state->setObjectName("redo-load-save-state");
    this->redo_load_save_state->setShortcut(QKeyCombination(Qt::ControlModifier | Qt::ShiftModifier, Qt::Key_U));
    connect(this->redo_load_save_state, SIGNAL(triggered()), this, SLOT(do_redo_load_save_state()));

    this->save_states_menu->addSeparator();

    this->use_number_row_for_quick_slots = this->save_states_menu->addAction("Use number row instead of function keys");
    this->use_number_row_for_quick_slots->setObjectName("quick-slots-use-number-row");
    this->use_number_row_for_quick_slots->setCheckable(true);
    connect(this->use_number_row_for_quick_slots, SIGNAL(triggered()), this, SLOT(do_toggle_number_row_for_save_states()));
}

void MainWindow::set_up_replays_menu() {
    this->replays_menu = this->menu_bar->addMenu("Replays");

    // Actions first, in the order they get used (record, play, control playback, annotate, export);
    // the checkable options live in the two submenus at the bottom.
    this->record_replay = this->replays_menu->addAction("Record (unset)");
    this->record_replay->setObjectName("record-replay");
    this->record_replay->setShortcut(QKeyCombination(Qt::ControlModifier, Qt::Key_R));
    connect(this->record_replay, SIGNAL(triggered()), this, SLOT(do_record_replay()));

    this->resume_replay = this->replays_menu->addAction("Resume recording replay");
    this->resume_replay->setObjectName("resume-replay");
    this->resume_replay->setShortcut(QKeyCombination(Qt::ShiftModifier | Qt::ControlModifier, Qt::Key_R));
    connect(this->resume_replay, SIGNAL(triggered()), this, SLOT(do_resume_replay()));

    this->replays_menu->addSeparator();

    // "Play replay" stays available while a replay is already playing: picking another one
    // replaces it (the frontend detaches the old replay itself), so closing is a separate action.
    // Stopping playback without closing the replay is the timeline's stop button.
    this->play_replay = this->replays_menu->addAction("Play replay");
    this->play_replay->setObjectName("play-replay");
    this->play_replay->setShortcut(QKeyCombination(Qt::ShiftModifier | Qt::ControlModifier, Qt::Key_P));
    connect(this->play_replay, SIGNAL(triggered()), this, SLOT(do_play_replay()));

    this->continue_last_replay = this->replays_menu->addAction("Continue last replay");
    this->continue_last_replay->setObjectName("continue-last-replay");
    this->continue_last_replay->setShortcut(QKeyCombination(Qt::ShiftModifier | Qt::ControlModifier, Qt::Key_C));
    connect(this->continue_last_replay, SIGNAL(triggered()), this, SLOT(do_continue_last_replay()));

    this->close_replay = this->replays_menu->addAction("Close replay");
    this->close_replay->setObjectName("close-replay");
    connect(this->close_replay, SIGNAL(triggered()), this, SLOT(do_close_replay()));

    this->replays_menu->addSeparator();

    // The timeline's buttons (see ReplayPlaybackControls), here so they are rebindable. Their
    // shortcuts are modified so they never collide with the keys the game is being played with.
    this->stop_playback = this->replays_menu->addAction("Stop playback (take control)");
    this->stop_playback->setObjectName("stop-playback");
    this->stop_playback->setShortcut(QKeyCombination(Qt::ShiftModifier | Qt::ControlModifier, Qt::Key_T));
    connect(this->stop_playback, SIGNAL(triggered()), this, SLOT(do_stop_playback()));

    this->resume_playback = this->replays_menu->addAction("Resume playback from the resume point");
    this->resume_playback->setObjectName("resume-playback");
    this->resume_playback->setShortcut(QKeyCombination(Qt::ShiftModifier | Qt::ControlModifier, Qt::Key_G));
    connect(this->resume_playback, SIGNAL(triggered()), this, SLOT(do_resume_playback()));

    this->go_to_resume_point = this->replays_menu->addAction("Go back to the resume point");
    this->go_to_resume_point->setObjectName("go-to-resume-point");
    this->go_to_resume_point->setShortcut(QKeyCombination(Qt::ShiftModifier | Qt::ControlModifier, Qt::Key_J));
    connect(this->go_to_resume_point, SIGNAL(triggered()), this, SLOT(do_go_to_resume_point()));

    this->replays_menu->addSeparator();

    // Bookmarks: BookmarkWindow adds these actions to itself so the shortcuts work there too.
    auto *bookmarks_menu = this->replays_menu->addMenu("Bookmarks");

    this->add_bookmark = bookmarks_menu->addAction("Add bookmark");
    this->add_bookmark->setObjectName("add-bookmark");
    this->add_bookmark->setShortcut(QKeyCombination(Qt::ControlModifier, Qt::Key_B));
    connect(this->add_bookmark, SIGNAL(triggered()), this, SLOT(do_add_bookmark()));

    this->add_keyframe_bookmark = bookmarks_menu->addAction("Add keyframe bookmark");
    this->add_keyframe_bookmark->setObjectName("add-keyframe-bookmark");
    this->add_keyframe_bookmark->setShortcut(QKeyCombination(Qt::ControlModifier | Qt::ShiftModifier, Qt::Key_B));
    connect(this->add_keyframe_bookmark, SIGNAL(triggered()), this, SLOT(do_add_keyframe_bookmark()));

    this->toggle_range_bookmark = bookmarks_menu->addAction("Start/end range bookmark");
    this->toggle_range_bookmark->setObjectName("toggle-range-bookmark");
    this->toggle_range_bookmark->setShortcut(QKeyCombination(Qt::ControlModifier | Qt::AltModifier, Qt::Key_B));
    connect(this->toggle_range_bookmark, SIGNAL(triggered()), this, SLOT(do_toggle_range_bookmark()));

    this->add_bookmark_at_frame = bookmarks_menu->addAction("Add bookmark at frame…");
    this->add_bookmark_at_frame->setObjectName("add-bookmark-at-frame");
    connect(this->add_bookmark_at_frame, SIGNAL(triggered()), this, SLOT(do_add_bookmark_at_frame()));

    bookmarks_menu->addSeparator();

    this->open_bookmarks = bookmarks_menu->addAction("Show bookmarks…");
    this->open_bookmarks->setObjectName("bookmarks");
    this->open_bookmarks->setShortcut(QKeyCombination(Qt::ControlModifier | Qt::AltModifier | Qt::ShiftModifier, Qt::Key_B));
    connect(this->open_bookmarks, SIGNAL(triggered()), this, SLOT(do_open_bookmarks()));

    this->replays_menu->addSeparator();

    this->export_video = this->replays_menu->addAction("Export video…");
    this->export_video->setObjectName("export-video");
    this->export_video->setShortcut(QKeyCombination(Qt::ShiftModifier | Qt::ControlModifier, Qt::Key_E));
    connect(this->export_video, SIGNAL(triggered()), this, SLOT(do_export_video()));

    auto *convert_menu = this->replays_menu->addMenu("Convert to current format");

    this->convert_replay = convert_menu->addAction("Replay…");
    this->convert_replay->setObjectName("convert-replay");
    connect(this->convert_replay, SIGNAL(triggered()), this, SLOT(do_convert_replay()));

    this->convert_replay_folder = convert_menu->addAction("Folder of replays…");
    this->convert_replay_folder->setObjectName("convert-replay-folder");
    connect(this->convert_replay_folder, SIGNAL(triggered()), this, SLOT(do_convert_replay_folder()));

    this->replays_menu->addSeparator();

    auto *recording_options = this->replays_menu->addMenu("Recording options");

    this->auto_pause_on_record = recording_options->addAction("Start recordings paused");
    this->auto_pause_on_record->setObjectName("start-recordings-paused");
    this->auto_pause_on_record->setCheckable(true);
    connect(this->auto_pause_on_record, SIGNAL(triggered()), this, SLOT(do_toggle_auto_pause_on_record()));

    this->disable_save_states_when_recording = recording_options->addAction("Disable save states when recording");
    this->disable_save_states_when_recording->setObjectName("disable-save-states-when-recording");
    this->disable_save_states_when_recording->setCheckable(true);
    connect(this->disable_save_states_when_recording, SIGNAL(triggered()), this, SLOT(do_toggle_disable_save_states_when_recording()));

    this->disable_speed_changes_when_recording = recording_options->addAction("Disable speed changes when recording");
    this->disable_speed_changes_when_recording->setObjectName("disable-speed-changes-when-recording");
    this->disable_speed_changes_when_recording->setCheckable(true);
    connect(this->disable_speed_changes_when_recording, SIGNAL(triggered()), this, SLOT(do_toggle_disable_speed_changes_when_recording()));

    recording_options->addSeparator();

    // zstd level for new recordings and conversions. Only 19 buys anything over 9 (about 10% smaller
    // files) and it costs roughly twice the conversion time; 3 is what pre-v4 versions used.
    auto *compression_items = recording_options->addMenu("Compression");
    this->replay_compression_levels[0] = new NumberedAction(this, "Fastest (level 1)", 1, &MainWindow::set_replay_compression_level);
    this->replay_compression_levels[1] = new NumberedAction(this, "Fast (level 3)", 3, &MainWindow::set_replay_compression_level);
    this->replay_compression_levels[2] = new NumberedAction(this, "Balanced (level 9, default)", 9, &MainWindow::set_replay_compression_level);
    this->replay_compression_levels[3] = new NumberedAction(this, "Smallest (level 19, slow to write)", 19, &MainWindow::set_replay_compression_level);
    for(auto *level : this->replay_compression_levels) {
        level->setObjectName(QString("replay-compression-%1").arg(level->number));
        level->setCheckable(true);
        compression_items->addAction(level);
    }
    // Shown (checked and disabled) only when settings.json holds a level that is not one of the above.
    this->replay_compression_custom = compression_items->addAction("Custom");
    this->replay_compression_custom->setCheckable(true);
    this->replay_compression_custom->setEnabled(false);
    this->replay_compression_custom->setVisible(false);

    auto *playback_options = this->replays_menu->addMenu("Playback options");

    this->auto_stop_replay_on_input = playback_options->addAction("Stop playback on input");
    this->auto_stop_replay_on_input->setObjectName("stop-playback-on-input");
    this->auto_stop_replay_on_input->setCheckable(true);
    connect(this->auto_stop_replay_on_input, SIGNAL(triggered()), this, SLOT(do_toggle_stop_replay_on_input()));

    this->keyboard_replay_controls = playback_options->addAction("Allow keyboard to control playback");
    this->keyboard_replay_controls->setObjectName("keyboard-replay-controls");
    this->keyboard_replay_controls->setCheckable(true);
    this->keyboard_replay_controls->setChecked(true);
    connect(this->keyboard_replay_controls, SIGNAL(triggered()), this, SLOT(do_toggle_replay_keyboard_controls()));

    this->ignore_speed_changes_in_replay = playback_options->addAction("Ignore speed changes in replay");
    this->ignore_speed_changes_in_replay->setObjectName("ignore-speed-changes-in-replay");
    this->ignore_speed_changes_in_replay->setCheckable(true);
    connect(this->ignore_speed_changes_in_replay, SIGNAL(triggered()), this, SLOT(do_toggle_ignore_speed_changes_in_replay()));

    this->auto_resync_keyframes_in_replay = playback_options->addAction("Auto-resync keyframes in replay");
    this->auto_resync_keyframes_in_replay->setObjectName("auto-resync-keyframes-in-replay");
    this->auto_resync_keyframes_in_replay->setCheckable(true);
    connect(this->auto_resync_keyframes_in_replay, SIGNAL(triggered()), this, SLOT(do_toggle_auto_resync_keyframes_in_replay()));
}

NumberedAction::NumberedAction(MainWindow *parent, const char *text, std::uint8_t number, on_activated activated): QAction(text, parent), number(number), parent(parent), activated_fn(activated) {
    connect(this, SIGNAL(triggered()), this, SLOT(activated()));
}

void NumberedAction::activated() {
    if(this->parent->frontend == nullptr) {
        return;
    }
    (this->parent->*this->activated_fn)(this->number);
}

StringAction::StringAction(MainWindow *parent, const char *text, const char *string, on_activated activated): QAction(text, parent), string(string), parent(parent), activated_fn(activated) {
    connect(this, SIGNAL(triggered()), this, SLOT(activated()));
}

void StringAction::activated() {
    if(this->parent->frontend == nullptr) {
        return;
    }
    (this->parent->*this->activated_fn)(this->string.c_str());
}

void MainWindow::set_video_scale(std::uint8_t scale) {
    supershuckie_frontend_set_video_scale(this->frontend, scale);
}

void MainWindow::make_save_state(const char *state) {
    char error[256];
    auto success = supershuckie_frontend_create_save_state(this->frontend, state, error, sizeof(error));
    if(success) {
        char title[512];
        std::snprintf(title, sizeof(title), "Created state \"%s\"", error);
        this->set_title(title);
    }
    else {
        this->show_error("Failed to create save state", "%s", error);
    }
}

void MainWindow::load_save_state(const char *state) {
    char error[256];
    auto success = supershuckie_frontend_load_save_state(this->frontend, state, error, sizeof(error));
    if(success) {
        char title[512];
        std::snprintf(title, sizeof(title), "Loaded state \"%s\"", state);
        this->set_title(title);
    }
    else if(error[0] != 0) {
        this->show_error("Failed to load save state", "%s", error);
    }
    else {
        char title[512];
        std::snprintf(title, sizeof(title), "State \"%s\" does not exist", state);
        this->set_title(title);
    }
}

void MainWindow::quick_save(std::uint8_t index) {
    char fmt[16];
    std::snprintf(fmt, sizeof(fmt), "quick-%d", index);
    this->make_save_state(fmt);
}

void MainWindow::quick_load(std::uint8_t index) {
    char fmt[16];
    std::snprintf(fmt, sizeof(fmt), "quick-%d", index);
    this->load_save_state(fmt);
}

const std::uint16_t MainWindow::audio_buffer_ms[MainWindow::AUDIO_BUFFER_PRESETS] = { 32, 64, 128 };

void MainWindow::set_up_audio_menu() {
    this->audio_menu = this->menu_bar->addMenu("Audio");

    this->audio_enabled = this->audio_menu->addAction("Enable audio");
    this->audio_enabled->setObjectName("enable-audio");
    this->audio_enabled->setCheckable(true);
    connect(this->audio_enabled, SIGNAL(triggered()), this, SLOT(do_toggle_audio_enabled()));

    this->audio_muted = this->audio_menu->addAction("Mute");
    this->audio_muted->setObjectName("mute");
    this->audio_muted->setCheckable(true);
    connect(this->audio_muted, SIGNAL(triggered()), this, SLOT(do_toggle_audio_muted()));

    // Someone who turbos through one stretch and plays the next at 1x should not get a barrage of
    // sped-up audio in between; the emulator drops the samples while the speed is not 1x.
    this->audio_mute_when_sped_up = this->audio_menu->addAction("Mute when sped up");
    this->audio_mute_when_sped_up->setObjectName("mute-when-sped-up");
    this->audio_mute_when_sped_up->setCheckable(true);
    connect(this->audio_mute_when_sped_up, SIGNAL(triggered()), this, SLOT(do_toggle_audio_mute_when_sped_up()));

    this->audio_volume_menu = this->audio_menu->addMenu("Volume");
    for(std::size_t i = 0; i < MainWindow::AUDIO_VOLUME_STEPS; i++) {
        auto percent = static_cast<std::uint8_t>((i + 1) * 100 / MainWindow::AUDIO_VOLUME_STEPS);
        char fmt[32];
        std::snprintf(fmt, sizeof(fmt), "%u%%", static_cast<unsigned>(percent));
        auto *action = new NumberedAction(this, fmt, percent, &MainWindow::set_audio_volume);
        action->setObjectName(QString("volume-%1").arg(percent));
        action->setCheckable(true);
        this->audio_volume_menu->addAction(action);
        this->audio_volumes[i] = action;
    }

    this->audio_menu->addSeparator();

    // How much audio may queue between the emulator and the device. Smaller is snappier but
    // has less slack for a busy frame.
    auto *buffer_menu = this->audio_menu->addMenu("Buffer");
    const char *buffer_names[MainWindow::AUDIO_BUFFER_PRESETS] = { "Low (32 ms)", "Normal (64 ms)", "High (128 ms)" };
    for(std::size_t i = 0; i < MainWindow::AUDIO_BUFFER_PRESETS; i++) {
        auto *action = new NumberedAction(this, buffer_names[i], static_cast<std::uint8_t>(i), &MainWindow::set_audio_buffer);
        action->setObjectName(QString("audio-buffer-%1ms").arg(MainWindow::audio_buffer_ms[i]));
        action->setCheckable(true);
        buffer_menu->addAction(action);
        this->audio_buffers[i] = action;
    }
}

void MainWindow::set_up_tools_menu() {
    this->tools_menu = this->menu_bar->addMenu("Tools");

    auto *ram_viewer = this->tools_menu->addAction("RAM viewer");
    ram_viewer->setObjectName("ram-viewer");
    ram_viewer->setShortcut(QKeyCombination(Qt::ControlModifier | Qt::AltModifier, Qt::Key_V));
    connect(ram_viewer, SIGNAL(triggered()), this, SLOT(do_open_ram_viewer()));

    auto *new_ram_viewer = this->tools_menu->addAction("New RAM viewer window");
    new_ram_viewer->setObjectName("new-ram-viewer");
    connect(new_ram_viewer, SIGNAL(triggered()), this, SLOT(do_new_ram_viewer()));

    auto *ram_search = this->tools_menu->addAction("RAM search");
    ram_search->setObjectName("ram-search");
    ram_search->setShortcut(QKeyCombination(Qt::ControlModifier | Qt::AltModifier, Qt::Key_F));
    connect(ram_search, SIGNAL(triggered()), this, SLOT(do_open_ram_search()));

    auto *ram_watch = this->tools_menu->addAction("RAM watch");
    ram_watch->setObjectName("ram-watch");
    ram_watch->setShortcut(QKeyCombination(Qt::ControlModifier | Qt::AltModifier, Qt::Key_W));
    connect(ram_watch, SIGNAL(triggered()), this, SLOT(do_open_ram_watch()));

    this->tools_menu->addSeparator();

    this->unfreeze_all = this->tools_menu->addAction("Unfreeze all");
    this->unfreeze_all->setObjectName("unfreeze-all");
    connect(this->unfreeze_all, SIGNAL(triggered()), this, SLOT(do_unfreeze_all()));
    this->unfreeze_all->setEnabled(false);

    this->confirm_ram_writes = this->tools_menu->addAction("Ask before editing memory while recording");
    this->confirm_ram_writes->setObjectName("confirm-ram-writes-while-recording");
    this->confirm_ram_writes->setCheckable(true);
    this->confirm_ram_writes->setChecked(true);
    connect(this->confirm_ram_writes, SIGNAL(triggered()), this, SLOT(do_toggle_confirm_ram_writes()));

    this->tools_menu->addSeparator();

    auto *open_tables = this->tools_menu->addAction("Open character tables folder");
    open_tables->setObjectName("open-character-tables-folder");
    connect(open_tables, SIGNAL(triggered()), this, SLOT(do_open_tables_folder()));

    auto *reload_tables = this->tools_menu->addAction("Reload character tables");
    reload_tables->setObjectName("reload-character-tables");
    connect(reload_tables, SIGNAL(triggered()), this, SLOT(do_reload_tables()));
}

QWidget *MainWindow::bookmark_dialog_parent() {
    if(this->bookmark_window != nullptr && this->bookmark_window->isActiveWindow()) {
        return this->bookmark_window;
    }
    return this;
}

void MainWindow::add_bookmark_now(bool keyframe) {
    auto *frontend = this->frontend;
    const char *request = keyframe ? "{\"keyframe\":true}" : "{}";
    auto result = BookmarkWindow::run_operation(this, this->bookmark_dialog_parent(), [frontend, request](bool allow_upgrade, char *out, std::size_t out_len) {
        return supershuckie_frontend_bookmark_add_json(frontend, request, allow_upgrade, out, out_len);
    });
    if(!result.has_value()) {
        return;
    }
    auto text = QString("Added %1bookmark \"%2\" at frame %3")
        .arg(keyframe ? "keyframe " : "")
        .arg((*result)["name"].toString())
        .arg(static_cast<qulonglong>((*result)["in_frame"].toDouble()));
    this->set_title(text.toUtf8().constData());
}

void MainWindow::do_add_bookmark() {
    this->add_bookmark_now(false);
}

void MainWindow::do_add_keyframe_bookmark() {
    this->add_bookmark_now(true);
}

void MainWindow::do_toggle_range_bookmark() {
    auto *frontend = this->frontend;
    auto result = BookmarkWindow::run_operation(this, this->bookmark_dialog_parent(), [frontend](bool allow_upgrade, char *out, std::size_t out_len) {
        return supershuckie_frontend_bookmark_toggle_range_json(frontend, "{}", allow_upgrade, out, out_len);
    });
    if(!result.has_value()) {
        return;
    }
    auto bookmark = (*result)["bookmark"].toObject();
    auto in_frame = static_cast<qulonglong>(bookmark["in_frame"].toDouble());
    QString text;
    if((*result)["started"].toBool()) {
        text = QString("Started range bookmark \"%1\" at frame %2").arg(bookmark["name"].toString()).arg(in_frame);
    }
    else {
        text = QString("Ended range bookmark \"%1\" (frames %2 to %3)").arg(bookmark["name"].toString()).arg(in_frame).arg(static_cast<qulonglong>(bookmark["out_frame"].toDouble()));
    }
    this->set_title(text.toUtf8().constData());
}

void MainWindow::do_add_bookmark_at_frame() {
    AddBookmarkDialog dialog(this, this->bookmark_dialog_parent());
    if(dialog.exec() != QDialog::Accepted) {
        return;
    }
    auto *frontend = this->frontend;
    auto request = QJsonDocument(dialog.request()).toJson(QJsonDocument::Compact).toStdString();
    auto result = BookmarkWindow::run_operation(this, this->bookmark_dialog_parent(), [frontend, &request](bool allow_upgrade, char *out, std::size_t out_len) {
        return supershuckie_frontend_bookmark_add_json(frontend, request.c_str(), allow_upgrade, out, out_len);
    });
    if(result.has_value()) {
        auto text = QString("Added bookmark \"%1\" at frame %2").arg((*result)["name"].toString()).arg(static_cast<qulonglong>((*result)["in_frame"].toDouble()));
        this->set_title(text.toUtf8().constData());
    }
}

void MainWindow::do_open_bookmarks() {
    if(this->bookmark_window == nullptr) {
        this->bookmark_window = new BookmarkWindow(this);
    }
    this->bookmark_window->show();
    this->bookmark_window->raise();
    this->bookmark_window->activateWindow();
    this->bookmark_window->tick();
}

void MainWindow::do_open_ram_viewer() {
    if(this->memory_tools != nullptr) {
        this->memory_tools->open_viewer();
    }
}

void MainWindow::do_open_ram_search() {
    if(this->memory_tools != nullptr) {
        this->memory_tools->open_search();
    }
}

void MainWindow::do_unfreeze_all() {
    supershuckie_frontend_memory_unfreeze_all(this->frontend);
    this->update_memory_status();
}

void MainWindow::do_toggle_confirm_ram_writes() {
    supershuckie_frontend_memory_set_confirm_writes_while_recording(this->frontend, this->confirm_ram_writes->isChecked());
}

void MainWindow::do_open_ram_watch() {
    if(this->memory_tools != nullptr) {
        this->memory_tools->open_watch();
    }
}

void MainWindow::do_new_ram_viewer() {
    if(this->memory_tools != nullptr && this->memory_tools->new_viewer() == nullptr) {
        this->set_title("All RAM viewer windows are already open");
    }
}

void MainWindow::do_open_tables_folder() {
    if(this->memory_tools != nullptr) {
        this->memory_tools->open_tables_folder();
    }
}

void MainWindow::do_reload_tables() {
    if(this->memory_tools != nullptr) {
        this->memory_tools->reload_tables();
    }
}

void MainWindow::set_up_settings_menu() {
    this->settings_menu = this->menu_bar->addMenu("Settings");

    auto *game_speed = this->settings_menu->addAction("Game speed…");
    game_speed->setObjectName("game-speed");
    connect(game_speed, SIGNAL(triggered()), this, SLOT(do_open_game_speed_dialog()));

    auto *controller_settings = this->settings_menu->addAction("Controls…");
    controller_settings->setObjectName("controls");
    connect(controller_settings, SIGNAL(triggered()), this, SLOT(do_open_controls_settings_dialog()));

    auto *shortcut_settings = this->settings_menu->addAction("Shortcuts…");
    shortcut_settings->setObjectName("shortcuts");
    connect(shortcut_settings, SIGNAL(triggered()), this, SLOT(do_open_shortcuts_dialog()));

    this->settings_menu->addSeparator();

    auto *video_scaling = this->settings_menu->addMenu("Video scaling");
    for(std::size_t i = 1; i <= MainWindow::VIDEO_SCALE_COUNT; i++) {
        char fmt[256];
        std::snprintf(fmt, sizeof(fmt), "%zux", i);

        auto *action = new NumberedAction(this, fmt, static_cast<uint8_t>(i), &MainWindow::set_video_scale);
        action->setObjectName(QString("video-scale-%1").arg(i));
        video_scaling->addAction(action);
        this->change_video_scale[i - 1] = action;
        action->setCheckable(true);
    }

    this->sync_display_to_refresh = this->settings_menu->addAction("Sync display to monitor refresh");
    this->sync_display_to_refresh->setObjectName("sync-display-to-refresh");
    this->sync_display_to_refresh->setCheckable(true);
    this->sync_display_to_refresh->setToolTip("Show one emulated frame per display refresh instead of each frame as it arrives; evens out periodic judder");
    connect(this->sync_display_to_refresh, SIGNAL(triggered()), this, SLOT(do_toggle_sync_display()));

    this->show_status_bar = this->settings_menu->addAction("Show status bar");
    this->show_status_bar->setObjectName("show-status-bar");
    this->show_status_bar->setCheckable(true);
    connect(this->show_status_bar, SIGNAL(triggered()), this, SLOT(do_toggle_status_bar()));

    this->settings_menu->addSeparator();

    this->game_boy_settings = this->settings_menu->addMenu("Game Boy");

    this->gbc_mode_items = this->game_boy_settings->addMenu("Game Boy Color mode");

    this->gbc_mode[0] = new NumberedAction(this, "Always Game Boy Color", SuperShuckieGBCMode::SuperShuckieGBCMode__AlwaysGBC, &MainWindow::set_gbc_mode);
    this->gbc_mode[1] = new NumberedAction(this, "Game Boy Color games only", SuperShuckieGBCMode::SuperShuckieGBCMode__GBInGBMode, &MainWindow::set_gbc_mode);
    this->gbc_mode[2] = new NumberedAction(this, "Always Game Boy", SuperShuckieGBCMode::SuperShuckieGBCMode__AlwaysGB, &MainWindow::set_gbc_mode);
    this->gbc_mode[2]->setToolTip("Game Boy Color-only games (such as Pokemon Crystal) cannot run as a Game Boy and always use Game Boy Color mode");

    for(auto m : this->gbc_mode) {
        m->setObjectName(QString("gbc-mode-%1").arg(m->number));
        m->setCheckable(true);
        this->gbc_mode_items->addAction(m);
    }

    this->sgb_enabled = this->game_boy_settings->addAction("Enable SGB colors");
    this->sgb_enabled->setObjectName("enable-sgb-colors");
    connect(this->sgb_enabled, SIGNAL(triggered()), this, SLOT(do_toggle_sgb()));
    this->sgb_enabled->setCheckable(true);

    this->game_boy_settings->addSeparator();

    this->gb_custom_colors = this->game_boy_settings->addAction("Use custom colors");
    this->gb_custom_colors->setObjectName("gb-use-custom-colors");
    this->gb_custom_colors->setCheckable(true);
    this->gb_custom_colors->setToolTip("Draw Game Boy games (and Game Boy games on a Game Boy Color) with the colors from Custom colors…");
    connect(this->gb_custom_colors, SIGNAL(triggered()), this, SLOT(do_toggle_gb_custom_colors()));

    auto *custom_colors = this->game_boy_settings->addAction("Custom colors…");
    custom_colors->setObjectName("gb-custom-colors");
    custom_colors->setToolTip("Choose the twelve colors a Game Boy game is drawn with");
    connect(custom_colors, SIGNAL(triggered()), this, SLOT(do_open_gb_palette_dialog()));

    auto *nds_settings = this->settings_menu->addMenu("Nintendo DS");

    auto *set_nds_date = nds_settings->addAction("Set date…");
    set_nds_date->setObjectName("nds-set-date");
    connect(set_nds_date, SIGNAL(triggered()), this, SLOT(do_open_nds_date_dialog()));

    this->horizontal_nds = nds_settings->addAction("Arrange horizontally");
    this->horizontal_nds->setObjectName("nds-arrange-horizontally");
    this->horizontal_nds->setCheckable(true);
    connect(this->horizontal_nds, SIGNAL(triggered()), this, SLOT(do_toggle_horizontal_nds()));

    this->swap_nds_screens = nds_settings->addAction("Swap screens");
    this->swap_nds_screens->setObjectName("nds-swap-screens");
    this->swap_nds_screens->setCheckable(true);
    connect(this->swap_nds_screens, SIGNAL(triggered()), this, SLOT(do_toggle_swap_nds_screens()));

    this->nds_jit = nds_settings->addAction("Enable JIT (disables replays)");
    this->nds_jit->setObjectName("nds-enable-jit");
    this->nds_jit->setCheckable(true);
    connect(this->nds_jit, SIGNAL(triggered()), this, SLOT(do_toggle_nds_jit()));

    this->settings_menu->addSeparator();

    auto *pokeabyte_menu = this->settings_menu->addMenu("Poke-A-Byte");

    this->enable_pokeabyte_integration = pokeabyte_menu->addAction("Enable integration");
    this->enable_pokeabyte_integration->setObjectName("enable-pokeabyte-integration");
    this->enable_pokeabyte_integration->setCheckable(true);
    connect(this->enable_pokeabyte_integration, SIGNAL(triggered()), this, SLOT(do_toggle_pokeabyte()));

    // Once a frontend is loaded the label also shows the current port (see the setText calls).
    this->pokeabyte_port = pokeabyte_menu->addAction("Port…");
    this->pokeabyte_port->setObjectName("pokeabyte-port");
    this->pokeabyte_port->setToolTip("The UDP port this game is served to Poke-A-Byte on (Poke-A-Byte connects to 55356 unless told otherwise)");
    connect(this->pokeabyte_port, SIGNAL(triggered()), this, SLOT(do_set_pokeabyte_port()));

    this->pokeabyte_serve_friends = pokeabyte_menu->addAction("Serve friends' games");
    this->pokeabyte_serve_friends->setObjectName("pokeabyte-serve-friends");
    this->pokeabyte_serve_friends->setCheckable(true);
    this->pokeabyte_serve_friends->setToolTip("In a Play Together session, serve each friend's game on its own port above the Poke-A-Byte port (right-click a friend's window to see which)");
    connect(this->pokeabyte_serve_friends, SIGNAL(triggered()), this, SLOT(do_toggle_pokeabyte_serve_friends()));

    this->enable_external_commands = this->settings_menu->addAction("Enable external commands");
    this->enable_external_commands->setObjectName("enable-external-commands");
    this->enable_external_commands->setCheckable(true);
    connect(this->enable_external_commands, SIGNAL(triggered()), this, SLOT(do_toggle_external_commands()));
}

void MainWindow::refresh_action_states() {
    bool game_loaded = this->is_game_running();

    auto replay_state = this->frontend != nullptr ?
        supershuckie_frontend_get_replay_state(this->frontend) : SuperShuckieReplayState::SuperShuckieReplayState__NoReplay;

    // A loaded replay is either driving the game (playing back) or stopped, with the game live
    // under the user; only the former takes anything away from the user.
    bool replay_stopped = this->frontend != nullptr && supershuckie_frontend_is_replay_playback_stopped(this->frontend);
    bool replay_playing = replay_state == SuperShuckieReplayState::SuperShuckieReplayState__Playback && !replay_stopped;

    this->gameplay_menu->setEnabled(game_loaded);
    this->replays_menu->setEnabled(true);
    this->close_rom->setEnabled(game_loaded);
    this->unload_rom->setEnabled(game_loaded);
    this->screenshot->setEnabled(game_loaded);

    for(auto &state : this->quick_save_save_states) {
        state->setEnabled(game_loaded);
    }

    // prevent loading any save states if playing back OR recording and it is disabled
    bool enable_load_save_state_buttons = this->frontend != nullptr
        && (!supershuckie_frontend_get_disable_save_states_when_recording(this->frontend) || replay_state != SuperShuckieReplayState::SuperShuckieReplayState__Recording)
        && !replay_playing;

    for(auto &state : this->quick_load_save_states) {
        state->setEnabled(enable_load_save_state_buttons);
    }

    this->redo_load_save_state->setEnabled(enable_load_save_state_buttons);
    this->undo_load_save_state->setEnabled(enable_load_save_state_buttons);

    this->record_replay->setText("Record replay");

    this->play_replay->setEnabled(game_loaded);
    this->close_replay->setEnabled(false);
    this->stop_playback->setEnabled(false);
    this->resume_playback->setEnabled(false);
    this->go_to_resume_point->setEnabled(false);
    this->record_replay->setEnabled(game_loaded);
    this->resume_replay->setEnabled(game_loaded);
    this->export_video->setEnabled(game_loaded);
    this->convert_replay->setEnabled(true);
    this->convert_replay_folder->setEnabled(true);
    this->set_game_boy_hardware_settings_enabled(true);

    this->reload_core->setEnabled(game_loaded);
    this->reset_console->setEnabled(game_loaded);
    this->nds_date_menu->menuAction()->setVisible(this->is_nds_game_running());

    for(auto &scale : this->change_video_scale) {
        scale->setEnabled(game_loaded);
    }

    auto gbc_mode = this->frontend != nullptr ? supershuckie_frontend_get_gbc_mode(this->frontend) : 0;
    for(auto &i : this->gbc_mode) {
        i->setChecked(i->number == gbc_mode);
    }

    this->continue_last_replay->setEnabled(this->frontend != nullptr && supershuckie_frontend_can_continue_last_replay(this->frontend));

    bool bookmarks_available = game_loaded && replay_state != SuperShuckieReplayState::SuperShuckieReplayState__NoReplay;
    for(auto *action : { this->add_bookmark, this->add_keyframe_bookmark, this->toggle_range_bookmark, this->add_bookmark_at_frame }) {
        action->setEnabled(bookmarks_available);
    }

    auto volume = this->frontend != nullptr ? supershuckie_frontend_get_audio_volume(this->frontend) : 100;
    for(auto *v : this->audio_volumes) {
        v->setChecked(v->number == volume);
    }
    auto latency = this->frontend != nullptr ? supershuckie_frontend_get_audio_latency_ms(this->frontend) : 64;
    for(std::size_t i = 0; i < MainWindow::AUDIO_BUFFER_PRESETS; i++) {
        this->audio_buffers[i]->setChecked(MainWindow::audio_buffer_ms[i] == latency);
    }
    bool audio_on = this->frontend != nullptr && supershuckie_frontend_get_audio_enabled(this->frontend);
    this->audio_muted->setEnabled(audio_on);
    this->audio_volume_menu->setEnabled(audio_on);

    auto compression_level = this->frontend != nullptr ? supershuckie_frontend_get_replay_compression_level(this->frontend) : 9;
    bool compression_is_preset = false;
    for(auto *level : this->replay_compression_levels) {
        bool matches = level->number == compression_level;
        level->setChecked(matches);
        compression_is_preset = compression_is_preset || matches;
    }
    this->replay_compression_custom->setVisible(!compression_is_preset);
    this->replay_compression_custom->setChecked(!compression_is_preset);
    this->replay_compression_custom->setText(QString("Custom (level %1, from settings.json)").arg(compression_level));

    switch(replay_state) {
        case SuperShuckieReplayState::SuperShuckieReplayState__Recording:
            this->play_replay->setEnabled(false);
            this->resume_replay->setEnabled(false);
            this->reload_core->setEnabled(false);
            this->export_video->setEnabled(false);
            // The recording in progress must not be converted underneath the recorder.
            this->convert_replay->setEnabled(false);
            this->convert_replay_folder->setEnabled(false);
            this->current_state->setText("RECORDING");
            this->current_state->show();
            this->record_replay->setText("Stop recording replay");
            this->set_game_boy_hardware_settings_enabled(false);
            break;

        case SuperShuckieReplayState::SuperShuckieReplayState__Playback:
            this->record_replay->setEnabled(false);
            // resume_replay stays enabled here: resuming while watching continues from the
            // current playback frame (a stopped replay's resume point) into a new, separate replay.
            this->reload_core->setEnabled(false);
            // The game is the user's again while the replay is stopped; resuming playback seeks
            // back to the resume point whatever they did to it.
            this->reset_console->setEnabled(replay_stopped);
            this->export_video->setEnabled(false);
            this->current_state->setText(replay_stopped ? "PLAYBACK STOPPED" : "PLAYBACK");
            this->current_state->show();

            // play_replay stays enabled here: choosing another replay swaps it in directly.
            this->close_replay->setEnabled(true);
            this->stop_playback->setEnabled(!replay_stopped);
            this->resume_playback->setEnabled(replay_stopped);
            this->go_to_resume_point->setEnabled(replay_stopped);
            this->set_game_boy_hardware_settings_enabled(false);
            break;

        case SuperShuckieReplayState::SuperShuckieReplayState__NoReplay:
            this->current_state->hide();
            break;
    }

    this->refresh_play_together_actions();

    this->refresh_nds_date_preset_states();

    this->last_known_replay_state = replay_state;
    this->last_known_replay_stopped = replay_stopped;
}

void MainWindow::do_open_rom() {
    QFileDialog rom_opener(this);
    rom_opener.setFileMode(QFileDialog::FileMode::ExistingFile);
    rom_opener.setNameFilters(QStringList({
        "All compatible ROM files (*.gb *.gbc *.gba *.nds)",
        "GB/GBC ROM dumps (*.gb *.gbc)",
        "GBA ROM dumps (*.gba)",
        "NDS ROM files (*.nds)",
        "Any files (*)"
    }));
    rom_opener.setWindowTitle("Select a ROM to open");

    // exec() runs a nested event loop; keep the 1 ms ticker from re-entering tick() underneath it.
    this->stop_timer();
    rom_opener.exec();
    this->start_timer();

    auto files = rom_opener.selectedFiles();
    if(files.size() != 1) {
        return;
    }

    this->load_rom(std::filesystem::path(files[0].toStdU16String()));
}

void MainWindow::load_rom(const std::filesystem::path &path) {
    char error[256] = "";

    if(this->play_together != nullptr && !this->play_together->confirm_leave("Opening another ROM")) {
        return;
    }

    // path.string() converts to the narrow "native" encoding and throws for characters that
    // encoding can't represent; u8string() always succeeds and is what the Rust side expects.
    auto path_utf8 = path.u8string();
    const char *path_utf8_str = reinterpret_cast<const char *>(path_utf8.c_str());
    if(!supershuckie_frontend_load_rom(this->frontend, path_utf8_str, error, sizeof(error))) {
        this->stop_timer();
        DISPLAY_ERROR_DIALOG_P(this, "Can't load ROM", "\"%s\" failed to load:\n\n%s", path_utf8_str, error);
        this->start_timer();
    }

    this->rebuild_recent_roms_menu();
}

void MainWindow::load_rom(const char *path) {
    this->load_rom(std::filesystem::path(path));
}

void MainWindow::do_close_rom() {
    if(this->play_together != nullptr && !this->play_together->confirm_leave("Closing the ROM")) {
        return;
    }
    supershuckie_frontend_close_rom(this->frontend);
    supershuckie_frontend_set_paused(this->frontend, false);
}

void MainWindow::do_unload_rom() {
    if(this->play_together != nullptr && !this->play_together->confirm_leave("Unloading the ROM")) {
        return;
    }
    supershuckie_frontend_unload_rom(this->frontend);
    supershuckie_frontend_set_paused(this->frontend, false);
}

void MainWindow::do_screenshot() {
    if(this->frontend == nullptr) {
        return;
    }

    // Capture exactly what's on screen right now. This holds the last frame while paused and
    // during replay playback, so it works in all of those states.
    QImage image = this->render_widget->capture();
    if(image.isNull()) {
        this->show_error("Screenshot", "%s", "No frame is available to capture. Load a ROM first.");
        return;
    }

    // Screenshots live in a "screenshots" folder alongside the ROM's replays and save data.
    std::size_t len = supershuckie_frontend_get_screenshot_directory(this->frontend, nullptr, 0);
    if(len == 0) {
        this->show_error("Screenshot", "%s", "Could not determine where to save the screenshot.");
        return;
    }
    std::vector<char> dir_buf(len, '\0');
    supershuckie_frontend_get_screenshot_directory(this->frontend, dir_buf.data(), dir_buf.size());

    // Timestamped name (with milliseconds) so rapid captures never collide.
    QString filename = QString("screenshot_%1.png").arg(QDateTime::currentDateTime().toString("yyyy-MM-dd_HH-mm-ss-zzz"));
    QString path = QDir(QString::fromUtf8(dir_buf.data())).filePath(filename);

    if(image.save(path, "PNG")) {
        char title[1024];
        std::snprintf(title, sizeof(title), "Saved screenshot \"%s\"", filename.toStdString().c_str());
        this->set_title(title);
    }
    else {
        this->show_error("Screenshot", "Failed to save screenshot to:\n\n%s", path.toStdString().c_str());
    }
}

void MainWindow::do_new_game() noexcept {
    std::size_t save_file_length = 0;
    const char *current_save_file = supershuckie_frontend_get_current_save_file(this->frontend, &save_file_length);
    std::string save_file(current_save_file, save_file_length);

    auto text = AskForTextDialog::ask(this, "New game", "Enter the name of the new (empty) save file", "WARNING: If the file exists, it will be deleted immediately.", save_file.c_str());
    if(text == std::nullopt) {
        return;
    }
    supershuckie_frontend_load_or_create_save_file(this->frontend, text->c_str(), true);

    char fmt[256];
    std::snprintf(fmt, sizeof(fmt), "Created empty save file \"%s\"", text->c_str());
    this->set_title(fmt);
}

void MainWindow::do_save_game() {
    char err[256];
    if(supershuckie_frontend_save_sram(this->frontend, err, sizeof(err))) {
        this->set_title("Saved SRAM successfully!");
    }
    else {
        this->show_error("Can't save SRAM", "%s", err);
    }
}

void MainWindow::do_save_new_game() {
    auto text = AskForTextDialog::ask(this, "Save as new game", "Enter the name of the new (copied) save file", "WARNING: If the file exists, it will be overwritten on save.");
    if(text == std::nullopt) {
        return;
    }
    supershuckie_frontend_set_current_save_file(this->frontend, text->c_str());

    char fmt[256];
    std::snprintf(fmt, sizeof(fmt), "Switched to save file \"%s\"", text->c_str());
    this->set_title(fmt);
}

void MainWindow::do_reset_console() {
    supershuckie_frontend_hard_reset_console(this->frontend);
}

void MainWindow::do_toggle_pause() {
    supershuckie_frontend_set_paused(this->frontend, this->pause->isChecked());
}

void MainWindow::do_toggle_number_row_for_save_states() {
    this->use_number_keys_for_quick_slots = this->use_number_row_for_quick_slots->isChecked();
    this->set_quick_load_shortcuts();
    
    supershuckie_frontend_set_custom_setting(this->frontend, USE_NUMBER_KEYS_FOR_QUICK_SLOTS, this->use_number_keys_for_quick_slots ? "1" : "0");
}

void MainWindow::set_quick_load_shortcuts() {
    Qt::KeyboardModifiers control = static_cast<Qt::KeyboardModifiers>(this->use_number_keys_for_quick_slots ? Qt::ControlModifier : 0);

    for(std::size_t i = 0; i < MainWindow::QUICK_SAVE_STATE_COUNT; i++) {
        Qt::Key key = static_cast<Qt::Key>((this->use_number_keys_for_quick_slots ? Qt::Key_1 : Qt::Key_F1) + i);
        this->set_default_shortcut(this->quick_save_save_states[i], QKeyCombination(control | Qt::ShiftModifier, key));
        this->set_default_shortcut(this->quick_load_save_states[i], QKeyCombination(control, key));
    }

    this->apply_shortcuts();
}

static QString shortcut_function_name(const QString &text) {
    auto name = text.trimmed();
    if(name.endsWith("...")) {
        name.chop(3);
    }
    else if(name.endsWith(QChar(u'…'))) {
        name.chop(1);
    }
    return name.trimmed();
}

void MainWindow::set_up_shortcuts() {
    // Never added to a widget, so Qt doesn't fire these; the render widget matches them during playback.
    auto playback_action = [this](const char *id, const char *name, Qt::Key key) {
        auto *action = new QAction(name, this);
        action->setObjectName(id);
        action->setShortcut(QKeyCombination(key));
        return action;
    };
    this->playback_toggle_pause = playback_action("playback-pause", "Pause or resume playback", Qt::Key_Space);
    this->playback_skip_back = playback_action("playback-skip-back", "Skip back 240 frames", Qt::Key_Left);
    this->playback_skip_forward = playback_action("playback-skip-forward", "Skip forward 240 frames", Qt::Key_Right);
    this->playback_step_back = playback_action("playback-step-back", "Step back one frame (while paused)", Qt::Key_Comma);
    this->playback_step_forward = playback_action("playback-step-forward", "Step forward one frame (while paused)", Qt::Key_Period);

    for(auto *menu_action : this->menu_bar->actions()) {
        auto *menu = QMenu::menuInAction(menu_action);
        if(menu == nullptr) {
            continue;
        }
        this->collect_shortcut_bindings(menu, { shortcut_function_name(menu->title()) });

        if(menu == this->replays_menu) {
            for(auto *action : { this->playback_toggle_pause, this->playback_skip_back, this->playback_skip_forward, this->playback_step_back, this->playback_step_forward }) {
                ShortcutBinding binding;
                binding.id = action->objectName();
                binding.path = QStringList("Replay playback");
                binding.name = action->text();
                binding.action = action;
                binding.playback_control = true;
                binding.defaults = action->shortcuts();
                this->shortcut_bindings.push_back(std::move(binding));
            }
        }
    }

    QSet<QString> ids;
    for(const auto &binding : this->shortcut_bindings) {
        Q_ASSERT_X(!ids.contains(binding.id), "set_up_shortcuts", qPrintable("duplicate shortcut id " + binding.id));
        ids.insert(binding.id);
    }
}

void MainWindow::collect_shortcut_bindings(QMenu *menu, const QStringList &path) {
    for(auto *action : menu->actions()) {
        if(auto *submenu = QMenu::menuInAction(action)) {
            this->collect_shortcut_bindings(submenu, path + QStringList(shortcut_function_name(submenu->title())));
            continue;
        }
        // Saved shortcuts are keyed by objectName, so only actions given one are rebindable. That leaves
        // out separators and dynamic entries such as recent ROMs.
        if(action->objectName().isEmpty()) {
            continue;
        }

        ShortcutBinding binding;
        binding.id = action->objectName();
        binding.path = path;
        auto name = action->property(SHORTCUT_NAME_PROPERTY);
        binding.name = name.isValid() ? name.toString() : shortcut_function_name(action->text());
        binding.action = action;
        binding.defaults = action->shortcuts();
        this->shortcut_bindings.push_back(std::move(binding));
    }
}

void MainWindow::set_default_shortcut(QAction *action, const QKeySequence &shortcut) {
    for(auto &binding : this->shortcut_bindings) {
        if(binding.action == action) {
            binding.defaults = { shortcut };
            return;
        }
    }
}

void MainWindow::load_shortcuts() {
    const char *setting = supershuckie_frontend_get_custom_setting(this->frontend, SHORTCUTS);
    if(setting == nullptr) {
        return;
    }

    auto saved = QJsonDocument::fromJson(QByteArray(setting)).object();
    for(auto &binding : this->shortcut_bindings) {
        auto value = saved.value(binding.id);
        if(!value.isArray()) {
            continue;
        }

        QList<QKeySequence> shortcuts;
        for(const auto &entry : value.toArray()) {
            auto sequence = QKeySequence::fromString(entry.toString(), QKeySequence::PortableText);
            if(sequence.count() == 1 && sequence[0].key() != Qt::Key_unknown && shortcuts.size() < SHORTCUT_SLOTS) {
                shortcuts.append(sequence);
            }
        }
        binding.custom = shortcuts;
    }
}

void MainWindow::save_shortcuts() {
    // Keep saved entries this build doesn't know, such as from another version sharing the settings file.
    QJsonObject saved;
    if(const char *setting = supershuckie_frontend_get_custom_setting(this->frontend, SHORTCUTS)) {
        saved = QJsonDocument::fromJson(QByteArray(setting)).object();
    }

    for(const auto &binding : this->shortcut_bindings) {
        saved.remove(binding.id);
        if(binding.custom.has_value()) {
            QJsonArray shortcuts;
            for(const auto &sequence : *binding.custom) {
                shortcuts.append(sequence.toString(QKeySequence::PortableText));
            }
            saved.insert(binding.id, shortcuts);
        }
    }

    if(saved.isEmpty()) {
        supershuckie_frontend_set_custom_setting(this->frontend, SHORTCUTS, nullptr);
    }
    else {
        supershuckie_frontend_set_custom_setting(this->frontend, SHORTCUTS, QJsonDocument(saved).toJson(QJsonDocument::Compact).constData());
    }
}

void MainWindow::apply_shortcuts() {
    resolve_shortcuts(this->shortcut_bindings);
    for(const auto &binding : this->shortcut_bindings) {
        binding.action->setShortcuts(binding.current);
    }
}

QAction *MainWindow::playback_action_for(const QKeyEvent *event) const {
    QAction *const actions[] = { this->playback_toggle_pause, this->playback_skip_back, this->playback_skip_forward, this->playback_step_back, this->playback_step_forward };
    auto key = static_cast<Qt::Key>(event->key());
    auto modifiers = event->modifiers() & ~(Qt::KeypadModifier | Qt::GroupSwitchModifier);

    // Exact match, then without Shift: shifted symbols such as "!" are recorded without it.
    for(auto held : { modifiers, modifiers & ~Qt::ShiftModifier }) {
        QKeySequence pressed(QKeyCombination(held, key));
        for(auto *action : actions) {
            if(action->shortcuts().contains(pressed)) {
                return action;
            }
        }
    }
    return nullptr;
}

void MainWindow::do_open_shortcuts_dialog() {
    this->open_shortcuts_dialog();
}

void MainWindow::open_shortcuts_dialog(const QString &focus_id) {
    ShortcutsSettingsWindow dialog(this, this->shortcut_bindings);
    if(!focus_id.isEmpty()) {
        dialog.focus_binding(focus_id);
    }
    if(dialog.exec() != QDialog::Accepted) {
        return;
    }

    for(std::size_t i = 0; i < this->shortcut_bindings.size(); i++) {
        this->shortcut_bindings[i].custom = dialog.bindings[i].custom;
    }
    this->apply_shortcuts();
    this->save_shortcuts();
    supershuckie_frontend_write_settings(this->frontend);

    // The start screen's tooltips show the favorites' shortcuts.
    this->landing_widget->rebuild_tiles();
}

void MainWindow::closeEvent(QCloseEvent *event) {
    QWidget::closeEvent(event);

    if(this->frontend) {
        char xy[256];
        auto geometry = this->geometry();
        std::snprintf(xy, sizeof(xy), "%d|%d", geometry.x(), geometry.y());
        supershuckie_frontend_set_custom_setting(this->frontend, WINDOW_XY, xy);
        if(this->memory_tools != nullptr) {
            this->memory_tools->save_windows();
        }
        if(this->play_together != nullptr) {
            this->play_together->save_windows();
            supershuckie_frontend_play_together_leave(this->frontend);
        }
        if(this->bookmark_window != nullptr) {
            supershuckie_frontend_set_custom_setting(this->frontend, BOOKMARK_WINDOW_STATE, this->bookmark_window->save_state().toUtf8().constData());
        }
        supershuckie_frontend_watch_save(this->frontend);
        char bookmark_error[512] = {};
        if(!supershuckie_frontend_bookmark_flush(this->frontend, bookmark_error, sizeof(bookmark_error))) {
            this->show_error("Bookmarks were not saved", "%s", bookmark_error);
        }
        char stop_recording_error[1024];
        if(!supershuckie_frontend_stop_recording_replay(this->frontend, stop_recording_error, sizeof(stop_recording_error))) {
            this->stop_timer();
            DISPLAY_ERROR_DIALOG_P(this, "Failed to stop recording", "%s", stop_recording_error);
            this->start_timer();
        }
        supershuckie_frontend_write_settings(this->frontend);
        supershuckie_frontend_save_sram(this->frontend, nullptr, 0);
    }

    // if(!this->try_unload_rom()) {
        // event->ignore();
    // }
}

MainWindow::~MainWindow() {
    // Stop asking for frames before the frontend goes away.
    this->display_sync.reset();
    // The device callback reads the ring, never the frontend, but stop it before the frontend
    // goes anyway.
    this->audio.reset();
    if(this->frontend) {
        supershuckie_frontend_free(this->frontend);
        this->frontend = nullptr;
    }
}

void MainWindow::do_record_replay() {
    const char *current_replay = supershuckie_frontend_get_recording_replay_file(this->frontend);
    if(current_replay != nullptr) {
        char saved[512];
        std::snprintf(saved, sizeof(saved), "Saved replay \"%s\"", current_replay);
        char stop_recording_error[1024];
        if(!supershuckie_frontend_stop_recording_replay(this->frontend, stop_recording_error, sizeof(stop_recording_error))) {
            this->stop_timer();
            DISPLAY_ERROR_DIALOG_P(this, "Failed to stop recording", "%s", stop_recording_error);
            this->start_timer();
        }
        this->set_title(saved);
    }
    else {
        if(!this->check_freezes_before_recording()) {
            return;
        }
        char result[256];
        if(supershuckie_frontend_start_recording_replay(this->frontend, nullptr, result, sizeof(result))) {
            char fmt[512];
            std::snprintf(fmt, sizeof(fmt), "Started recording replay \"%s\"", result);
            this->set_title(fmt);
        }
        else {
            this->show_error("Failed to start recording replay", "%s", result);
        }
    }
    this->refresh_action_states();
}

std::vector<std::string> SuperShuckie64::wrap_array_std(SuperShuckieStringArrayRaw *array) {
    auto ptr = std::unique_ptr<SuperShuckieStringArrayRaw, decltype(&supershuckie_stringarray_free)>(array, &supershuckie_stringarray_free);
    std::vector<std::string> q;
    std::size_t count = supershuckie_stringarray_len(ptr.get());

    for(std::size_t i = 0; i < count; i++) {
        q.emplace_back(supershuckie_stringarray_get(ptr.get(), i));
    }

    return q;
}

void MainWindow::do_load_game() {
    // TODO: consider pre-selecting the save that we're already on?
    auto saves = wrap_array_std(supershuckie_frontend_get_all_saves_for_rom(this->frontend, nullptr));

    auto text = SelectItemDialog::ask(this, saves, "Select a save", "Select a save file to load.");
    if(text == std::nullopt) {
        return;
    }
    
    supershuckie_frontend_load_or_create_save_file(this->frontend, text->c_str(), false);

    char fmt[256];
    std::snprintf(fmt, sizeof(fmt), "Switched to save file \"%s\"", text->c_str());
    this->set_title(fmt);
}

void MainWindow::do_resume_replay() {
    char result[512];
    bool ok;

    if(!this->check_freezes_before_recording()) {
        return;
    }

    if(supershuckie_frontend_get_replay_state(this->frontend) == SuperShuckieReplayState::SuperShuckieReplayState__Playback) {
        // Watching a replay: resume from the frame currently being played back, no prompts.
        ok = supershuckie_frontend_resume_recording_from_current_replay(this->frontend, result, sizeof(result));
    }
    else {
        // Not watching a replay: pick one and resume from its end.
        auto replays = wrap_array_std(supershuckie_frontend_get_all_replays_for_rom(this->frontend, nullptr));
        auto source = SelectItemDialog::ask(this, replays, "Resume from replay", "Select a replay to continue recording from its end.");
        if(source == std::nullopt) {
            return;
        }
        ok = supershuckie_frontend_resume_recording_from_replay(this->frontend, source->c_str(), 0, true, nullptr, result, sizeof(result));
    }

    if(ok) {
        char fmt[600];
        std::snprintf(fmt, sizeof(fmt), "Resumed recording into replay \"%s\"", result);
        this->set_title(fmt);
    }
    else {
        this->show_error("Failed to resume recording replay", "%s", result);
    }

    this->refresh_action_states();
}

void MainWindow::do_close_replay() {
    if(supershuckie_frontend_get_replay_state(this->frontend) != SuperShuckieReplayState::SuperShuckieReplayState__Playback) {
        return;
    }
    supershuckie_frontend_close_replay(this->frontend);
    this->set_title("Closed replay");
    this->refresh_action_states();
}

void MainWindow::do_stop_playback() {
    if(this->frontend == nullptr || supershuckie_frontend_is_replay_playback_stopped(this->frontend)) {
        return;
    }
    supershuckie_frontend_stop_replay_playback(this->frontend);
    this->refresh_action_states();
    this->playback_bar->tick();
}

void MainWindow::do_resume_playback() {
    if(this->frontend == nullptr || !supershuckie_frontend_is_replay_playback_stopped(this->frontend)) {
        return;
    }
    char err[512];
    if(!supershuckie_frontend_resume_replay_playback(this->frontend, err, sizeof(err))) {
        this->show_error("Could not resume the replay", "%s", err);
    }
    this->refresh_action_states();
    this->playback_bar->tick();
}

void MainWindow::do_go_to_resume_point() {
    if(this->frontend == nullptr || !supershuckie_frontend_is_replay_playback_stopped(this->frontend)) {
        return;
    }
    supershuckie_frontend_go_to_replay_resume_point(this->frontend);
}

void MainWindow::do_play_replay() {
    auto replays = wrap_array_std(supershuckie_frontend_get_all_replays_for_rom(this->frontend, nullptr));
    auto text = SelectItemDialog::ask(this, replays, "Select a replay", "Select a replay file to play.");
    if(text == std::nullopt) {
        return;
    }

    char err[512];
    char fmt[512];

    // A failed load can still have detached a replay that was playing, so the menu is refreshed
    // on every path from here on.
    if(!supershuckie_frontend_load_replay(this->frontend, text->c_str(), false, err, sizeof(err))) {
        std::snprintf(fmt, sizeof(fmt), "%s", err);
        this->show_error("Replay file issues detected", "%s", fmt);

        if(!supershuckie_frontend_load_replay(this->frontend, text->c_str(), true, err, sizeof(err))) {
            this->refresh_action_states();
            return;
        }
    }

    if(!supershuckie_frontend_get_replay_playback_time(this->frontend, nullptr, nullptr)) {
        this->refresh_action_states();
        return;
    }

    std::snprintf(fmt, sizeof(fmt), "Opened replay file \"%s\"", text->c_str());
    this->set_title(fmt);

    this->refresh_action_states();
}

void MainWindow::do_export_video() {
    auto replays = wrap_array_std(supershuckie_frontend_get_all_replays_for_rom(this->frontend, nullptr));
    if(replays.empty()) {
        this->show_error("Export video", "%s", "No replays found for this ROM.");
        return;
    }

    VideoExportDialog dlg(this);
    if(dlg.exec() != QDialog::Accepted) {
        return;
    }

    std::string replay = dlg.replay_name();
    std::string out_path = dlg.output_path();
    std::string custom = dlg.custom_args();
    bool use_range = dlg.use_range();
    std::uint32_t start = dlg.start_frame();
    std::uint32_t end = dlg.end_frame();
    std::uint32_t preset = dlg.preset();
    std::uint32_t scale = dlg.scale();
    std::uint32_t layout = dlg.layout();

    char err[1024];
    bool ok = supershuckie_frontend_export_replay_video(this->frontend,
        replay.c_str(), out_path.c_str(), use_range, start, end, preset,
        preset == 2 ? custom.c_str() : nullptr, scale, layout, err, sizeof(err));
    if(!ok) {
        this->show_error("Export failed to start", "%s", err);
        return;
    }

    // Stop the main ticker so it doesn't poll the export while we drive the progress dialog.
    this->stop_timer();

    QProgressDialog progress("Exporting…", "Cancel", 0, 100, this);
    progress.setWindowModality(Qt::WindowModal);
    progress.setMinimumDuration(0);
    progress.setValue(0);

    bool success = false;
    while(true) {
        std::uint64_t done = 0, total = 0;
        supershuckie_frontend_export_poll(this->frontend, &done, &total);

        char poll_err[1024];
        std::uint32_t fin = supershuckie_frontend_export_poll_finished(this->frontend, poll_err, sizeof(poll_err));
        if(fin == 1) {
            success = true;
            break;
        }
        if(fin == 2) {
            this->show_error("Export failed", "%s", poll_err);
            break;
        }

        if(total > 0) {
            progress.setMaximum(static_cast<int>(total));
            progress.setValue(static_cast<int>(done));
        }

        if(progress.wasCanceled()) {
            supershuckie_frontend_export_cancel(this->frontend);
        }

        QCoreApplication::processEvents(QEventLoop::AllEvents, 16);
    }

    progress.close();

    this->start_timer();

    if(success) {
        this->set_title("Exported video");
    }

    this->refresh_action_states();
}

static QString default_replay_dialog_dir(MainWindow *window, SuperShuckieFrontendRaw *frontend, const QString &app_dir) {
    char dir[4096];
    if(supershuckie_frontend_get_replays_dir_for_current_rom(frontend, dir, sizeof(dir)) && QDir(QString::fromUtf8(dir)).exists()) {
        return QString::fromUtf8(dir);
    }
    (void)window;
    return app_dir;
}

void MainWindow::do_convert_replay() {
    QString start = default_replay_dialog_dir(this, this->frontend, this->app_dir);
    QString path = QFileDialog::getOpenFileName(this, "Convert replay to current format", start, "Replays (*.replay)");
    if(path.isEmpty()) {
        return;
    }
    this->convert_replays_at(path);
}

void MainWindow::do_convert_replay_folder() {
    QString start = default_replay_dialog_dir(this, this->frontend, this->app_dir);
    QString path = QFileDialog::getExistingDirectory(this, "Convert every replay in a folder to current format", start);
    if(path.isEmpty()) {
        return;
    }
    this->convert_replays_at(path);
}

void MainWindow::convert_replays_at(const QString &path) {
    std::string path_utf8 = path.toStdString();

    char description[2048];
    if(!supershuckie_frontend_plan_replay_conversion(this->frontend, path_utf8.c_str(), description, sizeof(description))) {
        QMessageBox info(this);
        info.setWindowTitle("Convert replays");
        info.setIcon(QMessageBox::Icon::Information);
        info.setText(QString::fromUtf8(description));
        info.exec();
        return;
    }

    // Confirm, and ask whether to keep the originals.
    QMessageBox confirm(this);
    confirm.setWindowTitle("Convert replays");
    confirm.setIcon(QMessageBox::Icon::Question);
    confirm.setText(QString::fromUtf8(description));
    confirm.setInformativeText(
        "Replays are re-encoded with your current replay settings and verified; an original is only replaced "
        "after its conversion has been checked packet by packet. Older versions of Super Shuckie cannot open "
        "converted replays.\n\nKeep the originals as .replay.bak files?"
    );
    QPushButton *keep = confirm.addButton("Keep originals", QMessageBox::YesRole);
    QPushButton *replace = confirm.addButton("Replace originals", QMessageBox::NoRole);
    QPushButton *cancel = confirm.addButton(QMessageBox::Cancel);
    confirm.setDefaultButton(keep);
    confirm.exec();
    if(confirm.clickedButton() == cancel || confirm.clickedButton() == nullptr) {
        return;
    }
    bool keep_backups = confirm.clickedButton() != replace;

    char err[1024];
    if(!supershuckie_frontend_start_replay_conversion(this->frontend, keep_backups, err, sizeof(err))) {
        this->show_error("Convert replays", "%s", err);
        return;
    }

    // Stop the main ticker while the modal progress dialog drives the event loop.
    this->stop_timer();

    QProgressDialog progress("Converting…", "Cancel", 0, 100, this);
    progress.setWindowTitle("Convert replays");
    progress.setWindowModality(Qt::WindowModal);
    progress.setMinimumDuration(0);
    progress.setMinimumWidth(480);
    progress.setValue(0);

    char summary[8192];
    summary[0] = 0;
    while(true) {
        std::uint32_t fin = supershuckie_frontend_replay_conversion_poll_finished(this->frontend, summary, sizeof(summary));
        if(fin == 1) {
            break;
        }

        std::uint32_t file_index = 0, file_count = 0, phase = 0;
        std::uint64_t done = 0, total = 0;
        char name[512];
        name[0] = 0;
        if(supershuckie_frontend_replay_conversion_poll(this->frontend, &file_index, &file_count, &phase, &done, &total, name, sizeof(name))) {
            std::uint64_t percent = total > 0 ? done * 100 / total : 0;
            progress.setLabelText(QString("%1 %2 (%3 of %4)")
                .arg(phase == 1 ? "Verifying" : "Converting")
                .arg(QString::fromUtf8(name))
                .arg(file_index + 1)
                .arg(file_count));
            // Overall progress: each replay is 200 units (100 converting + 100 verifying).
            progress.setMaximum(static_cast<int>(file_count * 200));
            progress.setValue(static_cast<int>(file_index * 200 + phase * 100 + percent));
        }

        if(progress.wasCanceled()) {
            supershuckie_frontend_replay_conversion_cancel(this->frontend);
            progress.setLabelText("Cancelling after the current replay…");
        }

        QCoreApplication::processEvents(QEventLoop::AllEvents, 16);
        QThread::msleep(30);
    }

    progress.close();
    this->start_timer();

    QMessageBox result(this);
    result.setWindowTitle("Convert replays");
    result.setIcon(std::strstr(summary, "FAILED") != nullptr ? QMessageBox::Icon::Warning : QMessageBox::Icon::Information);
    result.setText(QString::fromUtf8(summary));
    result.exec();

    this->refresh_action_states();
}

void MainWindow::on_refresh_screens(void *user_data, std::size_t screen_count, const uint32_t *const *pixels) {
    auto *self = reinterpret_cast<MainWindow *>(user_data);
    
    self->frames_in_last_second += 1;
    self->render_widget->refresh_screen(screen_count, pixels);
}

void MainWindow::on_change_video_mode(void *user_data, std::size_t screen_count, const SuperShuckieScreenData *screen_data, std::uint8_t video_scale) {
    auto *self = reinterpret_cast<MainWindow *>(user_data);
    
    self->render_widget->set_dimensions(screen_count, screen_data, video_scale);
    self->frames_in_last_second = 0;
    self->current_fps = 0.0;
    self->second_start = clock::now();
    if(self->is_game_running()) {
        self->set_title("Loaded ROM successfully!");
    }
    else {
        self->set_title();
    }

    for(auto &scale : self->change_video_scale) {
        scale->setChecked(scale->number == video_scale);
    }

    self->update_landing_visibility();
    self->refresh_action_states();
}

void MainWindow::update_landing_visibility() {
    bool running = this->is_game_running();
    bool landing_shown = this->landing_widget->isVisibleTo(this->landing_widget->parentWidget());

    if(running) {
        if(landing_shown) {
            this->landing_widget->hide();
            this->render_widget->show();
            // The landing screen had the mouse and keyboard; give the keyboard back to the game.
            this->render_widget->setFocus(Qt::OtherFocusReason);
        }
    }
    else {
        // Take over the exact footprint of the (blank) game view so the fixed-size window doesn't
        // jump when switching between the two.
        this->landing_widget->setFixedSize(this->render_widget->size());
        this->render_widget->hide();
        this->landing_widget->show();
    }
}

bool MainWindow::is_game_running() {
    return this->frontend != nullptr && supershuckie_frontend_is_game_running(this->frontend);
}

void MainWindow::do_open_game_speed_dialog() noexcept {
    GameSpeedDialog *dialog = new GameSpeedDialog(this);

    dialog->exec();

    delete dialog;
}

void MainWindow::do_undo_load_save_state() {
    if(supershuckie_frontend_undo_load_save_state(this->frontend)) {
        this->set_title("Undo load save state successful");
    }
    else {
        this->set_title("No more states in the stack!");
    }
}

void MainWindow::do_redo_load_save_state() {
    if(supershuckie_frontend_redo_load_save_state(this->frontend)) {
        this->set_title("Redo load save state successful");
    }
    else {
        this->set_title("No more states in the stack!");
    }
}

void MainWindow::do_toggle_status_bar() {
    bool displayed = this->show_status_bar->isChecked();
    supershuckie_frontend_set_custom_setting(this->frontend, DISPLAY_STATUS_BAR, displayed ? "1" : "0");
    this->status_bar->setVisible(displayed);
    this->refresh_title();
}

void MainWindow::do_toggle_sync_display() {
    bool on = this->sync_display_to_refresh->isChecked();
    supershuckie_frontend_set_custom_setting(this->frontend, SYNC_DISPLAY_TO_REFRESH, on ? "1" : "0");
    this->apply_display_sync(on);
}

void MainWindow::apply_display_sync(bool on) {
    supershuckie_frontend_set_present_on_demand(this->frontend, on);
    this->render_widget->set_manual_present(on);
    this->late_presents = 0;
    this->worst_present_us = 0;
    if(on) {
        if(!this->display_sync) {
            this->display_sync = std::make_unique<DisplaySyncThread>();
            connect(this->display_sync.get(), &DisplaySyncThread::vblank, this, &MainWindow::present_frame, Qt::QueuedConnection);
            this->display_sync->start(QThread::HighestPriority);
        }
    }
    else {
        this->display_sync.reset();
    }
}

void MainWindow::present_frame() {
    if(!this->display_sync || this->frontend == nullptr) {
        return;
    }

    auto refreshes = this->display_sync->acknowledge();
    auto frames_before = this->frames_in_last_second;
    supershuckie_frontend_present_latest_frame(this->frontend, refreshes);
    if(this->frames_in_last_second == frames_before) {
        return;
    }

    // Paint now rather than from a queued paint event: the frame has to reach the compositor
    // before the next refresh, and the event queue is shared with the 1 ms ticker.
    this->render_widget->present_now();

    // A frame that took most of a refresh to get from the vertical blank to the compositor has
    // probably missed it, and shows up a refresh late.
    auto took = this->display_sync->microseconds_since_vblank();
    if(took > this->worst_present_us) {
        this->worst_present_us = took;
    }
    if(took * 4 > this->display_sync->refresh_period_microseconds() * 3) {
        this->late_presents++;
    }
}

void MainWindow::do_toggle_pokeabyte() {
    char err[256];

    bool enabled = this->enable_pokeabyte_integration->isChecked();
    if(!supershuckie_frontend_set_pokeabyte_enabled(this->frontend, enabled, err, sizeof(err))) {
        this->show_error("Failed to enable Poke-A-Byte integration", "An error occurred when enabling Poke-A-Byte integration:\n\n%s", err);
        this->enable_pokeabyte_integration->setChecked(false);
    }
}

void MainWindow::do_set_pokeabyte_port() {
    auto current = supershuckie_frontend_get_pokeabyte_port(this->frontend);
    auto text = AskForTextDialog::ask(this, "Poke-A-Byte port", "Enter the UDP port this game is served to Poke-A-Byte on", "Poke-A-Byte connects to 55356 unless told otherwise. In Play Together, friends' games are served on the ports above this one.", QString::number(current));
    if(!text.has_value()) {
        return;
    }

    bool ok = false;
    int port = QString::fromStdString(*text).trimmed().toInt(&ok);
    if(!ok || port < 1 || port > 65535) {
        this->show_error("Poke-A-Byte port", "%s is not a port number (1-65535).", text->c_str());
        return;
    }

    char err[256];
    if(!supershuckie_frontend_set_pokeabyte_port(this->frontend, static_cast<uint16_t>(port), err, sizeof(err))) {
        this->show_error("Failed to change the Poke-A-Byte port", "An error occurred when moving the Poke-A-Byte integration to port %d:\n\n%s", port, err);
    }
    this->pokeabyte_port->setText(QString("Port (%1)…").arg(supershuckie_frontend_get_pokeabyte_port(this->frontend)));
}

void MainWindow::do_toggle_pokeabyte_serve_friends() {
    supershuckie_frontend_set_pokeabyte_serve_friends(this->frontend, this->pokeabyte_serve_friends->isChecked());
}

void MainWindow::do_toggle_stop_replay_on_input() {
    supershuckie_frontend_set_auto_stop_playback_on_input_setting(this->frontend, this->auto_stop_replay_on_input->isChecked());
}

void MainWindow::show_error(const char *title, const char *fmt, ...) {
    char message[1024];
    va_list args;
    va_start(args, fmt);
    std::vsnprintf(message, sizeof(message), fmt, args);
    va_end(args);

    this->stop_timer();
    DISPLAY_ERROR_DIALOG_P(this, title, "%s", message);
    this->start_timer();
}

void MainWindow::start_timer() {
    this->timer_stack--;
    if(this->timer_stack == 0) {
        this->ticker.start();
    }
    if(this->timer_stack < 0) {
        // Deliberately not routed through show_error(): we're already inside start_timer(), and
        // show_error() calls stop_timer()/start_timer() around the dialog, which would re-enter
        // this function while the stack counter is already unbalanced. Parent it and skip the
        // timer guard instead.
        DISPLAY_ERROR_DIALOG_P(this, "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA", "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA");
        this->timer_stack = 0;
    }
}

void MainWindow::stop_timer() {
    this->timer_stack++;
    this->ticker.stop();
}

void MainWindow::do_open_controls_settings_dialog() noexcept {
    ControlsSettingsWindow::SettingsMap settings_structs;
    const char *name = nullptr;
    for(std::uint8_t i = 0; i < 255 && (name = supershuckie_frontend_get_emulator_type_name(i)) != nullptr; i++) {
        if(supershuckie_frontend_emulator_type_uses_shared_config(i)) {
            continue;
        }

        auto *settings_struct = supershuckie_frontend_get_control_settings(this->frontend, i);
        std::unique_ptr<SuperShuckieControlSettingsRaw, decltype(&supershuckie_control_settings_free)> settings_struct_managed(settings_struct, supershuckie_control_settings_free);
        auto pair = std::pair(name, std::move(settings_struct_managed));
        settings_structs.emplace(i, std::move(pair));
    }

    auto *settings = new ControlsSettingsWindow(this, std::move(settings_structs));

    if(settings->exec() == QDialog::Accepted) {
        for(auto &entry : settings->settings) {
            supershuckie_frontend_set_control_settings(this->frontend, entry.second.second.get(), entry.first);
        }
    }

    delete settings;
}

void MainWindow::do_toggle_auto_unpause_on_input() {
    supershuckie_frontend_set_auto_unpause_on_input_setting(this->frontend, this->auto_unpause_on_input->isChecked());
}

void MainWindow::do_toggle_auto_pause_on_record() {
    supershuckie_frontend_set_auto_pause_on_record_setting(this->frontend, this->auto_pause_on_record->isChecked());
}

void MainWindow::do_open_user_dir() {
    std::size_t buffer_len = supershuckie_frontend_get_current_data_directory(this->frontend, nullptr, 0);
    std::vector<char> buffer = std::vector(buffer_len, '\x00');
    supershuckie_frontend_get_current_data_directory(this->frontend, buffer.data(), buffer.size());
    QDesktopServices::openUrl(QUrl::fromLocalFile(buffer.data()));
}

void MainWindow::do_change_playback_time(int frames) {
    supershuckie_frontend_set_playback_frame(this->frontend, static_cast<std::uint32_t>(frames));
}

void MainWindow::do_toggle_replay_keyboard_controls() {
    supershuckie_frontend_set_custom_setting(this->frontend, KEYBOARD_REPLAY_CONTROLS_DISABLED, !this->keyboard_replay_controls->isChecked() ? "1" : "0");
}

void MainWindow::do_toggle_sgb() {
    supershuckie_frontend_set_sgb_enabled(this->frontend, this->sgb_enabled->isChecked());
}

void MainWindow::do_toggle_gb_custom_colors() {
    SuperShuckieGBCustomColors colors = {};
    supershuckie_frontend_get_gb_custom_colors(this->frontend, &colors);
    colors.enabled = this->gb_custom_colors->isChecked();
    supershuckie_frontend_set_gb_custom_colors(this->frontend, &colors);
}

void MainWindow::do_open_gb_palette_dialog() {
    auto *dialog = new GBPaletteDialog(this);
    dialog->exec();
    delete dialog;

    SuperShuckieGBCustomColors colors = {};
    supershuckie_frontend_get_gb_custom_colors(this->frontend, &colors);
    this->gb_custom_colors->setChecked(colors.enabled);
}

// The Game Boy model settings would change the game under a recording, a replay or a Play
// Together session; the custom colors only change how it is drawn and stay available.
void MainWindow::set_game_boy_hardware_settings_enabled(bool enabled) {
    this->gbc_mode_items->menuAction()->setEnabled(enabled);
    this->sgb_enabled->setEnabled(enabled);
}

void MainWindow::set_gbc_mode(std::uint8_t mode) {
    supershuckie_frontend_set_gbc_mode(this->frontend, mode);
    this->refresh_action_states();
}

void MainWindow::set_replay_compression_level(std::uint8_t level) {
    supershuckie_frontend_set_replay_compression_level(this->frontend, level);
    this->refresh_action_states();
}

void MainWindow::apply_audio_gain() {
    bool muted = supershuckie_frontend_get_audio_muted(this->frontend);
    auto volume = supershuckie_frontend_get_audio_volume(this->frontend);
    this->audio->set_gain(muted ? 0.0f : static_cast<float>(volume) / 100.0f);
}

void MainWindow::do_toggle_audio_enabled() {
    bool enable = this->audio_enabled->isChecked();
    if(enable && !this->audio->open()) {
        this->audio_enabled->setChecked(false);
        this->show_error("Failed to open the audio device", "%s", this->audio->last_error().c_str());
        this->refresh_action_states();
        return;
    }
    supershuckie_frontend_set_audio_enabled(this->frontend, enable);
    if(!enable) {
        this->audio->close();
    }
    this->refresh_action_states();
}

void MainWindow::do_toggle_audio_muted() {
    supershuckie_frontend_set_audio_muted(this->frontend, this->audio_muted->isChecked());
    this->apply_audio_gain();
}

void MainWindow::do_toggle_audio_mute_when_sped_up() {
    supershuckie_frontend_set_audio_mute_when_sped_up(this->frontend, this->audio_mute_when_sped_up->isChecked());
    // Silence right away if the game is sped up at this moment, rather than after the queue plays out.
    if(this->audio_mute_when_sped_up->isChecked()) {
        this->audio->clear();
    }
}

void MainWindow::set_audio_volume(std::uint8_t percent) {
    supershuckie_frontend_set_audio_volume(this->frontend, percent);
    this->apply_audio_gain();
    this->refresh_action_states();
}

void MainWindow::set_audio_buffer(std::uint8_t preset) {
    if(preset < MainWindow::AUDIO_BUFFER_PRESETS) {
        supershuckie_frontend_set_audio_latency_ms(this->frontend, MainWindow::audio_buffer_ms[preset]);
    }
    this->refresh_action_states();
}

void MainWindow::do_open_nds_date_dialog() noexcept {
    auto *dialog = new NDSDateDialog(this);
    dialog->exec();
    delete dialog;
    this->rebuild_nds_date_menu();
}

void MainWindow::rebuild_nds_date_menu() {
    // deleteLater: this runs from inside the triggered() of the preset that was picked.
    for(auto *action : this->nds_date_preset_extras) {
        this->nds_date_menu->removeAction(action);
        action->deleteLater();
    }
    this->nds_date_preset_extras.clear();

    SuperShuckieNintendoDSDate current = {};
    supershuckie_frontend_get_nds_date(this->frontend, &current);

    auto presets = NDSDateDialog::load_presets(this->frontend);
    for(std::size_t i = 0; i < presets.size() || i < MainWindow::NDS_DATE_PRESET_SLOTS; i++) {
        QAction *action;
        if(i < MainWindow::NDS_DATE_PRESET_SLOTS) {
            action = this->nds_date_preset_slots[i];
            action->setVisible(i < presets.size());
            if(i >= presets.size()) {
                continue;
            }
        }
        else {
            action = new QAction(this->nds_date_menu);
            action->setCheckable(true);
            connect(action, &QAction::triggered, this, [this, i]() { this->apply_nds_date_preset(i); });
            this->nds_date_menu->insertAction(this->nds_date_no_presets, action);
            this->nds_date_preset_extras.push_back(action);
        }

        action->setText(QString("%1 — %2").arg(QString(presets[i].name).replace("&", "&&"), NDSDateDialog::describe_date(presets[i].date)));
        action->setChecked(NDSDateDialog::same_date(presets[i].date, current));
    }
    this->nds_date_no_presets->setVisible(presets.empty());

    this->refresh_nds_date_preset_states();
}

void MainWindow::refresh_nds_date_preset_states() {
    // A preset reloads the core, so it is available exactly when Reload core is. Checking for a DS
    // game matters for the slots' shortcuts, which a hidden submenu doesn't turn off.
    bool enabled = this->reload_core->isEnabled() && this->is_nds_game_running();
    for(auto *action : this->nds_date_preset_slots) {
        action->setEnabled(enabled);
    }
    for(auto *action : this->nds_date_preset_extras) {
        action->setEnabled(enabled);
    }
}

bool MainWindow::is_nds_game_running() {
    return this->is_game_running() && supershuckie_frontend_get_emulator_type(this->frontend) == SuperShuckieEmulatorType::SuperShuckieEmulatorType__NintendoDS;
}

void MainWindow::apply_nds_date_preset(std::size_t index) {
    if(!this->reload_core->isEnabled() || !this->is_nds_game_running()) {
        return;
    }

    SuperShuckieNintendoDSDate date = {};
    const char *name = supershuckie_frontend_get_nds_date_preset(this->frontend, index, &date);
    if(name == nullptr) {
        return;
    }

    auto message = QString("Reloaded core with date preset %1 (%2)").arg(QString::fromUtf8(name), NDSDateDialog::describe_date(date));
    supershuckie_frontend_set_nds_date(this->frontend, &date);
    supershuckie_frontend_reload_core(this->frontend);
    this->set_title(message.toUtf8().constData());
    this->rebuild_nds_date_menu();
}

void MainWindow::do_toggle_horizontal_nds() {
    supershuckie_frontend_set_custom_setting(this->frontend, HORIZONTAL_NDS, this->horizontal_nds->isChecked() ? "1" : "0");

    // FIXME: this is a hack
    this->set_video_scale(this->render_widget->current_scale+1);
    this->set_video_scale(this->render_widget->current_scale-1);
}

void MainWindow::do_toggle_swap_nds_screens() {
    // The setter re-emits the video mode, which drives render_widget to re-lay out the screens.
    supershuckie_frontend_set_swap_nds_screens(this->frontend, this->swap_nds_screens->isChecked());
}

void MainWindow::do_toggle_nds_jit() {
    supershuckie_frontend_set_nds_jit(this->frontend, this->nds_jit->isChecked());
}

void MainWindow::rebuild_recent_roms_menu() noexcept {
    this->recent_roms_menu->clear();

    auto menu = wrap_array_std(supershuckie_frontend_get_recent_roms(this->frontend));
    for(auto &item : menu) {
        this->recent_roms_menu->addAction(new StringAction(this, item.c_str(), item.c_str(), &MainWindow::load_rom));
    }

    if(!menu.empty()) {
        this->recent_roms_menu->addSeparator();
    }

    auto *clear = this->recent_roms_menu->addAction("Clear list");
    connect(clear, SIGNAL(triggered()), this, SLOT(do_clear_recent_roms()));
    clear->setEnabled(!menu.empty());
}

void MainWindow::rebuild_favorite_roms_menu() {
    // Bindings first: they point at the actions about to go.
    std::erase_if(this->shortcut_bindings, [](const ShortcutBinding &binding) {
        return binding.id.startsWith(FAVORITE_ROM_SHORTCUT_PREFIX);
    });
    // deleteLater: this can run from inside a favorite's own triggered(), e.g. one that failed to load.
    for(auto *action : this->favorite_rom_actions) {
        this->favorite_roms_menu->removeAction(action);
        action->deleteLater();
    }
    this->favorite_rom_actions.clear();

    QStringList path = { shortcut_function_name(this->file_menu->title()), shortcut_function_name(this->favorite_roms_menu->title()) };
    for(const auto &favorite : this->landing_widget->favorites) {
        auto *action = new QAction(QString(favorite.name).replace("&", "&&"), this->favorite_roms_menu);
        // Keyed by path so a shortcut survives renaming and reordering.
        action->setObjectName(FAVORITE_ROM_SHORTCUT_PREFIX + favorite.path);
        connect(action, &QAction::triggered, this, [this, rom = favorite.path, name = favorite.name]() {
            this->open_favorite_rom(rom, name);
        });
        this->favorite_roms_menu->insertAction(this->favorite_roms_none, action);
        this->favorite_rom_actions.push_back(action);

        ShortcutBinding binding;
        binding.id = action->objectName();
        binding.path = path;
        binding.name = favorite.name;
        binding.action = action;
        this->shortcut_bindings.push_back(std::move(binding));
    }
    this->favorite_roms_none->setVisible(this->favorite_rom_actions.empty());

    this->load_shortcuts();
    this->apply_shortcuts();
}

void MainWindow::open_favorite_rom(const QString &path, const QString &name) {
    // Loading the game that's already running would restart it, which a stray shortcut shouldn't do.
    if(this->is_game_running()) {
        auto recent = wrap_array_std(supershuckie_frontend_get_recent_roms(this->frontend));
        if(!recent.empty() && QFileInfo(QString::fromStdString(recent.front())).absoluteFilePath() == QFileInfo(path).absoluteFilePath()) {
            this->set_title(QString("Already playing %1").arg(name).toUtf8().constData());
            return;
        }
    }
    this->load_rom(std::filesystem::path(path.toStdU16String()));
}

QList<QKeySequence> MainWindow::favorite_rom_shortcuts(const QString &path) const {
    for(const auto &binding : this->shortcut_bindings) {
        if(binding.id == FAVORITE_ROM_SHORTCUT_PREFIX + path) {
            return binding.current;
        }
    }
    return {};
}

void MainWindow::edit_favorite_rom_shortcut(const QString &path) {
    this->open_shortcuts_dialog(FAVORITE_ROM_SHORTCUT_PREFIX + path);
}

void MainWindow::clear_favorite_rom_shortcut(const QString &path) {
    for(auto &binding : this->shortcut_bindings) {
        if(binding.id == FAVORITE_ROM_SHORTCUT_PREFIX + path && binding.custom.has_value()) {
            binding.custom.reset();
            this->apply_shortcuts();
            this->save_shortcuts();
            supershuckie_frontend_write_settings(this->frontend);
            return;
        }
    }
}

void MainWindow::do_clear_recent_roms() {
    supershuckie_frontend_clear_recent_roms(this->frontend);
    this->rebuild_recent_roms_menu();
}

void MainWindow::do_reload_core() {
    supershuckie_frontend_reload_core(this->frontend);
}

void MainWindow::do_toggle_external_commands() {
    char buf[256];
    if(!supershuckie_frontend_set_external_commands_enabled(this->frontend, this->enable_external_commands->isChecked(), buf, sizeof(buf))) {
        this->show_error("Failed to start remote commands", "%s", buf);
    }
}

void MainWindow::do_toggle_ignore_speed_changes_in_replay() {
    supershuckie_frontend_set_ignore_speed_changes_in_replay(this->frontend, this->ignore_speed_changes_in_replay->isChecked());
}

void MainWindow::do_toggle_auto_resync_keyframes_in_replay() {
    supershuckie_frontend_set_auto_resync_keyframes_in_replay(this->frontend, this->auto_resync_keyframes_in_replay->isChecked());
}

void MainWindow::do_continue_last_replay() {
    char buf[512];
    if(!supershuckie_frontend_continue_last_replay(this->frontend, buf, sizeof(buf))) {
        this->show_error("Failed to continue replay", "%s", buf);
    }
}

void MainWindow::do_toggle_disable_save_states_when_recording() {
    supershuckie_frontend_set_disable_save_states_when_recording(this->frontend, this->disable_save_states_when_recording->isChecked());
    this->refresh_action_states();
}

void MainWindow::do_toggle_disable_speed_changes_when_recording() {
    supershuckie_frontend_set_disable_speed_changes_when_recording(this->frontend, this->disable_speed_changes_when_recording->isChecked());
}

void MainWindow::set_up_play_together_menu() {
    this->play_together_menu = this->menu_bar->addMenu("Play Together");

    this->pt_open = this->play_together_menu->addAction("Host or join a session…");
    this->pt_open->setObjectName("play-together-session");
    this->pt_open->setShortcut(QKeyCombination(Qt::ControlModifier | Qt::ShiftModifier, Qt::Key_H));
    connect(this->pt_open, SIGNAL(triggered()), this, SLOT(do_play_together_open()));

    this->pt_leave = this->play_together_menu->addAction("Leave session");
    this->pt_leave->setObjectName("play-together-leave");
    connect(this->pt_leave, SIGNAL(triggered()), this, SLOT(do_play_together_leave()));

    this->play_together_menu->addSeparator();

    this->pt_reset_all = this->play_together_menu->addAction("Reset everyone (race start)");
    this->pt_reset_all->setObjectName("play-together-reset-all");
    this->pt_reset_all->setToolTip("Every player's console resets after a 3 second countdown (host only)");
    connect(this->pt_reset_all, SIGNAL(triggered()), this, SLOT(do_play_together_reset_all()));

    this->pt_sync_pause = this->play_together_menu->addAction("Sync pause");
    this->pt_sync_pause->setObjectName("play-together-sync-pause");
    this->pt_sync_pause->setCheckable(true);
    this->pt_sync_pause->setToolTip("When anyone pauses, everyone's game pauses; the host's setting applies to the whole session");
    connect(this->pt_sync_pause, SIGNAL(triggered()), this, SLOT(do_toggle_play_together_sync_pause()));

    this->pt_start_state = this->play_together_menu->addAction("Start from host's save state");
    this->pt_start_state->setObjectName("play-together-start-state");
    this->pt_start_state->setCheckable(true);
    this->pt_start_state->setToolTip("Pause and send your current save state to everyone, so every game starts from it (host only; everyone needs your ROM). \"Reset everyone\" then restarts from it.");
    connect(this->pt_start_state, SIGNAL(triggered()), this, SLOT(do_toggle_play_together_start_state()));

    this->play_together_menu->addSeparator();

    this->pt_show_windows = this->play_together_menu->addAction("Show friends' windows");
    this->pt_show_windows->setObjectName("play-together-show-windows");
    connect(this->pt_show_windows, SIGNAL(triggered()), this, SLOT(do_play_together_show_windows()));

    auto *scale_menu = this->play_together_menu->addMenu("Friends' view scale");
    for(std::size_t i = 0; i < MainWindow::PEER_SCALE_COUNT; i++) {
        char text[16];
        std::snprintf(text, sizeof(text), "%zux", i + 1);
        auto *action = new NumberedAction(this, text, static_cast<std::uint8_t>(i + 1), &MainWindow::set_peer_video_scale);
        action->setObjectName(QString("play-together-scale-%1").arg(i + 1));
        action->setCheckable(true);
        scale_menu->addAction(action);
        this->pt_scale[i] = action;
    }

    this->play_together_menu->addSeparator();

    this->pt_unlink = this->play_together_menu->addAction("Unplug link cable");
    this->pt_unlink->setObjectName("play-together-unlink");
    this->pt_unlink->setToolTip("Pull the link cable out of the friend's game it is plugged into (plug one in from a friend's window)");
    connect(this->pt_unlink, SIGNAL(triggered()), this, SLOT(do_play_together_unlink()));

    auto *delay_menu = this->play_together_menu->addMenu("Link cable input delay");
    delay_menu->setToolTip("How many frames ahead inputs are sent while linked: automatic from the ping, or at least this many (the larger of the two players' settings wins)");
    for(std::size_t i = 0; i < MainWindow::LINK_DELAY_COUNT; i++) {
        char text[32];
        if(i == 0) {
            std::snprintf(text, sizeof(text), "Auto (from ping)");
        }
        else {
            std::snprintf(text, sizeof(text), "%zu frame%s", i, i == 1 ? "" : "s");
        }
        auto *action = new NumberedAction(this, text, static_cast<std::uint8_t>(i), &MainWindow::set_link_input_delay);
        action->setObjectName(QString("play-together-link-delay-%1").arg(i));
        action->setCheckable(true);
        delay_menu->addAction(action);
        this->pt_link_delay[i] = action;
    }

    this->play_together_menu->addSeparator();

    this->pt_save_replays = this->play_together_menu->addAction("Save friends' games as replays");
    this->pt_save_replays->setObjectName("play-together-save-replays");
    this->pt_save_replays->setCheckable(true);
    this->pt_save_replays->setToolTip("Write every friend's game to a replay file of its own from the moment it appears here");
    connect(this->pt_save_replays, SIGNAL(triggered()), this, SLOT(do_toggle_save_peer_replays()));

    this->pt_record_everyone = this->play_together_menu->addAction("Record everyone's replay now");
    this->pt_record_everyone->setObjectName("play-together-record-everyone");
    this->pt_record_everyone->setToolTip("Start a replay of your own game and a fresh replay file for every friend's game at the same moment (any file already being written for them is finished first)");
    connect(this->pt_record_everyone, SIGNAL(triggered()), this, SLOT(do_play_together_record_everyone()));
}

void MainWindow::set_link_input_delay(std::uint8_t frames) {
    supershuckie_frontend_play_together_set_link_input_delay(this->frontend, frames);
    this->refresh_play_together_actions();
}

void MainWindow::do_play_together_unlink() {
    this->play_together->unlink();
    this->refresh_action_states();
}

void MainWindow::refresh_play_together_actions() {
    if(this->frontend == nullptr || this->play_together == nullptr) {
        return;
    }
    bool game_loaded = this->is_game_running();
    bool active = this->play_together->is_active();
    bool host = this->play_together->is_host();
    auto emulator_type = supershuckie_frontend_get_emulator_type(this->frontend);
    bool supported = game_loaded && emulator_type != SuperShuckieEmulatorType::SuperShuckieEmulatorType__NintendoDS;

    this->pt_open->setEnabled(active || supported);
    this->pt_leave->setEnabled(active);
    this->pt_leave->setText(host ? "Stop hosting" : "Leave session");
    this->pt_reset_all->setEnabled(active && host);
    // The host's setting rules the session: a client sees it and cannot change it.
    this->pt_sync_pause->setChecked(supershuckie_frontend_play_together_get_sync_pause(this->frontend));
    this->pt_sync_pause->setEnabled(!active || host);
    this->pt_sync_pause->setText(active && !host ? "Sync pause (set by the host)" : "Sync pause");
    this->pt_start_state->setChecked(supershuckie_frontend_play_together_get_start_state(this->frontend));
    this->pt_start_state->setEnabled(active && host);
    this->pt_reset_all->setText(this->pt_start_state->isChecked() ? "Restart everyone from the start state (race start)" : "Reset everyone (race start)");
    this->pt_show_windows->setEnabled(active);

    // "Everyone" is recording once this game is and at least one friend's file is being written;
    // the action then stops all of them together.
    bool recording_own = supershuckie_frontend_get_replay_state(this->frontend) == SuperShuckieReplayState::SuperShuckieReplayState__Recording;
    bool recording_everyone = recording_own && supershuckie_frontend_play_together_is_recording_peers(this->frontend);
    this->pt_record_everyone->setEnabled(active && game_loaded && (recording_everyone || this->record_replay->isEnabled()));
    this->pt_record_everyone->setText(recording_everyone ? "Stop recording everyone's replay" : "Record everyone's replay now");

    auto scale = supershuckie_frontend_play_together_get_video_scale(this->frontend);
    for(auto *action : this->pt_scale) {
        action->setChecked(action->number == scale);
    }
    auto delay = supershuckie_frontend_play_together_get_link_input_delay(this->frontend);
    for(auto *action : this->pt_link_delay) {
        action->setChecked(action->number == delay);
    }
    bool linked = active && this->play_together->is_link_cable_plugged();
    this->pt_unlink->setEnabled(linked);

    // The game being played together cannot be swapped for a replay or another Game Boy model.
    if(active) {
        this->play_replay->setEnabled(false);
        this->continue_last_replay->setEnabled(false);
        this->set_game_boy_hardware_settings_enabled(false);
    }
    // Two linked games have to stay in step: nothing that changes one of them behind the
    // other's back (the frontend refuses these too; the speed is the host's, and a client's own
    // speed controls simply do nothing while linked).
    if(linked) {
        for(auto &state : this->quick_load_save_states) {
            state->setEnabled(false);
        }
        this->undo_load_save_state->setEnabled(false);
        this->redo_load_save_state->setEnabled(false);
        this->resume_replay->setEnabled(false);
        this->export_video->setEnabled(false);
        this->reload_core->setEnabled(false);
    }
}

void MainWindow::set_peer_video_scale(std::uint8_t scale) {
    this->play_together->set_scale(scale);
    this->refresh_play_together_actions();
}

void MainWindow::do_play_together_open() {
    this->play_together->open_dialog();
}

void MainWindow::do_play_together_leave() {
    this->play_together->leave();
    this->refresh_action_states();
}

void MainWindow::do_play_together_reset_all() {
    this->play_together->reset_all();
}

void MainWindow::do_toggle_play_together_start_state() {
    char error[1024] = {};
    if(!supershuckie_frontend_play_together_set_start_state(this->frontend, this->pt_start_state->isChecked(), reinterpret_cast<uint8_t *>(error), sizeof(error))) {
        this->show_error("Start state", "%s", error);
    }
    this->refresh_play_together_actions();
}

void MainWindow::do_toggle_play_together_sync_pause() {
    char error[512] = {};
    if(!supershuckie_frontend_play_together_set_sync_pause(this->frontend, this->pt_sync_pause->isChecked(), reinterpret_cast<uint8_t *>(error), sizeof(error))) {
        this->show_error("Sync pause", "%s", error);
    }
    this->refresh_play_together_actions();
}

void MainWindow::do_play_together_show_windows() {
    this->play_together->show_windows();
}

void MainWindow::do_toggle_save_peer_replays() {
    supershuckie_frontend_play_together_set_save_peer_replays(this->frontend, this->pt_save_replays->isChecked());
}

void MainWindow::do_play_together_record_everyone() {
    char error[2048] = {};
    bool recording_own = supershuckie_frontend_get_replay_state(this->frontend) == SuperShuckieReplayState::SuperShuckieReplayState__Recording;
    bool recording_everyone = recording_own && supershuckie_frontend_play_together_is_recording_peers(this->frontend);
    bool ok = recording_everyone
        ? supershuckie_frontend_play_together_stop_recording_everyone(this->frontend, reinterpret_cast<uint8_t *>(error), sizeof(error))
        : supershuckie_frontend_play_together_start_recording_everyone(this->frontend, reinterpret_cast<uint8_t *>(error), sizeof(error));
    if(!ok) {
        this->show_error(recording_everyone ? "Stop recording everyone" : "Record everyone", "%s", error);
    }
    this->refresh_action_states();
}
