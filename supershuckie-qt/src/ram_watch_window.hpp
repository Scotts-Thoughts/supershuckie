#ifndef __SUPERSHUCKIE_RAM_WATCH_WINDOW_HPP__
#define __SUPERSHUCKIE_RAM_WATCH_WINDOW_HPP__

#include <QJsonObject>
#include <QWidget>
#include <cstdint>
#include <map>
#include <set>
#include <vector>

class QLabel;
class QPlainTextEdit;
class QPushButton;
class QSplitter;
class QTreeWidget;
class QTreeWidgetItem;

namespace SuperShuckie64 {

class MemoryToolsController;

/** The RAM watch window: the watch list, grouped, with live values and a change log. */
class RamWatchWindow: public QWidget {
    Q_OBJECT
public:
    RamWatchWindow(MemoryToolsController *controller);

    /** Select the watch with this id (and show the window). */
    void select_watch(std::uint32_t id);

    QString save_state() const;
    void restore_state(const QString &state);

protected:
    void showEvent(QShowEvent *event) override;
    void hideEvent(QHideEvent *event) override;

private slots:
    void on_refresh();
    void on_add();
    void on_edit();
    void on_duplicate();
    void on_delete();
    void on_import();
    void on_export();
    void on_clear_log();
    void on_export_log();
    void on_item_double_clicked(QTreeWidgetItem *item, int column);
    void on_context_menu(const QPoint &position);
    void on_visible_changed();
    void on_freeze();
    void on_unfreeze();

private:
    MemoryToolsController *controller;

    QTreeWidget *tree;
    QPlainTextEdit *log;
    QLabel *status;
    QSplitter *splitter;
    QPushButton *edit_button;
    QPushButton *duplicate_button;
    QPushButton *delete_button;
    QPushButton *freeze_button;
    QPushButton *unfreeze_button;

    enum Column { Label, Region, Address, Value, Previous, Changed, Frozen, Flags, ColumnCount };

    std::uint64_t watch_generation = 0;
    std::map<std::uint32_t, QJsonObject> watches;
    std::map<std::uint32_t, QTreeWidgetItem *> items;
    std::set<QString> collapsed_groups;
    std::vector<std::uint32_t> visible_ids;

    void rebuild();
    std::vector<std::uint32_t> selected_ids() const;
    QString address_text(const QJsonObject &watch) const;
};

}

#endif
