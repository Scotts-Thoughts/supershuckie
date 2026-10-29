#ifndef __SUPERSHUCKIE_UPDATE_CONTROLLER_HPP__
#define __SUPERSHUCKIE_UPDATE_CONTROLLER_HPP__

#include <QDateTime>
#include <QObject>
#include <QString>
#include <QTimer>
#include <vector>

class QAction;
class QMenu;

namespace SuperShuckie64 {

class MainWindow;

/**
 * The Updates menu: checks GitHub for releases (on launch unless silenced, and on demand), offers
 * the newest, and switches to any released or locally kept version. See docs/updates.md.
 *
 * Every version that is replaced is first copied into a "versions" folder next to the exe, so
 * going back needs no download. Switching swaps the exe only after the main window has closed
 * (finish_pending_install, called from main()), then starts the new one.
 */
class UpdateController: public QObject {
    Q_OBJECT
public:
    explicit UpdateController(MainWindow *main_window);

    /** Start the launch check unless the user silenced it. */
    void check_on_launch();

    /** Called by main() once the main window is gone: swaps in the chosen exe and starts it. */
    static void finish_pending_install();

private:
    struct Release {
        QString version;     // "0.5.2" (the tag without its "v")
        QString tag;
        QString notes;
        QString page_url;
        QString exe_url;
        QString sha256;      // lowercase hex, empty if GitHub gave no digest
        qint64 exe_size = -1;
        QDateTime published;
    };

    /** One entry of "Switch to version": a release, a kept exe, or both. */
    struct Choice {
        QString version;
        bool own_build = false;  // a build not made by the release workflow
        QString local_path;      // empty when it has to be downloaded
        int release = -1;        // index into releases, -1 if not released
    };

    MainWindow *main;
    QMenu *menu;
    QAction *update_action;
    QAction *previous_action;
    QMenu *versions_menu;
    QAction *check_on_launch_action;
    QTimer prompt_timer;

    std::vector<Release> releases;
    bool releases_loaded = false;
    bool fetching = false;
    QString fetch_error;
    bool launch_prompt_pending = false;

    void set_up_menu();
    void fetch_releases(bool interactive);
    void on_releases(bool interactive, const QByteArray &body, const QString &error);
    const Release *newest_release() const;
    bool newer_than_current(const QString &version) const;

    void refresh_menu();
    void rebuild_versions_menu();
    std::vector<Choice> choices() const;

    /** The "a new version is out" dialog; `interactive` when the user asked to check. */
    void prompt(const Release &release, bool interactive);
    void try_launch_prompt();

    void switch_to(const Choice &choice);
    bool preserve_current(QString &error);
    bool download(const Release &release, const QString &dest, QString &error);

    QString setting(const char *key) const;
    void set_setting(const char *key, const QString &value);

    static QString versions_dir();
    static QString stored_exe_name(const QString &version, bool own_build);
    static void remove_leftovers();
};

}

#endif
