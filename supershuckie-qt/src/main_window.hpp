#ifndef __SUPERSHUCKIE_MAIN_WINDOW_HPP__
#define __SUPERSHUCKIE_MAIN_WINDOW_HPP__

#include <QMainWindow>
#include <QTimer>
#include <filesystem>
#include <memory>
#include <chrono>
#include <supershuckie/supershuckie.h>
#include "sdl_event_wrapper.hpp"
#include "display_sync.hpp"
#include "shortcuts_settings_window.hpp"

class QMenu;
class QAction;
class QCloseEvent;
class QKeyEvent;
class QLabel;
class QToolButton;

namespace SuperShuckie64 {

class GameRenderWidget;
class NumberedAction;
class GameSpeedDialog;
class SuperShuckieTimestamp;
class AskForTextDialog;
class SelectItemDialog;
class SelectReplayDialog;
class ControlsSettingsWindow;
class ReplayPlaybackControls;
class NDSDateDialog;
class GBPaletteDialog;
class StringAction;
class VideoExportDialog;
class AudioOutput;
class MemoryToolsController;
class BookmarkWindow;
class BookmarkTypesDialog;
class AddBookmarkDialog;
class LandingWidget;
class PlayTogetherController;
class PeerWindow;
class PlayTogetherDialog;

std::vector<std::string> wrap_array_std(SuperShuckieStringArrayRaw *array);

enum ReplayStatus {
    NoReplay,
    Recording,
    PlayingBack
};

class MainWindow: public QMainWindow {
    Q_OBJECT
    friend GameRenderWidget;
    friend NumberedAction;
    friend GameSpeedDialog;
    friend AskForTextDialog;
    friend SelectItemDialog;
    friend SelectReplayDialog;
    friend ControlsSettingsWindow;
    friend ShortcutsSettingsWindow;
    friend ReplayPlaybackControls;
    friend NDSDateDialog;
    friend GBPaletteDialog;
    friend StringAction;
    friend VideoExportDialog;
    friend MemoryToolsController;
    friend BookmarkWindow;
    friend BookmarkTypesDialog;
    friend AddBookmarkDialog;
    friend LandingWidget;
    friend PlayTogetherController;
    friend PeerWindow;
    friend PlayTogetherDialog;
    
public:
    MainWindow();
    ~MainWindow();

    void load_rom(const std::filesystem::path &path);
    void load_rom(const char *path);

    /**
     * Show a modal, MainWindow-parented error dialog while the 1 ms ticker is paused, so
     * `tick()` can't re-enter `supershuckie_frontend_tick` underneath it. Pairs stop_timer()
     * and start_timer() itself (nesting-safe via the stack counter) so callers can never leave
     * one unbalanced with an early return.
     */
    void show_error(const char *title, const char *fmt, ...);

private:
    typedef std::chrono::steady_clock clock;

    void set_title(const char *title = "");
    GameRenderWidget *render_widget;

    // Shown in the game view's place while no ROM is loaded (see update_landing_visibility()).
    LandingWidget *landing_widget;
    void update_landing_visibility();

    SuperShuckieFrontendRaw *frontend = nullptr;

    QTimer ticker;

    void tick();

    void set_up_menu();
    QMenuBar *menu_bar;

    QMenu *file_menu;
    QMenu *gameplay_menu;
    QMenu *save_states_menu;
    QMenu *replays_menu;
    QMenu *audio_menu;
    QMenu *tools_menu;
    QMenu *settings_menu;
    QMenu *recent_roms_menu;
    QMenu *favorite_roms_menu;
    QAction *favorite_roms_none;
    std::vector<QAction *> favorite_rom_actions;

    QAction *undo_load_save_state;
    QAction *redo_load_save_state;

    QStatusBar *status_bar;
    QLabel *status_bar_fps;
    SuperShuckieTimestamp *status_bar_time;

    QAction *open_rom;
    QAction *close_rom;
    QAction *unload_rom;
    QAction *screenshot;

    QAction *new_game;
    QAction *load_game;
    QAction *save_game;
    QAction *save_new_game;
    QAction *reset_console;
    QAction *reload_core;
    QMenu *nds_date_menu;
    QAction *nds_date_no_presets;
    // The first presets get fixed actions so shortcuts can be bound to them; any more are
    // made (and remade) by rebuild_nds_date_menu().
    static const std::size_t NDS_DATE_PRESET_SLOTS = 9;
    QAction *nds_date_preset_slots[NDS_DATE_PRESET_SLOTS];
    std::vector<QAction *> nds_date_preset_extras;
    QAction *pause;
    QAction *quit;

    QAction *record_replay;
    QAction *resume_replay;
    QAction *play_replay;
    QAction *close_replay;
    // The timeline's stop/resume and back-to-the-resume-point buttons, as menu actions so they
    // can be given shortcuts (they have to work while the keyboard is game input, unlike the
    // "Replay playback" controls the render widget matches during playback).
    QAction *stop_playback;
    QAction *resume_playback;
    QAction *go_to_resume_point;
    QAction *export_video;
    QAction *convert_replay;
    QAction *convert_replay_folder;
    QAction *auto_stop_replay_on_input;
    QAction *auto_unpause_on_input;
    QAction *auto_pause_on_record;
    QAction *keyboard_replay_controls;
    QAction *horizontal_nds;
    QAction *swap_nds_screens;
    QAction *nds_jit;
    QAction *ignore_speed_changes_in_replay;
    QAction *auto_resync_keyframes_in_replay;
    QAction *continue_last_replay;
    QAction *disable_save_states_when_recording;
    QAction *disable_speed_changes_when_recording;
    NumberedAction *replay_compression_levels[4];
    QAction *replay_compression_custom;

    QAction *add_bookmark;
    QAction *add_keyframe_bookmark;
    QAction *toggle_range_bookmark;
    QAction *add_bookmark_at_frame;
    QAction *open_bookmarks;
    BookmarkWindow *bookmark_window = nullptr;
    void add_bookmark_now(bool keyframe);
    QWidget *bookmark_dialog_parent();

    QAction *sgb_enabled;
    QMenu *game_boy_settings;
    QMenu *gbc_mode_items;
    NumberedAction *gbc_mode[3];
    QAction *gb_custom_colors;
    void set_game_boy_hardware_settings_enabled(bool enabled);

    std::unique_ptr<AudioOutput> audio;
    QAction *audio_enabled;
    QAction *audio_muted;
    QAction *audio_mute_when_sped_up;
    QMenu *audio_volume_menu;
    static const std::size_t AUDIO_VOLUME_STEPS = 10;
    NumberedAction *audio_volumes[AUDIO_VOLUME_STEPS];
    static const std::size_t AUDIO_BUFFER_PRESETS = 3;
    static const std::uint16_t audio_buffer_ms[AUDIO_BUFFER_PRESETS];
    NumberedAction *audio_buffers[AUDIO_BUFFER_PRESETS];
    float audio_frequency_ratio = 1.0f;

    QLabel *current_state;
    QLabel *paused_state;
    QToolButton *frozen_state;
    QLabel *ram_modified_state;
    QAction *unfreeze_all;
    QAction *confirm_ram_writes;
    int memory_status_countdown = 0;
    void update_memory_status();
    bool check_freezes_before_recording();

    ReplayPlaybackControls *playback_bar;

    QAction *use_number_row_for_quick_slots;
    QAction *show_status_bar;
    QAction *sync_display_to_refresh;
    // Runs while "Sync display to monitor refresh" is on; see DisplaySyncThread.
    std::unique_ptr<DisplaySyncThread> display_sync;
    void apply_display_sync(bool on);
    // Diagnostics since display sync was last switched on; see present_frame().
    std::uint64_t late_presents = 0;
    std::int64_t worst_present_us = 0;
    QAction *enable_pokeabyte_integration;
    QAction *pokeabyte_port;
    QAction *pokeabyte_serve_friends;
    QAction *enable_external_commands;

    SuperShuckieReplayState last_known_replay_state = SuperShuckieReplayState::SuperShuckieReplayState__NoReplay;
    // Playback has a sub-state (the replay stopped, the game live under the user) that changes what the menus allow.
    bool last_known_replay_stopped = false;

    static const std::size_t QUICK_SAVE_STATE_COUNT = 9;

    QAction *quick_load_save_states[QUICK_SAVE_STATE_COUNT];
    QAction *quick_save_save_states[QUICK_SAVE_STATE_COUNT];

    QAction *playback_toggle_pause;
    QAction *playback_skip_back;
    QAction *playback_skip_forward;
    QAction *playback_step_back;
    QAction *playback_step_forward;
    QAction *playback_action_for(const QKeyEvent *event) const;

    std::vector<ShortcutBinding> shortcut_bindings;
    void set_up_shortcuts();
    void collect_shortcut_bindings(QMenu *menu, const QStringList &path);
    void set_default_shortcut(QAction *action, const QKeySequence &shortcut);
    void load_shortcuts();
    void save_shortcuts();
    void apply_shortcuts();

    static const std::size_t VIDEO_SCALE_COUNT = 12;

    NumberedAction *change_video_scale[VIDEO_SCALE_COUNT];

    bool use_number_keys_for_quick_slots = false;

    bool replay_time_shown = false;
    bool temporarily_paused = false;

    void set_up_file_menu();
    void set_up_gameplay_menu();
    void set_up_save_states_menu();
    void set_up_replays_menu();
    void set_up_audio_menu();
    void set_up_tools_menu();
    void set_up_settings_menu();

    MemoryToolsController *memory_tools = nullptr;

    // Play Together (playing alongside other players over the network); see play_together_controller.hpp.
    PlayTogetherController *play_together = nullptr;
    QMenu *play_together_menu;
    QAction *pt_open;
    QAction *pt_leave;
    QAction *pt_reset_all;
    QAction *pt_sync_pause;
    QAction *pt_start_state;
    QAction *pt_show_windows;
    QAction *pt_save_replays;
    QAction *pt_record_everyone;
    QAction *pt_unlink;
    static const std::size_t LINK_DELAY_COUNT = 16;
    NumberedAction *pt_link_delay[LINK_DELAY_COUNT];
    static const std::size_t PEER_SCALE_COUNT = 10;
    NumberedAction *pt_scale[PEER_SCALE_COUNT];
    void set_up_play_together_menu();
    void set_peer_video_scale(std::uint8_t scale);
    void set_link_input_delay(std::uint8_t frames);
    void refresh_play_together_actions();

    void apply_audio_gain();
    void set_audio_volume(std::uint8_t percent);
    void set_audio_buffer(std::uint8_t preset);

    void rebuild_recent_roms_menu() noexcept;

    // The start screen's favorite ROMs, also listed in File so each can have a shortcut. Their
    // shortcut bindings come and go with the favorites (see rebuild_favorite_roms_menu()).
    void rebuild_favorite_roms_menu();
    void open_favorite_rom(const QString &path, const QString &name);
    QList<QKeySequence> favorite_rom_shortcuts(const QString &path) const;
    void edit_favorite_rom_shortcut(const QString &path);
    void clear_favorite_rom_shortcut(const QString &path);
    void open_shortcuts_dialog(const QString &focus_id = QString());
    void rebuild_nds_date_menu();
    void refresh_nds_date_preset_states();
    void apply_nds_date_preset(std::size_t index);
    bool is_nds_game_running();

    void refresh_action_states();
    void set_quick_load_shortcuts();

    void quick_save(std::uint8_t index);
    void quick_load(std::uint8_t index);
    void set_gbc_mode(std::uint8_t mode);
    void set_replay_compression_level(std::uint8_t level);

    void make_save_state(const char *state);
    void load_save_state(const char *state);

    void set_video_scale(std::uint8_t scale);

    void closeEvent(QCloseEvent *event) override;

    bool is_game_running();
    void convert_replays_at(const QString &path);

    char title_text[128] = {};

    /** The local player's display name while a Play Together session is active (else empty). */
    std::string play_together_name;

    static void on_refresh_screens(void *user_data, std::size_t screen_count, const uint32_t *const *pixels);
    static void on_change_video_mode(void *user_data, std::size_t screen_count, const SuperShuckieScreenData *screen_data, std::uint8_t scaling);

    std::uint32_t frames_in_last_second = 0;
    double current_fps = 0.0;         // emulated frames per second
    double current_display_fps = 0.0; // frames that reached the screen per second
    clock::time_point second_start;
    void refresh_title();

    void stop_timer();
    void start_timer();
    int timer_stack = 0;

    QString app_dir;

    SDLEventWrapper sdl;

private slots:
    void do_open_rom();
    void do_close_rom();
    void do_unload_rom();
    void do_screenshot();
    void do_new_game() noexcept;
    void do_load_game();
    void do_save_game();
    void do_save_new_game();
    void do_reset_console();
    void do_toggle_pause();
    void do_toggle_number_row_for_save_states();
    void do_record_replay();
    void do_resume_replay();
    void do_play_replay();
    void do_close_replay();
    void do_stop_playback();
    void do_resume_playback();
    void do_go_to_resume_point();
    void do_export_video();
    void do_convert_replay();
    void do_convert_replay_folder();
    void do_open_game_speed_dialog() noexcept;
    void do_undo_load_save_state();
    void do_redo_load_save_state();
    void do_toggle_status_bar();
    void do_toggle_sync_display();
    void present_frame();
    void do_toggle_pokeabyte();
    void do_set_pokeabyte_port();
    void do_toggle_pokeabyte_serve_friends();
    void do_toggle_stop_replay_on_input();
    void do_open_controls_settings_dialog() noexcept;
    void do_open_shortcuts_dialog();
    void do_toggle_auto_unpause_on_input();
    void do_toggle_auto_pause_on_record();
    void do_open_user_dir();
    void do_change_playback_time(int frames);
    void do_toggle_replay_keyboard_controls();
    void do_toggle_sgb();
    void do_toggle_gb_custom_colors();
    void do_open_gb_palette_dialog();
    void do_open_nds_date_dialog() noexcept;
    void do_toggle_horizontal_nds();
    void do_toggle_swap_nds_screens();
    void do_toggle_nds_jit();
    void do_clear_recent_roms();
    void do_reload_core();
    void do_toggle_external_commands();
    void do_toggle_ignore_speed_changes_in_replay();
    void do_toggle_auto_resync_keyframes_in_replay();
    void do_continue_last_replay();
    void do_toggle_disable_save_states_when_recording();
    void do_toggle_disable_speed_changes_when_recording();
    void do_toggle_audio_enabled();
    void do_toggle_audio_muted();
    void do_toggle_audio_mute_when_sped_up();
    void do_open_ram_viewer();
    void do_new_ram_viewer();
    void do_open_ram_search();
    void do_open_ram_watch();
    void do_unfreeze_all();
    void do_toggle_confirm_ram_writes();
    void do_open_tables_folder();
    void do_reload_tables();
    void do_add_bookmark();
    void do_add_keyframe_bookmark();
    void do_toggle_range_bookmark();
    void do_add_bookmark_at_frame();
    void do_open_bookmarks();
    void do_play_together_open();
    void do_play_together_leave();
    void do_play_together_reset_all();
    void do_toggle_play_together_sync_pause();
    void do_toggle_play_together_start_state();
    void do_play_together_show_windows();
    void do_play_together_unlink();
    void do_toggle_save_peer_replays();
    void do_play_together_record_everyone();
};

class NumberedAction: public QAction {
    Q_OBJECT
    friend MainWindow;
public:
    typedef void (MainWindow::*on_activated)(std::uint8_t);
    NumberedAction(MainWindow *parent, const char *text, std::uint8_t number, on_activated activated);
private:
    std::uint8_t number;
    MainWindow *parent;
    on_activated activated_fn;
private slots:
    void activated();
};

class StringAction: public QAction {
    Q_OBJECT
    friend MainWindow;
public:
    typedef void (MainWindow::*on_activated)(const char *);
    StringAction(MainWindow *parent, const char *text, const char *string, on_activated activated);
private:
    std::string string;
    MainWindow *parent;
    on_activated activated_fn;
private slots:
    void activated();
};

}

#endif
