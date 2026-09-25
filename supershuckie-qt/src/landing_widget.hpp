#ifndef __SUPERSHUCKIE_LANDING_WIDGET_HPP__
#define __SUPERSHUCKIE_LANDING_WIDGET_HPP__

#include <QWidget>
#include <QString>
#include <QPixmap>
#include <vector>

class QGridLayout;
class QScrollArea;
class QToolButton;
class QLabel;

namespace SuperShuckie64 {

class MainWindow;

/**
 * Shown in place of the game view while no ROM is loaded.
 *
 * Holds a user-curated grid of "favorite" ROMs. Clicking a tile loads that ROM immediately; a
 * tile can be given a custom picture (copied, scaled, into `<app dir>/favorite-icons/`), renamed,
 * given a keyboard shortcut (kept with the other shortcuts; see MainWindow::rebuild_favorite_roms_menu()),
 * reordered, or removed from its context menu. Dropping a ROM anywhere on the widget loads it,
 * just as dropping one on the game view does; dropping an image on a tile sets that tile's icon.
 * Tiles can also be dragged onto one another (or past the last one) to reorder them.
 *
 * The list lives in the `qt__favorite_roms` custom setting as a JSON array of
 * `{"path": <rom path>, "name": <label>, "image": <file name in favorite-icons/ or "">}`.
 */
class LandingWidget: public QWidget {
    Q_OBJECT
    friend MainWindow;
public:
    LandingWidget(MainWindow *window, QWidget *parent);

    /** Re-read the list from the frontend's settings and rebuild the tiles. */
    void reload();

    /** The favourite ROMs' paths, in tile order. */
    QStringList favorite_paths() const;

private:
    struct Favorite {
        QString path;
        QString name;
        QString image; // file name inside the icons directory, or empty for the generated icon
    };

    MainWindow *main_window;
    std::vector<Favorite> favorites;

    QWidget *hint_row;
    QLabel *hint;
    QScrollArea *scroll_area;
    QWidget *tile_container;
    QGridLayout *tile_grid;
    std::vector<QToolButton *> tiles;
    QToolButton *add_tile = nullptr;

    static constexpr int TILE_WIDTH = 120;
    static constexpr int TILE_HEIGHT = 136;
    static constexpr int ICON_SIZE = 88;
    static constexpr int TILE_SPACING = 10;
    static constexpr int STORED_ICON_SIZE = 256;

    void load_favorites();
    void save_favorites();
    void rebuild_tiles();
    void relayout_tiles();
    int columns() const;

    QString icons_dir() const;
    QPixmap icon_for(const Favorite &favorite) const;
    static QPixmap generated_icon(const Favorite &favorite);
    static bool looks_like_image(const QString &path);

    void add_rom(const QString &path);
    void set_image(std::size_t index, const QString &source);
    void clear_image(std::size_t index);
    void remove_favorite(std::size_t index);
    void rename_favorite(std::size_t index);
    void move_favorite(std::size_t index, int delta);
    void open_favorite(std::size_t index);
    void show_tile_menu(std::size_t index, const QPoint &global_pos);

    int tile_index_at(const QPoint &pos) const;
    int drop_slot_at(const QPoint &pos) const;
    void start_tile_drag(std::size_t index);
    QPoint drag_press_pos;
    bool drag_press_active = false;

    bool eventFilter(QObject *watched, QEvent *event) override;

    void resizeEvent(QResizeEvent *event) override;
    void dragEnterEvent(QDragEnterEvent *event) override;
    void dragMoveEvent(QDragMoveEvent *event) override;
    void dropEvent(QDropEvent *event) override;

private slots:
    void do_add_rom();
    void do_dismiss_hint();
};

}

#endif
