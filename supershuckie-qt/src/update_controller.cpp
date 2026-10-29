#include "update_controller.hpp"

#include <QApplication>
#include <QCheckBox>
#include <QCoreApplication>
#include <QCryptographicHash>
#include <QDesktopServices>
#include <QDir>
#include <QEventLoop>
#include <QFile>
#include <QFileInfo>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QLocale>
#include <QMenu>
#include <QMenuBar>
#include <QMessageBox>
#include <QPointer>
#include <QProcess>
#include <QProgressDialog>
#include <QPushButton>
#include <QRegularExpression>
#include <QStatusBar>
#include <QUrl>
#include <algorithm>
#include <memory>
#include <thread>

#include <supershuckie/supershuckie.h>

#include "main_window.hpp"
#include "play_together_controller.hpp"
#include "update_http.hpp"

#ifndef SUPERSHUCKIE_UPDATE_REPO
#define SUPERSHUCKIE_UPDATE_REPO "Scotts-Thoughts/supershuckie"
#endif

// Settings (custom settings in the frontend's settings file).
#define UPDATE_CHECK_ON_LAUNCH "update_check_on_launch"    // "0" = silenced
#define UPDATE_SKIPPED_VERSION "update_skipped_version"    // don't offer this version again
#define UPDATE_REMIND_AFTER "update_remind_after"          // seconds since the epoch
#define UPDATE_PREVIOUS_VERSION "update_previous_version"  // what ran before the last switch, "0.5.1" or "0.5.1-own-build"
#define UPDATE_INSTALLED_VERSION "update_installed_version" // set just before switching; the new version confirms it

namespace SuperShuckie64 {

namespace {

constexpr qint64 REMIND_LATER_SECONDS = 24 * 60 * 60;
constexpr const char *RELEASE_ASSET_NAME = "supershuckie.exe";

#ifdef SUPERSHUCKIE_RELEASE_BUILD
constexpr bool CURRENT_IS_OWN_BUILD = false;
#else
// Built by hand rather than by the release workflow: the version number may be shared with a
// different exe on GitHub, so it is kept under its own name.
constexpr bool CURRENT_IS_OWN_BUILD = true;
#endif

struct PendingInstall {
    QString source;   // the exe to install
    QString target;   // the running exe's path
    QString work_dir;
};
PendingInstall pending;

/** "v0.5.2" / "0.5.2" -> {0, 5, 2}; empty if it is not a dotted number. */
std::vector<int> version_parts(QString version) {
    if(version.startsWith('v') || version.startsWith('V')) {
        version.remove(0, 1);
    }
    static const QRegularExpression numbers("^(\\d+(?:\\.\\d+)*)");
    auto match = numbers.match(version);
    std::vector<int> parts;
    if(match.hasMatch()) {
        for(const auto &part : match.captured(1).split('.')) {
            parts.push_back(part.toInt());
        }
    }
    return parts;
}

int compare_versions(const QString &a, const QString &b) {
    auto pa = version_parts(a);
    auto pb = version_parts(b);
    for(std::size_t i = 0; i < std::max(pa.size(), pb.size()); i++) {
        int x = i < pa.size() ? pa[i] : 0;
        int y = i < pb.size() ? pb[i] : 0;
        if(x != y) {
            return x < y ? -1 : 1;
        }
    }
    return 0;
}

QString describe(const QString &version, bool own_build) {
    return own_build ? QString("%1 (your own build)").arg(version) : version;
}

QString encode_version(const QString &version, bool own_build) {
    return own_build ? version + "-own-build" : version;
}

}

UpdateController::UpdateController(MainWindow *main_window) : QObject(main_window), main(main_window) {
    this->set_up_menu();
    remove_leftovers();

    // Switching writes UPDATE_INSTALLED_VERSION before quitting; this is the version it started.
    QString installed = this->setting(UPDATE_INSTALLED_VERSION);
    if(!installed.isEmpty()) {
        this->set_setting(UPDATE_INSTALLED_VERSION, {});
        if(installed == encode_version(SUPERSHUCKIE_VERSION, CURRENT_IS_OWN_BUILD)) {
            this->main->status_bar->showMessage(QString("Now running Super Shuckie %1").arg(SUPERSHUCKIE_VERSION), 15000);
        }
    }

    this->prompt_timer.setInterval(5000);
    connect(&this->prompt_timer, &QTimer::timeout, this, &UpdateController::try_launch_prompt);
}

void UpdateController::set_up_menu() {
    this->menu = this->main->menu_bar->addMenu("Updates");

    auto *check = this->menu->addAction("Check for updates now");
    connect(check, &QAction::triggered, this, [this]() { this->fetch_releases(true); });

    this->update_action = this->menu->addAction("Update to the newest version");
    connect(this->update_action, &QAction::triggered, this, [this]() {
        if(const auto *newest = this->newest_release()) {
            this->prompt(*newest, true);
        }
    });

    this->previous_action = this->menu->addAction("Go back to the previous version");
    connect(this->previous_action, &QAction::triggered, this, [this]() {
        QString previous = this->setting(UPDATE_PREVIOUS_VERSION);
        for(const auto &choice : this->choices()) {
            if(encode_version(choice.version, choice.own_build) == previous && !choice.local_path.isEmpty()) {
                this->switch_to(choice);
                return;
            }
        }
    });

    this->versions_menu = this->menu->addMenu("Roll back or switch to version");
    connect(this->versions_menu, &QMenu::aboutToShow, this, [this]() {
        if(!this->releases_loaded && !this->fetching) {
            this->fetch_releases(false);
        }
        this->rebuild_versions_menu();
    });

    this->menu->addSeparator();

    this->check_on_launch_action = this->menu->addAction("Check for updates when Super Shuckie starts");
    this->check_on_launch_action->setCheckable(true);
    this->check_on_launch_action->setChecked(this->setting(UPDATE_CHECK_ON_LAUNCH) != "0");
    this->check_on_launch_action->setToolTip("Turn off to never be asked about updates; \"Check for updates now\" still works");
    connect(this->check_on_launch_action, &QAction::toggled, this, [this](bool checked) {
        this->set_setting(UPDATE_CHECK_ON_LAUNCH, checked ? QString() : QString("0"));
    });

    auto *open_folder = this->menu->addAction("Open the kept versions folder");
    connect(open_folder, &QAction::triggered, this, []() {
        QDir().mkpath(versions_dir());
        QDesktopServices::openUrl(QUrl::fromLocalFile(versions_dir()));
    });

    auto *releases_page = this->menu->addAction("Release notes on GitHub");
    connect(releases_page, &QAction::triggered, this, []() {
        QDesktopServices::openUrl(QUrl("https://github.com/" SUPERSHUCKIE_UPDATE_REPO "/releases"));
    });

    this->refresh_menu();
}

void UpdateController::check_on_launch() {
    if(this->setting(UPDATE_CHECK_ON_LAUNCH) == "0") {
        return;
    }
    this->launch_prompt_pending = true;
    this->fetch_releases(false);
}

void UpdateController::fetch_releases(bool interactive) {
    if(this->fetching) {
        return;
    }
    this->fetching = true;

    QPointer<UpdateController> self(this);
    std::thread([self, interactive]() {
        QByteArray body;
        QString error = http_get("https://api.github.com/repos/" SUPERSHUCKIE_UPDATE_REPO "/releases?per_page=100", "application/vnd.github+json", body);
        QMetaObject::invokeMethod(qApp, [self, interactive, body, error]() {
            if(self) {
                self->on_releases(interactive, body, error);
            }
        }, Qt::QueuedConnection);
    }).detach();

    if(interactive) {
        this->main->status_bar->showMessage("Checking GitHub for updates…", 5000);
    }
}

void UpdateController::on_releases(bool interactive, const QByteArray &body, const QString &error) {
    this->fetching = false;
    this->fetch_error = error;

    if(error.isEmpty()) {
        QJsonParseError parse_error;
        auto document = QJsonDocument::fromJson(body, &parse_error);
        if(!document.isArray()) {
            this->fetch_error = QString("GitHub's answer could not be read (%1)").arg(parse_error.errorString());
        }
        else {
            static const QRegularExpression release_tag("^[vV]?\\d+\\.\\d+\\.\\d+$");
            std::vector<Release> found;
            for(const auto &entry : document.array()) {
                auto object = entry.toObject();
                QString tag = object["tag_name"].toString();
                if(object["draft"].toBool() || object["prerelease"].toBool() || !release_tag.match(tag).hasMatch()) {
                    continue;
                }
                Release release;
                release.tag = tag;
                release.version = tag.startsWith('v') || tag.startsWith('V') ? tag.mid(1) : tag;
                release.notes = object["body"].toString().trimmed();
                release.page_url = object["html_url"].toString();
                release.published = QDateTime::fromString(object["published_at"].toString(), Qt::ISODate);
                for(const auto &asset_value : object["assets"].toArray()) {
                    auto asset = asset_value.toObject();
                    if(asset["name"].toString().compare(RELEASE_ASSET_NAME, Qt::CaseInsensitive) != 0) {
                        continue;
                    }
                    release.exe_url = asset["browser_download_url"].toString();
                    release.exe_size = asset["size"].toInteger(-1);
                    QString digest = asset["digest"].toString();
                    if(digest.startsWith("sha256:")) {
                        release.sha256 = digest.mid(7).toLower();
                    }
                }
                // Releases without the exe (made by hand, or a failed upload) can't be installed.
                if(!release.exe_url.isEmpty()) {
                    found.push_back(std::move(release));
                }
            }
            std::sort(found.begin(), found.end(), [](const Release &a, const Release &b) {
                return compare_versions(a.version, b.version) > 0;
            });
            this->releases = std::move(found);
            this->releases_loaded = true;
        }
    }

    this->refresh_menu();
    if(this->versions_menu->isVisible()) {
        this->rebuild_versions_menu();
    }

    if(interactive) {
        if(!this->fetch_error.isEmpty()) {
            this->main->show_error("Could not check for updates", "%s", this->fetch_error.toUtf8().constData());
            return;
        }
        const auto *newest = this->newest_release();
        if(newest != nullptr) {
            this->prompt(*newest, true);
        }
        else {
            this->main->stop_timer();
            QMessageBox::information(this->main, "No update available",
                QString("You have the newest version of Super Shuckie (%1).").arg(SUPERSHUCKIE_VERSION));
            this->main->start_timer();
        }
        return;
    }

    if(this->launch_prompt_pending && this->fetch_error.isEmpty()) {
        this->try_launch_prompt();
    }
}

const UpdateController::Release *UpdateController::newest_release() const {
    if(this->releases.empty() || !this->newer_than_current(this->releases.front().version)) {
        return nullptr;
    }
    return &this->releases.front();
}

bool UpdateController::newer_than_current(const QString &version) const {
    return compare_versions(version, SUPERSHUCKIE_VERSION) > 0;
}

void UpdateController::try_launch_prompt() {
    const auto *newest = this->newest_release();
    if(newest == nullptr || !this->launch_prompt_pending || this->setting(UPDATE_CHECK_ON_LAUNCH) == "0") {
        this->launch_prompt_pending = false;
        this->prompt_timer.stop();
        return;
    }
    if(newest->version == this->setting(UPDATE_SKIPPED_VERSION)) {
        this->launch_prompt_pending = false;
        return;
    }
    bool ok = false;
    qint64 remind_after = this->setting(UPDATE_REMIND_AFTER).toLongLong(&ok);
    if(ok && QDateTime::currentSecsSinceEpoch() < remind_after) {
        this->launch_prompt_pending = false;
        return;
    }
    // Never interrupt a game: wait until no ROM is running (the menu shows the update meanwhile).
    if(supershuckie_frontend_is_game_running(this->main->frontend) || QApplication::activeModalWidget() != nullptr) {
        if(!this->prompt_timer.isActive()) {
            this->prompt_timer.start();
        }
        return;
    }
    this->prompt_timer.stop();
    this->launch_prompt_pending = false;
    this->prompt(*newest, false);
}

void UpdateController::prompt(const Release &release, bool interactive) {
    QMessageBox box(this->main);
    box.setWindowTitle("Update available");
    box.setIcon(QMessageBox::Information);
    QString text = QString("Super Shuckie %1 is available. You have %2.").arg(release.version, SUPERSHUCKIE_VERSION);
    if(release.published.isValid()) {
        text += QString("\n\nReleased %1.").arg(QLocale().toString(release.published.toLocalTime().date(), QLocale::LongFormat));
    }
    text += "\n\nYour current version is kept, so you can go back to it from the Updates menu.";
    box.setText(text);
    if(!release.notes.isEmpty()) {
        box.setDetailedText(release.notes);
    }
    auto *update = box.addButton("Update now", QMessageBox::AcceptRole);
    auto *later = box.addButton("Remind me later", QMessageBox::RejectRole);
    auto *skip = box.addButton("Skip this version", QMessageBox::DestructiveRole);
    box.setDefaultButton(update);
    box.setEscapeButton(later);

    QCheckBox silence("Don't check for updates when Super Shuckie starts");
    silence.setChecked(this->setting(UPDATE_CHECK_ON_LAUNCH) == "0");
    box.setCheckBox(&silence);

    this->main->stop_timer();
    box.exec();
    this->main->start_timer();

    // The checkbox mirrors the menu item; toggling the action stores the setting.
    this->check_on_launch_action->setChecked(!silence.isChecked());

    if(box.clickedButton() == update) {
        for(const auto &choice : this->choices()) {
            if(!choice.own_build && choice.version == release.version) {
                this->switch_to(choice);
                return;
            }
        }
    }
    else if(box.clickedButton() == skip) {
        this->set_setting(UPDATE_SKIPPED_VERSION, release.version);
        this->refresh_menu();
    }
    else if(!interactive) {
        this->set_setting(UPDATE_REMIND_AFTER, QString::number(QDateTime::currentSecsSinceEpoch() + REMIND_LATER_SECONDS));
    }
}

void UpdateController::refresh_menu() {
    const auto *newest = this->newest_release();
    if(newest != nullptr) {
        this->update_action->setText(QString("Update to %1…").arg(newest->version));
        this->update_action->setEnabled(true);
    }
    else {
        this->update_action->setText("Update to the newest version");
        this->update_action->setEnabled(false);
    }
    // A skipped version is still offered in the menu, just not announced in the title.
    bool announce = newest != nullptr && newest->version != this->setting(UPDATE_SKIPPED_VERSION);
    this->menu->setTitle(announce ? "Updates (new!)" : "Updates");

    QString previous = this->setting(UPDATE_PREVIOUS_VERSION);
    bool have_previous = false;
    if(!previous.isEmpty() && previous != encode_version(SUPERSHUCKIE_VERSION, CURRENT_IS_OWN_BUILD)) {
        for(const auto &choice : this->choices()) {
            if(encode_version(choice.version, choice.own_build) == previous && !choice.local_path.isEmpty()) {
                have_previous = true;
                this->previous_action->setText(QString("Go back to the previous version (%1)…").arg(describe(choice.version, choice.own_build)));
                break;
            }
        }
    }
    this->previous_action->setVisible(have_previous);
}

std::vector<UpdateController::Choice> UpdateController::choices() const {
    std::vector<Choice> list;
    for(int i = 0; i < static_cast<int>(this->releases.size()); i++) {
        Choice choice;
        choice.version = this->releases[i].version;
        choice.release = i;
        list.push_back(choice);
    }

    static const QRegularExpression stored("^supershuckie-(\\d+(?:\\.\\d+)*)(-own-build)?\\.exe$", QRegularExpression::CaseInsensitiveOption);
    QDir dir(versions_dir());
    for(const auto &info : dir.entryInfoList({"supershuckie-*.exe"}, QDir::Files)) {
        auto match = stored.match(info.fileName());
        if(!match.hasMatch()) {
            continue;
        }
        QString version = match.captured(1);
        bool own_build = !match.captured(2).isEmpty();
        auto existing = std::find_if(list.begin(), list.end(), [&](const Choice &c) {
            return c.version == version && c.own_build == own_build;
        });
        if(existing != list.end()) {
            existing->local_path = info.absoluteFilePath();
        }
        else {
            Choice choice;
            choice.version = version;
            choice.own_build = own_build;
            choice.local_path = info.absoluteFilePath();
            list.push_back(choice);
        }
    }

    std::sort(list.begin(), list.end(), [](const Choice &a, const Choice &b) {
        int order = compare_versions(a.version, b.version);
        return order != 0 ? order > 0 : (!a.own_build && b.own_build);
    });
    return list;
}

void UpdateController::rebuild_versions_menu() {
    this->versions_menu->clear();

    auto add_note = [this](const QString &text) {
        auto *note = this->versions_menu->addAction(text);
        note->setEnabled(false);
    };

    if(this->fetching) {
        add_note("Loading versions from GitHub…");
    }
    else if(!this->fetch_error.isEmpty()) {
        add_note("GitHub could not be reached; showing kept versions only");
    }

    auto list = this->choices();
    QString current = encode_version(SUPERSHUCKIE_VERSION, CURRENT_IS_OWN_BUILD);
    bool current_listed = false;
    for(const auto &choice : list) {
        QString label = describe(choice.version, choice.own_build);
        QStringList notes;
        bool is_current = encode_version(choice.version, choice.own_build) == current;
        if(is_current) {
            notes << "current";
            current_listed = true;
        }
        else if(choice.release == 0 && this->newer_than_current(choice.version)) {
            notes << "newest";
        }
        if(!choice.local_path.isEmpty() && !is_current) {
            notes << "kept on this computer";
        }
        if(choice.release >= 0 && this->releases[choice.release].published.isValid()) {
            notes << QLocale().toString(this->releases[choice.release].published.toLocalTime().date(), QLocale::ShortFormat);
        }
        if(!notes.isEmpty()) {
            label += " — " + notes.join(", ");
        }
        auto *action = this->versions_menu->addAction(label);
        action->setCheckable(true);
        action->setChecked(is_current);
        action->setEnabled(!is_current);
        connect(action, &QAction::triggered, this, [this, choice]() { this->switch_to(choice); });
    }
    if(!current_listed) {
        auto *action = this->versions_menu->addAction(QString("%1 — current").arg(describe(SUPERSHUCKIE_VERSION, CURRENT_IS_OWN_BUILD)));
        action->setCheckable(true);
        action->setChecked(true);
        action->setEnabled(false);
    }
}

void UpdateController::switch_to(const Choice &choice) {
    QString target = describe(choice.version, choice.own_build);
    int order = compare_versions(choice.version, SUPERSHUCKIE_VERSION);

    QString text = QString("Switch from Super Shuckie %1 to %2?\n\nSuper Shuckie will close and start again as %2. "
                           "Your settings, saves and replays are not touched, and %1 is kept so you can switch back from the Updates menu.")
        .arg(describe(SUPERSHUCKIE_VERSION, CURRENT_IS_OWN_BUILD), target);
    if(order < 0) {
        text += "\n\nOlder versions may not open replays or save states made by newer ones.";
    }
    if(choice.local_path.isEmpty() && choice.release >= 0) {
        text += QString("\n\n%1 will be downloaded from GitHub (about %2 MB).")
            .arg(choice.version)
            .arg(std::max<qint64>(1, this->releases[choice.release].exe_size / (1024 * 1024)));
    }
    if(supershuckie_frontend_get_replay_state(this->main->frontend) == SuperShuckieReplayState__Recording) {
        text += "\n\nThe replay being recorded will be stopped and saved.";
    }

    this->main->stop_timer();
    auto answer = QMessageBox::question(this->main, "Switch version", text, QMessageBox::Yes | QMessageBox::Cancel, QMessageBox::Yes);
    this->main->start_timer();
    if(answer != QMessageBox::Yes) {
        return;
    }
    if(this->main->play_together != nullptr && !this->main->play_together->confirm_leave("Switching version")) {
        return;
    }

    QString error;
    if(!this->preserve_current(error)) {
        this->main->show_error("Could not switch version", "The current version could not be kept, so nothing was changed.\n\n%s", error.toUtf8().constData());
        return;
    }

    QString source = choice.local_path;
    if(source.isEmpty()) {
        if(choice.release < 0) {
            return;
        }
        source = QDir(versions_dir()).filePath(stored_exe_name(choice.version, false));
        if(!this->download(this->releases[choice.release], source, error)) {
            if(!error.isEmpty()) {
                this->main->show_error("Download failed", "%s", error.toUtf8().constData());
            }
            return;
        }
    }

    pending.source = source;
    pending.target = QCoreApplication::applicationFilePath();
    pending.work_dir = QDir::currentPath();
    this->set_setting(UPDATE_PREVIOUS_VERSION, encode_version(SUPERSHUCKIE_VERSION, CURRENT_IS_OWN_BUILD));
    this->set_setting(UPDATE_INSTALLED_VERSION, encode_version(choice.version, choice.own_build));
    if(choice.version == this->setting(UPDATE_SKIPPED_VERSION)) {
        this->set_setting(UPDATE_SKIPPED_VERSION, {});
    }

    // closeEvent saves settings, SRAM and any replay being recorded; quit() also closes the
    // other windows, which would otherwise keep the application running.
    this->main->close();
    QCoreApplication::quit();
}

bool UpdateController::preserve_current(QString &error) {
    QString dir = versions_dir();
    if(!QDir().mkpath(dir)) {
        error = QString("Could not create %1. Is the folder Super Shuckie is in read-only?").arg(QDir::toNativeSeparators(dir));
        return false;
    }
    QString dest = QDir(dir).filePath(stored_exe_name(SUPERSHUCKIE_VERSION, CURRENT_IS_OWN_BUILD));
    if(QFileInfo(dest).size() == QFileInfo(QCoreApplication::applicationFilePath()).size()) {
        return true;
    }
    QString part = dest + ".part";
    QFile::remove(part);
    if(!QFile::copy(QCoreApplication::applicationFilePath(), part)) {
        error = QString("Could not copy the running exe to %1.").arg(QDir::toNativeSeparators(part));
        return false;
    }
    QFile::remove(dest);
    if(!QFile::rename(part, dest)) {
        error = QString("Could not create %1.").arg(QDir::toNativeSeparators(dest));
        return false;
    }
    return true;
}

bool UpdateController::download(const Release &release, const QString &dest, QString &error) {
    struct State {
        HttpProgress progress;
        QString error;
        bool done = false;
    };
    auto state = std::make_shared<State>();
    QString part = dest + ".part";

    QProgressDialog dialog(QString("Downloading Super Shuckie %1…").arg(release.version), "Cancel", 0, 1000, this->main);
    dialog.setWindowTitle("Downloading update");
    dialog.setWindowModality(Qt::WindowModal);
    dialog.setMinimumDuration(0);
    dialog.setAutoClose(false);
    dialog.setAutoReset(false);
    dialog.setValue(0);

    QEventLoop loop;
    QTimer poll;
    poll.setInterval(100);
    connect(&poll, &QTimer::timeout, &dialog, [&]() {
        qint64 total = state->progress.total;
        if(total <= 0) {
            total = release.exe_size;
        }
        qint64 received = state->progress.received;
        if(total > 0) {
            dialog.setValue(static_cast<int>(std::min<qint64>(1000, received * 1000 / total)));
            dialog.setLabelText(QString("Downloading Super Shuckie %1… %2 of %3 MB")
                .arg(release.version)
                .arg(received / (1024.0 * 1024.0), 0, 'f', 1)
                .arg(total / (1024.0 * 1024.0), 0, 'f', 1));
        }
        if(state->done) {
            loop.quit();
        }
    });
    connect(&dialog, &QProgressDialog::canceled, &dialog, [&]() {
        state->progress.cancel = true;
        dialog.setLabelText("Cancelling…");
    });

    QString url = release.exe_url;
    QString expected_hash = release.sha256;
    qint64 expected_size = release.exe_size;
    std::thread worker([state, url, part, expected_hash, expected_size]() {
        QFile file(part);
        if(!file.open(QIODevice::WriteOnly | QIODevice::Truncate)) {
            state->error = QString("Could not create %1: %2").arg(QDir::toNativeSeparators(part), file.errorString());
        }
        else {
            state->error = http_get(url, "application/octet-stream", &file, &state->progress);
            file.close();
            if(state->error.isEmpty() && expected_size >= 0 && file.size() != expected_size) {
                state->error = QString("The download is incomplete (%1 of %2 bytes).").arg(file.size()).arg(expected_size);
            }
            if(state->error.isEmpty() && !expected_hash.isEmpty() && file.open(QIODevice::ReadOnly)) {
                QCryptographicHash hash(QCryptographicHash::Sha256);
                hash.addData(&file);
                file.close();
                if(QString::fromLatin1(hash.result().toHex()) != expected_hash) {
                    state->error = "The download is damaged (its SHA-256 does not match the one GitHub lists).";
                }
            }
            if(!state->error.isEmpty()) {
                QFile::remove(part);
            }
        }
        state->done = true;
    });

    this->main->stop_timer();
    poll.start();
    loop.exec();
    worker.join();
    this->main->start_timer();

    if(state->progress.cancel) {
        QFile::remove(part);
        error.clear();
        return false;
    }
    if(!state->error.isEmpty()) {
        error = state->error;
        return false;
    }
    QFile::remove(dest);
    if(!QFile::rename(part, dest)) {
        error = QString("Could not create %1.").arg(QDir::toNativeSeparators(dest));
        return false;
    }
    return true;
}

void UpdateController::finish_pending_install() {
    if(pending.source.isEmpty()) {
        return;
    }
    QString target = pending.target;
    auto fail = [](const QString &message) {
        QMessageBox::critical(nullptr, "Could not switch version", message);
    };

    // A running exe can be renamed but not overwritten; the next start deletes the old name.
    QString old = target + ".old";
    QFile::remove(old);
    if(QFile::exists(old)) {
        old = QString("%1.%2.old").arg(target).arg(QDateTime::currentMSecsSinceEpoch());
    }
    if(!QFile::rename(target, old)) {
        fail(QString("%1 could not be renamed, so it was left as it is. Is the folder read-only?").arg(QDir::toNativeSeparators(target)));
        return;
    }
    if(!QFile::copy(pending.source, target)) {
        QFile::rename(old, target);
        fail(QString("%1 could not be copied over %2, so it was left as it is.")
            .arg(QDir::toNativeSeparators(pending.source), QDir::toNativeSeparators(target)));
        return;
    }
    if(!QProcess::startDetached(target, {}, pending.work_dir)) {
        fail(QString("The new version was installed but could not be started. Start %1 yourself.").arg(QDir::toNativeSeparators(target)));
    }
}

void UpdateController::remove_leftovers() {
    // The previous process may still be exiting; try again a little later too.
    auto remove = []() {
        QFileInfo exe(QCoreApplication::applicationFilePath());
        QDir dir = exe.absoluteDir();
        for(const auto &name : dir.entryList({exe.fileName() + ".old", exe.fileName() + ".*.old"}, QDir::Files)) {
            dir.remove(name);
        }
    };
    remove();
    QTimer::singleShot(10000, qApp, remove);
}

QString UpdateController::versions_dir() {
    return QFileInfo(QCoreApplication::applicationFilePath()).absoluteDir().filePath("versions");
}

QString UpdateController::stored_exe_name(const QString &version, bool own_build) {
    return QString("supershuckie-%1.exe").arg(encode_version(version, own_build));
}

QString UpdateController::setting(const char *key) const {
    // Copied out at once: the next frontend call may invalidate the pointer.
    const char *value = supershuckie_frontend_get_custom_setting(this->main->frontend, key);
    return value != nullptr ? QString::fromUtf8(value) : QString();
}

void UpdateController::set_setting(const char *key, const QString &value) {
    supershuckie_frontend_set_custom_setting(this->main->frontend, key, value.isEmpty() ? nullptr : value.toUtf8().constData());
}

}
