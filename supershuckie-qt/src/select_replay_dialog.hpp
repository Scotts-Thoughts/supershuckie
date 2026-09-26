#ifndef __SUPERSHUCKIE_SELECT_REPLAY_DIALOG_HPP__
#define __SUPERSHUCKIE_SELECT_REPLAY_DIALOG_HPP__

#include <QDialog>
#include <optional>
#include <string>
#include <vector>

class QString;
class QTreeWidget;

namespace SuperShuckie64 {

class MainWindow;

// Replay picker: a table of the current ROM's replays with when each was filmed and its size.
// Clicking a column header sorts by it; the choice is remembered for the rest of the session.
class SelectReplayDialog: public QDialog {
    Q_OBJECT
public:
    SelectReplayDialog(MainWindow *parent, const std::vector<std::string> &replays, const QString &title, const QString &message);
    QString text() const;
    int exec() override;
    static std::optional<std::string> ask(MainWindow *parent, const QString &title, const QString &message);
private:
    QTreeWidget *tree = nullptr;
    MainWindow *parent;
};

}

#endif
