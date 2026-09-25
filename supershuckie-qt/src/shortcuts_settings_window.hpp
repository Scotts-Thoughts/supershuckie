#ifndef __SUPERSHUCKIE_SHORTCUTS_SETTINGS_WINDOW_HPP__
#define __SUPERSHUCKIE_SHORTCUTS_SETTINGS_WINDOW_HPP__

#include <QDialog>
#include <QHash>
#include <QKeySequence>
#include <QKeySequenceEdit>
#include <QList>
#include <QString>
#include <QStringList>
#include <cstddef>
#include <optional>
#include <vector>

class QAction;
class QLabel;
class QLineEdit;
class QPushButton;
class QTreeWidget;
class QTreeWidgetItem;

namespace SuperShuckie64 {

class MainWindow;

constexpr int SHORTCUT_SLOTS = 2;

struct ShortcutBinding {
    QString id;
    QStringList path;
    QString name;
    QAction *action = nullptr;

    // Matched by the render widget during replay playback rather than fired by Qt.
    bool playback_control = false;

    QList<QKeySequence> defaults;
    std::optional<QList<QKeySequence>> custom;
    QList<QKeySequence> current;
};

// Custom shortcuts beat defaults, and no combination is given out twice: Qt fires neither action sharing one.
void resolve_shortcuts(std::vector<ShortcutBinding> &bindings);

class ShortcutSequenceEdit: public QKeySequenceEdit {
    Q_OBJECT
public:
    explicit ShortcutSequenceEdit(QWidget *parent);
signals:
    void recorded(const QKeySequence &sequence);
    void cleared();
protected:
    void keyPressEvent(QKeyEvent *event) override;
    void focusOutEvent(QFocusEvent *event) override;
private:
    void cancel_recording();
    bool recording = false;
};

class ShortcutsSettingsWindow: public QDialog {
    Q_OBJECT
    friend MainWindow;
public:
    ShortcutsSettingsWindow(MainWindow *parent, std::vector<ShortcutBinding> bindings);
    int exec() override;
    void reject() override;

    /** Select the binding with this id, ready to record its shortcut. */
    void focus_binding(const QString &id);
protected:
    bool eventFilter(QObject *object, QEvent *event) override;
private:
    MainWindow *parent;
    std::vector<ShortcutBinding> bindings;
    std::vector<std::optional<QList<QKeySequence>>> initial_custom;
    std::vector<QTreeWidgetItem *> items;
    QHash<int, QStringList> game_controls;
    std::optional<std::size_t> selected;

    QLineEdit *filter;
    QTreeWidget *tree;
    QLabel *function_label;
    ShortcutSequenceEdit *edits[SHORTCUT_SLOTS];
    QPushButton *reset_button;
    QLabel *default_label;
    QLabel *notes_label;

    void collect_game_controls();
    void build_tree();
    void refresh_items();
    void refresh_details();
    void apply_filter();
    void select_first_visible();
    std::optional<std::size_t> binding_for_item(QTreeWidgetItem *item) const;
    QString describe(std::size_t index) const;
    bool has_changes() const;
    bool can_reset(std::size_t index) const;

    void record_shortcut(int slot, const QKeySequence &sequence);
    void clear_shortcut(std::size_t index, int slot);
    void set_shortcuts(std::size_t index, QList<QKeySequence> shortcuts);
    bool take_from_others(std::size_t index, const QList<QKeySequence> &sequences);
    void reset_binding(std::size_t index);
    void reset_all();
    void show_context_menu(const QPoint &position);
};

}

#endif
