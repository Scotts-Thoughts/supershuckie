// FIXME: we need this to be somewhere else
#define SUPERSHUCKIE_VERSION "0.4.12stp"

#include <cstdio>
#include <cstdint>
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

#ifdef _WIN32
#include <windows.h>
#include <dwmapi.h>
#endif

#include <supershuckie/supershuckie.h>

#include "ask_for_text_dialog.hpp"
#include "audio_output.hpp"
#include "nds_date_dialog.hpp"
#include "select_item_dialog.hpp"
#include "error.hpp"
#include "file_rw.hpp"
#include "game_speed_dialog.hpp"
#include "render_widget.hpp"
#include "main_window.hpp"
#include "controller_settings_window.hpp"
#include "replay_playback_controls.hpp"
#include "video_export_dialog.hpp"

#include <QProgressDialog>
#include <QThread>
#include <QPushButton>
#include <QCoreApplication>

using namespace SuperShuckie64;

static const char *USE_NUMBER_KEYS_FOR_QUICK_SLOTS = "qt__number_keys_for_quick_slots";
static const char *WINDOW_XY = "qt__window_xy";
static const char *DISPLAY_STATUS_BAR = "qt__display_status_bar";
static const char *KEYBOARD_REPLAY_CONTROLS_DISABLED = "qt__replay_controls_disabled";
static const char *HORIZONTAL_NDS = "qt__horizontal_nds";

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

    char buf[256];
    if(supershuckie_frontend_is_pokeabyte_enabled(this->frontend, buf, sizeof(buf))) {
        this->enable_pokeabyte_integration->setChecked(true);
    }
    else if(buf[0] != 0) {
        DISPLAY_ERROR_DIALOG("Failed to automatically start Poke-A-Byte integration", "An error occurred on startup when trying to enable Poke-A-Byte integration:\n\n%s", buf);
    }
    if(supershuckie_frontend_get_external_commands_enabled(this->frontend, buf, sizeof(buf))) {
        this->enable_external_commands->setChecked(true);
    }
    else if(buf[0] != 0) {
        DISPLAY_ERROR_DIALOG("Failed to automatically start external commands", "An error occurred on startup when trying to enable external commands:\n\n%s", buf);
    }

    const char *quick_slots = supershuckie_frontend_get_custom_setting(this->frontend, USE_NUMBER_KEYS_FOR_QUICK_SLOTS);
    if(quick_slots != nullptr && quick_slots[0] == '1') {
        this->use_number_keys_for_quick_slots = true;
        this->use_number_row_for_quick_slots->setChecked(true);
        this->set_quick_load_shortcuts();
    }

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
        DISPLAY_ERROR_DIALOG("Failed to open the audio device", "Audio has been turned off. Enable it again from the Audio menu to retry.\n\n%s", this->audio->last_error().c_str());
    }

    this->sdl.frontend = this->frontend;
    this->render_widget->setFocus(Qt::OtherFocusReason);
    this->rebuild_recent_roms_menu();

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
    
    if(this->status_bar->isVisible()) {
        std::snprintf(fmt, sizeof(fmt), "Super Shuckie " SUPERSHUCKIE_VERSION " - %s", rom_name);
    }
    else if(this->title_text[0] == 0) {
        std::snprintf(fmt, sizeof(fmt), "Super Shuckie " SUPERSHUCKIE_VERSION " - %s - %.00f FPS", rom_name, this->current_fps);
    }
    else {
        std::snprintf(fmt, sizeof(fmt), "Super Shuckie " SUPERSHUCKIE_VERSION " - %s - %s - %.00f FPS", rom_name, this->title_text, this->current_fps);
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
        this->status_bar_fps->setToolTip(detail);

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
        DISPLAY_ERROR_DIALOG("Error!", "%s", buf);
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

    if(this->last_known_replay_state != state) {
        this->refresh_action_states();
    }

    this->playback_bar->tick();
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
    this->set_up_settings_menu();

    this->refresh_action_states();
}

void MainWindow::set_up_file_menu() {
    this->file_menu = this->menu_bar->addMenu("File");

    this->open_rom = this->file_menu->addAction("Open ROM...");
    this->open_rom->setShortcut(QKeyCombination(Qt::ControlModifier, Qt::Key_O));
    connect(this->open_rom, SIGNAL(triggered()), this, SLOT(do_open_rom()));

    this->recent_roms_menu = this->file_menu->addMenu("Open recent ROM");

    this->close_rom = this->file_menu->addAction("Close ROM");
    this->close_rom->setShortcut(QKeyCombination(Qt::ControlModifier, Qt::Key_W));
    connect(this->close_rom, SIGNAL(triggered()), this, SLOT(do_close_rom()));

    this->unload_rom = this->file_menu->addAction("Unload ROM without saving");
    this->unload_rom->setShortcut(QKeyCombination(Qt::ControlModifier | Qt::ShiftModifier, Qt::Key_W));
    connect(this->unload_rom, SIGNAL(triggered()), this, SLOT(do_unload_rom()));

    this->file_menu->addSeparator();
    this->screenshot = this->file_menu->addAction("Screenshot");
    this->screenshot->setShortcut(QKeyCombination(Qt::Key_F12));
    connect(this->screenshot, SIGNAL(triggered()), this, SLOT(do_screenshot()));

    this->file_menu->addSeparator();
    auto *open_user_dir = this->file_menu->addAction("Open data directory");
    connect(open_user_dir, SIGNAL(triggered()), this, SLOT(do_open_user_dir()));

    this->quit = this->file_menu->addAction("Quit");
    this->quit->setShortcut(QKeyCombination(Qt::ControlModifier, Qt::Key_Q));
    connect(this->quit, SIGNAL(triggered()), this, SLOT(close()));
}

void MainWindow::set_up_gameplay_menu() {
    this->gameplay_menu = this->menu_bar->addMenu("Gameplay");

    this->new_game = this->gameplay_menu->addAction("New game...");
    this->new_game->setShortcut(QKeyCombination(Qt::ControlModifier, Qt::Key_N));
    connect(this->new_game, SIGNAL(triggered()), this, SLOT(do_new_game()));

    this->load_game = this->gameplay_menu->addAction("Load game...");
    connect(this->load_game, SIGNAL(triggered()), this, SLOT(do_load_game()));

    this->save_game = this->gameplay_menu->addAction("Save game");
    this->save_game->setShortcut(QKeyCombination(Qt::ControlModifier, Qt::Key_S));
    connect(this->save_game, SIGNAL(triggered()), this, SLOT(do_save_game()));

    this->save_new_game = this->gameplay_menu->addAction("Save as new game...");
    this->save_new_game->setShortcut(QKeyCombination(Qt::ControlModifier | Qt::ShiftModifier, Qt::Key_S));
    connect(this->save_new_game, SIGNAL(triggered()), this, SLOT(do_save_new_game()));

    this->gameplay_menu->addSeparator();

    this->reset_console = this->gameplay_menu->addAction("Reset console");
    connect(this->reset_console, SIGNAL(triggered()), this, SLOT(do_reset_console()));

    this->reload_core = this->gameplay_menu->addAction("Reload core");
    connect(this->reload_core, SIGNAL(triggered()), this, SLOT(do_reload_core()));

    this->pause = this->gameplay_menu->addAction("Pause");
    this->pause->setCheckable(true);
    this->pause->setShortcut(QKeyCombination(Qt::ControlModifier, Qt::Key_P));
    connect(this->pause, SIGNAL(triggered()), this, SLOT(do_toggle_pause()));

    this->gameplay_menu->addSeparator();
    this->auto_unpause_on_input = this->gameplay_menu->addAction("Unpause on input");
    this->auto_unpause_on_input->setCheckable(true);
    connect(this->auto_unpause_on_input, SIGNAL(triggered()), this, SLOT(do_toggle_auto_unpause_on_input()));
}

void MainWindow::set_up_save_states_menu() {
    this->save_states_menu = this->menu_bar->addMenu("Save states");

    this->quick_slots = this->save_states_menu->addMenu("Quick slot");
    for(std::size_t i = 1; i <= MainWindow::QUICK_SAVE_STATE_COUNT; i++) {
        char fmt[64];

        std::snprintf(fmt, sizeof(fmt), "Quick slot #%zu", i);
        QMenu *menu = quick_slots->addMenu(fmt);

        std::snprintf(fmt, sizeof(fmt), "Load quick slot #%zu", i);
        auto *quick_load = new NumberedAction(this, fmt, i, &MainWindow::quick_load);

        std::snprintf(fmt, sizeof(fmt), "Save quick slot #%zu", i);
        auto *quick_save = new NumberedAction(this, fmt, i, &MainWindow::quick_save);

        this->quick_load_save_states[i - 1] = quick_load;
        menu->addAction(quick_load);
        this->quick_save_save_states[i - 1] = quick_save;
        menu->addAction(quick_save);
    }

    quick_slots->addSeparator();
    
    this->use_number_row_for_quick_slots = quick_slots->addAction("Use number row instead of function keys");
    this->use_number_row_for_quick_slots->setCheckable(true);
    connect(this->use_number_row_for_quick_slots, SIGNAL(triggered()), this, SLOT(do_toggle_number_row_for_save_states()));

    this->save_states_menu->addSeparator();
    
    this->undo_load_save_state = this->save_states_menu->addAction("Undo load save state");
    this->undo_load_save_state->setShortcut(QKeyCombination(Qt::ControlModifier, Qt::Key_U));
    connect(this->undo_load_save_state, SIGNAL(triggered()), this, SLOT(do_undo_load_save_state()));
    
    this->redo_load_save_state = this->save_states_menu->addAction("Redo load save state");
    this->redo_load_save_state->setShortcut(QKeyCombination(Qt::ControlModifier | Qt::ShiftModifier, Qt::Key_U));
    connect(this->redo_load_save_state, SIGNAL(triggered()), this, SLOT(do_redo_load_save_state()));

    this->set_quick_load_shortcuts();
}

void MainWindow::set_up_replays_menu() {
    this->replays_menu = this->menu_bar->addMenu("Replays");
    
    this->record_replay = this->replays_menu->addAction("Record (unset)");
    this->resume_replay = this->replays_menu->addAction("Resume recording replay");

    this->replays_menu->addSeparator();

    this->auto_pause_on_record = this->replays_menu->addAction("Start recordings paused");
    connect(this->auto_pause_on_record, SIGNAL(triggered()), this, SLOT(do_toggle_auto_pause_on_record()));
    this->auto_pause_on_record->setCheckable(true);

    this->disable_save_states_when_recording = this->replays_menu->addAction("Disable save states when recording");
    connect(this->disable_save_states_when_recording, SIGNAL(triggered()), this, SLOT(do_toggle_disable_save_states_when_recording()));
    this->disable_save_states_when_recording->setCheckable(true);

    this->disable_speed_changes_when_recording = this->replays_menu->addAction("Disable speed changes when recording");
    connect(this->disable_speed_changes_when_recording, SIGNAL(triggered()), this, SLOT(do_toggle_disable_speed_changes_when_recording()));
    this->disable_speed_changes_when_recording->setCheckable(true);

    // zstd level for new recordings and conversions. Only 19 buys anything over 9 (about 10% smaller
    // files) and it costs roughly twice the conversion time; 3 is what pre-v4 versions used.
    auto *compression_items = this->replays_menu->addMenu("Replay compression");
    this->replay_compression_levels[0] = new NumberedAction(this, "Fastest (level 1)", 1, &MainWindow::set_replay_compression_level);
    this->replay_compression_levels[1] = new NumberedAction(this, "Fast (level 3)", 3, &MainWindow::set_replay_compression_level);
    this->replay_compression_levels[2] = new NumberedAction(this, "Balanced (level 9, default)", 9, &MainWindow::set_replay_compression_level);
    this->replay_compression_levels[3] = new NumberedAction(this, "Smallest (level 19, slow to write)", 19, &MainWindow::set_replay_compression_level);
    for(auto *level : this->replay_compression_levels) {
        level->setCheckable(true);
        compression_items->addAction(level);
    }
    // Shown (checked and disabled) only when settings.json holds a level that is not one of the above.
    this->replay_compression_custom = compression_items->addAction("Custom");
    this->replay_compression_custom->setCheckable(true);
    this->replay_compression_custom->setEnabled(false);
    this->replay_compression_custom->setVisible(false);

    this->replays_menu->addSeparator();

    this->play_replay = this->replays_menu->addAction("Play (unset)");
    this->continue_last_replay = this->replays_menu->addAction("Continue last replay");

    this->replays_menu->addSeparator();

    this->export_video = this->replays_menu->addAction("Export video…");

    this->replays_menu->addSeparator();

    this->convert_replay = this->replays_menu->addAction("Convert replay to current format…");
    this->convert_replay_folder = this->replays_menu->addAction("Convert folder of replays to current format…");

    connect(this->record_replay, SIGNAL(triggered()), this, SLOT(do_record_replay()));
    connect(this->resume_replay, SIGNAL(triggered()), this, SLOT(do_resume_replay()));
    connect(this->play_replay, SIGNAL(triggered()), this, SLOT(do_play_replay()));
    connect(this->continue_last_replay, SIGNAL(triggered()), this, SLOT(do_continue_last_replay()));
    connect(this->export_video, SIGNAL(triggered()), this, SLOT(do_export_video()));
    connect(this->convert_replay, SIGNAL(triggered()), this, SLOT(do_convert_replay()));
    connect(this->convert_replay_folder, SIGNAL(triggered()), this, SLOT(do_convert_replay_folder()));

    this->record_replay->setShortcut(QKeyCombination(Qt::ControlModifier, Qt::Key_R));
    this->resume_replay->setShortcut(QKeyCombination(Qt::ShiftModifier | Qt::ControlModifier, Qt::Key_R));
    this->play_replay->setShortcut(QKeyCombination(Qt::ShiftModifier | Qt::ControlModifier, Qt::Key_P));
    this->continue_last_replay->setShortcut(QKeyCombination(Qt::ShiftModifier | Qt::ControlModifier, Qt::Key_C));
    this->export_video->setShortcut(QKeyCombination(Qt::ShiftModifier | Qt::ControlModifier, Qt::Key_E));

    this->replays_menu->addSeparator();

    this->auto_stop_replay_on_input = this->replays_menu->addAction("Stop playback on input");
    this->auto_stop_replay_on_input->setCheckable(true);
    connect(this->auto_stop_replay_on_input, SIGNAL(triggered()), this, SLOT(do_toggle_stop_replay_on_input()));

    this->keyboard_replay_controls = this->replays_menu->addAction("Allow keyboard to control replay playback");
    connect(this->keyboard_replay_controls, SIGNAL(triggered()), this, SLOT(do_toggle_replay_keyboard_controls()));
    this->keyboard_replay_controls->setCheckable(true);
    this->keyboard_replay_controls->setChecked(true);

    this->ignore_speed_changes_in_replay = this->replays_menu->addAction("Ignore speed changes in replay");
    connect(this->ignore_speed_changes_in_replay, SIGNAL(triggered()), this, SLOT(do_toggle_ignore_speed_changes_in_replay()));
    this->ignore_speed_changes_in_replay->setCheckable(true);

    this->auto_resync_keyframes_in_replay = this->replays_menu->addAction("Auto-resync keyframes in replay");
    connect(this->auto_resync_keyframes_in_replay, SIGNAL(triggered()), this, SLOT(do_toggle_auto_resync_keyframes_in_replay()));
    this->auto_resync_keyframes_in_replay->setCheckable(true);
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
        DISPLAY_ERROR_DIALOG("Failed to create save state", "%s", error);
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
        DISPLAY_ERROR_DIALOG("Failed to load save state", "%s", error);
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
    this->audio_enabled->setCheckable(true);
    connect(this->audio_enabled, SIGNAL(triggered()), this, SLOT(do_toggle_audio_enabled()));

    this->audio_muted = this->audio_menu->addAction("Mute");
    this->audio_muted->setCheckable(true);
    connect(this->audio_muted, SIGNAL(triggered()), this, SLOT(do_toggle_audio_muted()));

    // Someone who turbos through one stretch and plays the next at 1x should not get a barrage of
    // sped-up audio in between; the emulator drops the samples while the speed is not 1x.
    this->audio_mute_when_sped_up = this->audio_menu->addAction("Mute when sped up");
    this->audio_mute_when_sped_up->setCheckable(true);
    connect(this->audio_mute_when_sped_up, SIGNAL(triggered()), this, SLOT(do_toggle_audio_mute_when_sped_up()));

    this->audio_volume_menu = this->audio_menu->addMenu("Volume");
    for(std::size_t i = 0; i < MainWindow::AUDIO_VOLUME_STEPS; i++) {
        auto percent = static_cast<std::uint8_t>((i + 1) * 100 / MainWindow::AUDIO_VOLUME_STEPS);
        char fmt[32];
        std::snprintf(fmt, sizeof(fmt), "%u%%", static_cast<unsigned>(percent));
        auto *action = new NumberedAction(this, fmt, percent, &MainWindow::set_audio_volume);
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
        action->setCheckable(true);
        buffer_menu->addAction(action);
        this->audio_buffers[i] = action;
    }
}

void MainWindow::set_up_settings_menu() {
    this->settings_menu = this->menu_bar->addMenu("Settings");

    auto *game_speed = this->settings_menu->addAction("Game speed...");
    connect(game_speed, SIGNAL(triggered()), this, SLOT(do_open_game_speed_dialog()));

    auto *controller_settings = this->settings_menu->addAction("Controls settings...");
    connect(controller_settings, SIGNAL(triggered()), this, SLOT(do_open_controls_settings_dialog()));
    
    auto *video_scaling = this->settings_menu->addMenu("Video scaling");
    for(std::size_t i = 1; i <= MainWindow::VIDEO_SCALE_COUNT; i++) {
        char fmt[256];
        std::snprintf(fmt, sizeof(fmt), "%zux", i);

        auto *action = new NumberedAction(this, fmt, static_cast<uint8_t>(i), &MainWindow::set_video_scale);
        video_scaling->addAction(action);
        this->change_video_scale[i - 1] = action;
        action->setCheckable(true);
    }

    this->settings_menu->addSeparator();

    this->game_boy_settings = this->settings_menu->addMenu("Game Boy settings");

    auto *gbc_mode_items = this->game_boy_settings->addMenu("Game Boy Color mode");

    this->gbc_mode[0] = new NumberedAction(this, "Always Game Boy Color", SuperShuckieGBCMode::SuperShuckieGBCMode__AlwaysGBC, &MainWindow::set_gbc_mode);
    this->gbc_mode[1] = new NumberedAction(this, "Game Boy Color games only", SuperShuckieGBCMode::SuperShuckieGBCMode__GBInGBMode, &MainWindow::set_gbc_mode);
    this->gbc_mode[2] = new NumberedAction(this, "Always Game Boy", SuperShuckieGBCMode::SuperShuckieGBCMode__AlwaysGB, &MainWindow::set_gbc_mode);

    for(auto m : this->gbc_mode) {
        m->setCheckable(true);
        gbc_mode_items->addAction(m);
    }

    this->sgb_enabled = this->game_boy_settings->addAction("Enable SGB colors");
    connect(this->sgb_enabled, SIGNAL(triggered()), this, SLOT(do_toggle_sgb()));
    this->sgb_enabled->setCheckable(true);

    auto *nds_settings = this->settings_menu->addMenu("Nintendo DS settings");
    auto *set_nds_date = nds_settings->addAction("Set date...");
    connect(set_nds_date, SIGNAL(triggered()), this, SLOT(do_open_nds_date_dialog()));
    
    this->horizontal_nds = nds_settings->addAction("Arrange horizontally");
    this->horizontal_nds->setCheckable(true);
    connect(this->horizontal_nds, SIGNAL(triggered()), this, SLOT(do_toggle_horizontal_nds()));

    this->swap_nds_screens = nds_settings->addAction("Swap screens");
    this->swap_nds_screens->setCheckable(true);
    connect(this->swap_nds_screens, SIGNAL(triggered()), this, SLOT(do_toggle_swap_nds_screens()));

    this->nds_jit = nds_settings->addAction("Enable JIT (disables replays)");
    this->nds_jit->setCheckable(true);
    connect(this->nds_jit, SIGNAL(triggered()), this, SLOT(do_toggle_nds_jit()));

    this->settings_menu->addSeparator();

    this->enable_pokeabyte_integration = this->settings_menu->addAction("Enable Poke-A-Byte integration");
    this->enable_pokeabyte_integration->setCheckable(true);
    connect(this->enable_pokeabyte_integration, SIGNAL(triggered()), this, SLOT(do_toggle_pokeabyte()));

    this->enable_external_commands = this->settings_menu->addAction("Enable external commands");
    this->enable_external_commands->setCheckable(true);
    connect(this->enable_external_commands, SIGNAL(triggered()), this, SLOT(do_toggle_external_commands()));

    this->settings_menu->addSeparator();

    this->show_status_bar = this->settings_menu->addAction("Show status bar");
    this->show_status_bar->setCheckable(true);
    connect(this->show_status_bar, SIGNAL(triggered()), this, SLOT(do_toggle_status_bar()));
}

void MainWindow::refresh_action_states() {
    bool game_loaded = this->is_game_running();

    auto replay_state = this->frontend != nullptr ?
        supershuckie_frontend_get_replay_state(this->frontend) : SuperShuckieReplayState::SuperShuckieReplayState__NoReplay;

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
        && replay_state != SuperShuckieReplayState::SuperShuckieReplayState__Playback;

    for(auto &state : this->quick_load_save_states) {
        state->setEnabled(enable_load_save_state_buttons);
    }

    this->redo_load_save_state->setEnabled(enable_load_save_state_buttons);
    this->undo_load_save_state->setEnabled(enable_load_save_state_buttons);

    this->record_replay->setText("Record replay");
    this->play_replay->setText("Play replay");

    this->play_replay->setEnabled(game_loaded);
    this->record_replay->setEnabled(game_loaded);
    this->resume_replay->setEnabled(game_loaded);
    this->export_video->setEnabled(game_loaded);
    this->convert_replay->setEnabled(true);
    this->convert_replay_folder->setEnabled(true);
    this->game_boy_settings->setEnabled(true);

    this->reload_core->setEnabled(game_loaded);
    this->reset_console->setEnabled(game_loaded);

    for(auto &scale : this->change_video_scale) {
        scale->setEnabled(game_loaded);
    }

    auto gbc_mode = this->frontend != nullptr ? supershuckie_frontend_get_gbc_mode(this->frontend) : 0;
    for(auto &i : this->gbc_mode) {
        i->setChecked(i->number == gbc_mode);
    }

    this->continue_last_replay->setEnabled(this->frontend != nullptr && supershuckie_frontend_can_continue_last_replay(this->frontend));

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
            this->game_boy_settings->setEnabled(false);
            break;

        case SuperShuckieReplayState::SuperShuckieReplayState__Playback:
            this->record_replay->setEnabled(false);
            // resume_replay stays enabled here: resuming while watching continues from the
            // current playback frame into a new, separate replay.
            this->reload_core->setEnabled(false);
            this->reset_console->setEnabled(false);
            this->export_video->setEnabled(false);
            this->current_state->setText("PLAYBACK");
            this->current_state->show();

            this->play_replay->setText("Stop replay");
            this->game_boy_settings->setEnabled(false);
            break;

        case SuperShuckieReplayState::SuperShuckieReplayState__NoReplay:
            this->current_state->hide();
            break;
    }

    this->last_known_replay_state = replay_state;
}

void MainWindow::do_open_rom() {
    QFileDialog rom_opener;
    rom_opener.setFileMode(QFileDialog::FileMode::ExistingFile);
    rom_opener.setNameFilters(QStringList({
        "All compatible ROM files (*.gb *.gbc *.gba *.nds)",
        "GB/GBC ROM dumps (*.gb *.gbc)",
        "GBA ROM dumps (*.gba)",
        "NDS ROM files (*.nds)",
        "Any files (*)"
    }));
    rom_opener.setWindowTitle("Select a ROM to open");
    rom_opener.exec();

    auto files = rom_opener.selectedFiles();
    if(files.size() != 1) {
        return;
    }

    this->load_rom(files[0].toStdString());
}

void MainWindow::load_rom(const std::filesystem::path &path) {
    char error[256] = "";

    auto path_string = path.string();
    if(!supershuckie_frontend_load_rom(this->frontend, path.string().c_str(), error, sizeof(error))) {
        DISPLAY_ERROR_DIALOG("Can't load ROM", "\"%s\" failed to load:\n\n%s", path_string.c_str(), error);
    }

    this->rebuild_recent_roms_menu();
}

void MainWindow::load_rom(const char *path) {
    this->load_rom(std::filesystem::path(path));
}

void MainWindow::do_close_rom() {
    supershuckie_frontend_close_rom(this->frontend);
    supershuckie_frontend_set_paused(this->frontend, false);
}

void MainWindow::do_unload_rom() {
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
        DISPLAY_ERROR_DIALOG("Screenshot", "%s", "No frame is available to capture. Load a ROM first.");
        return;
    }

    // Screenshots live in a "screenshots" folder alongside the ROM's replays and save data.
    std::size_t len = supershuckie_frontend_get_screenshot_directory(this->frontend, nullptr, 0);
    if(len == 0) {
        DISPLAY_ERROR_DIALOG("Screenshot", "%s", "Could not determine where to save the screenshot.");
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
        DISPLAY_ERROR_DIALOG("Screenshot", "Failed to save screenshot to:\n\n%s", path.toStdString().c_str());
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
        DISPLAY_ERROR_DIALOG("Can't save SRAM", "%s", err);
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
        this->quick_save_save_states[i]->setShortcut(QKeyCombination(control | Qt::ShiftModifier, key));
        this->quick_load_save_states[i]->setShortcut(QKeyCombination(control, key));
    }
}

void MainWindow::closeEvent(QCloseEvent *event) {
    QWidget::closeEvent(event);

    if(this->frontend) {
        char xy[256];
        auto geometry = this->geometry();
        std::snprintf(xy, sizeof(xy), "%d|%d", geometry.x(), geometry.y());
        supershuckie_frontend_set_custom_setting(this->frontend, WINDOW_XY, xy);
        supershuckie_frontend_stop_recording_replay(this->frontend);
        supershuckie_frontend_write_settings(this->frontend);
        supershuckie_frontend_save_sram(this->frontend, nullptr, 0);
    }

    // if(!this->try_unload_rom()) {
        // event->ignore();
    // }
}

MainWindow::~MainWindow() {
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
        supershuckie_frontend_stop_recording_replay(this->frontend);
        this->set_title(saved);
    }
    else {
        char result[256];
        if(supershuckie_frontend_start_recording_replay(this->frontend, nullptr, result, sizeof(result))) {
            char fmt[512];
            std::snprintf(fmt, sizeof(fmt), "Started recording replay \"%s\"", result);
            this->set_title(fmt);
        }
        else {
            DISPLAY_ERROR_DIALOG("Failed to start recording replay", "%s", result);
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
        DISPLAY_ERROR_DIALOG("Failed to resume recording replay", "%s", result);
    }

    this->refresh_action_states();
}

void MainWindow::do_play_replay() {
    if(supershuckie_frontend_get_replay_state(this->frontend) != SuperShuckieReplayState::SuperShuckieReplayState__NoReplay) {
        supershuckie_frontend_stop_replay_playback(this->frontend);
        this->set_title("Closed replay");
        return;
    }

    auto replays = wrap_array_std(supershuckie_frontend_get_all_replays_for_rom(this->frontend, nullptr));
    auto text = SelectItemDialog::ask(this, replays, "Select a replay", "Select a replay file to play.");
    if(text == std::nullopt) {
        return;
    }

    char err[512];
    char fmt[512];
    
    if(!supershuckie_frontend_load_replay(this->frontend, text->c_str(), false, err, sizeof(err))) {
        std::snprintf(fmt, sizeof(fmt), "%s", err);
        DISPLAY_ERROR_DIALOG("Replay file issues detected", "%s", fmt);

        if(!supershuckie_frontend_load_replay(this->frontend, text->c_str(), true, err, sizeof(err))) {
            return;
        }
    }

    if(!supershuckie_frontend_get_replay_playback_time(this->frontend, nullptr, nullptr)) {
        return;
    }

    std::snprintf(fmt, sizeof(fmt), "Opened replay file \"%s\"", text->c_str());
    this->set_title(fmt);

    this->refresh_action_states();
}

void MainWindow::do_export_video() {
    auto replays = wrap_array_std(supershuckie_frontend_get_all_replays_for_rom(this->frontend, nullptr));
    if(replays.empty()) {
        DISPLAY_ERROR_DIALOG("Export video", "%s", "No replays found for this ROM.");
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
        DISPLAY_ERROR_DIALOG("Export failed to start", "%s", err);
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
            DISPLAY_ERROR_DIALOG("Export failed", "%s", poll_err);
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
        DISPLAY_ERROR_DIALOG("Convert replays", "%s", err);
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

    self->refresh_action_states();
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

void MainWindow::do_toggle_pokeabyte() {
    char err[256];

    bool enabled = this->enable_pokeabyte_integration->isChecked();
    if(!supershuckie_frontend_set_pokeabyte_enabled(this->frontend, enabled, err, sizeof(err))) {
        DISPLAY_ERROR_DIALOG("Failed to enable Poke-A-Byte integration", "An error occurred when enabling Poke-A-Byte integration:\n\n%s", err);
        this->enable_pokeabyte_integration->setChecked(false);
    }
}

void MainWindow::do_toggle_stop_replay_on_input() {
    supershuckie_frontend_set_auto_stop_playback_on_input_setting(this->frontend, this->auto_stop_replay_on_input->isChecked());
}

void MainWindow::start_timer() {
    this->timer_stack--;
    if(this->timer_stack == 0) {
        this->ticker.start();
    }
    if(this->timer_stack < 0) {
        DISPLAY_ERROR_DIALOG("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA", "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA");
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
        DISPLAY_ERROR_DIALOG("Failed to open the audio device", "%s", this->audio->last_error().c_str());
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
        DISPLAY_ERROR_DIALOG("Failed to start remote commands", "%s", buf);
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
        DISPLAY_ERROR_DIALOG("Failed to continue replay", "%s", buf);
    }
}

void MainWindow::do_toggle_disable_save_states_when_recording() {
    supershuckie_frontend_set_disable_save_states_when_recording(this->frontend, this->disable_save_states_when_recording->isChecked());
    this->refresh_action_states();
}

void MainWindow::do_toggle_disable_speed_changes_when_recording() {
    supershuckie_frontend_set_disable_speed_changes_when_recording(this->frontend, this->disable_speed_changes_when_recording->isChecked());
}
