#ifndef __SUPERSHUCKIE_BOOKMARK_WINDOW_HPP__
#define __SUPERSHUCKIE_BOOKMARK_WINDOW_HPP__

#include <QColor>
#include <QDialog>
#include <QIcon>
#include <QJsonArray>
#include <QJsonObject>
#include <QWidget>
#include <cstdint>
#include <functional>
#include <optional>
#include <vector>

class QCheckBox;
class QComboBox;
class QLabel;
class QLineEdit;
class QListWidget;
class QPushButton;
class QSpinBox;
class QTreeWidget;
class QTreeWidgetItem;

namespace SuperShuckie64 {

class MainWindow;

/**
 * A bookmark operation from the C API: called with whether upgrading the replay file is allowed and
 * a buffer for the result (JSON or a message); returns a SuperShuckieBookmarkResult.
 */
using BookmarkOperation = std::function<std::uint32_t(bool allow_upgrade, char *out, std::size_t out_len)>;

/** "m:ss.mmm" (or "h:mm:ss.mmm"). */
QString format_bookmark_time(std::uint64_t millis);

/** The replay's bookmarks: a list with type colors; clicking a row seeks playback there. */
class BookmarkWindow: public QWidget {
    Q_OBJECT
public:
    BookmarkWindow(MainWindow *main_window);

    /**
     * Run `operation`, asking the user first if it would upgrade a pre-v5 replay and showing any
     * error. Returns the result JSON (an empty object for operations without one) on success.
     */
    static std::optional<QJsonObject> run_operation(MainWindow *main_window, QWidget *parent, const BookmarkOperation &operation);

    /** Current bookmarks and types, as supershuckie_frontend_bookmarks_json describes. */
    static QJsonObject read_state(MainWindow *main_window);

    /** Rebuild when the bookmarks changed (call from the main window's tick while visible). */
    void tick();

    QString save_state() const;
    void restore_state(const QString &state);

private slots:
    void on_item_clicked(QTreeWidgetItem *item, int column);
    void on_item_double_clicked(QTreeWidgetItem *item, int column);
    void on_item_changed(QTreeWidgetItem *item, int column);
    void on_context_menu(const QPoint &position);
    void on_type_chosen(int index);
    void on_set_out();
    void on_delete();
    void on_types();

private:
    MainWindow *main_window;

    QComboBox *type_combo;
    QTreeWidget *tree;
    QLabel *status;
    QPushButton *add_button;
    QPushButton *add_keyframe_button;
    QPushButton *range_button;
    QPushButton *set_out_button;
    QPushButton *delete_button;
    QPushButton *types_button;

    enum Column { Type, Name, In, Out, Duration, Keyframe, ColumnCount };

    std::uint64_t generation = 0;
    bool rebuilding = false;
    QJsonObject state;

    void rebuild();
    void rebuild_type_combo();
    void update_buttons();
    std::vector<std::uint64_t> selected_ids() const;
    QJsonObject bookmark(std::uint64_t id) const;
    bool update_bookmark(std::uint64_t id, const QJsonObject &patch);
    void go_to(std::uint64_t id, bool out_point);
};

/** Manage the user's bookmark types and their colors. */
class BookmarkTypesDialog: public QDialog {
    Q_OBJECT
public:
    BookmarkTypesDialog(MainWindow *main_window, QWidget *parent);
    int exec() override;

private slots:
    void on_add();
    void on_rename();
    void on_color();
    void on_delete();

private:
    MainWindow *main_window;
    QListWidget *list;
    QPushButton *rename_button;
    QPushButton *color_button;
    QPushButton *delete_button;
    QJsonArray types;

    void rebuild(const QString &select_id = QString());
    QJsonObject selected_type() const;
    std::optional<QJsonObject> upsert(const QJsonObject &type);
};

/** Add a bookmark at a chosen frame, with a name, type, out frame, or as a keyframe bookmark. */
class AddBookmarkDialog: public QDialog {
    Q_OBJECT
public:
    AddBookmarkDialog(MainWindow *main_window, QWidget *parent);
    int exec() override;

    /** The request for supershuckie_frontend_bookmark_add_json. */
    QJsonObject request() const;

private slots:
    void update_fields();

private:
    MainWindow *main_window;
    QLineEdit *name;
    QComboBox *type;
    QSpinBox *in_frame;
    QCheckBox *has_out;
    QSpinBox *out_frame;
    QCheckBox *keyframe;
    std::uint32_t current_frame = 0;
};

/** A 12x12 color swatch. */
QIcon bookmark_swatch(const QColor &color);

}

#endif
