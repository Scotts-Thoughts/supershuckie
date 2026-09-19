#ifndef __SUPERSHUCKIE_PLAY_TOGETHER_DIALOG_HPP__
#define __SUPERSHUCKIE_PLAY_TOGETHER_DIALOG_HPP__

#include <QDialog>
#include <QJsonObject>

class QLabel;
class QLineEdit;
class QPushButton;
class QSpinBox;
class QTabWidget;
class QTableWidget;
class QCheckBox;
class QComboBox;

namespace SuperShuckie64 {

class PlayTogetherController;

/**
 * Host or join a Play Together session, and see who is in it. Non-modal, so the other players'
 * windows keep updating while it is open.
 */
class PlayTogetherDialog: public QDialog {
    Q_OBJECT
public:
    explicit PlayTogetherDialog(PlayTogetherController *controller);

    /** Show the given session state (see play_together.h). */
    void refresh(const QJsonObject &state);

private slots:
    void do_host();
    void do_join();
    void do_leave();
    void do_reset_all();
    void do_copy_code();
    void do_toggle_save_replays(bool on);

private:
    PlayTogetherController *controller;

    QTabWidget *tabs;

    QLineEdit *host_name;
    QComboBox *host_color;
    QSpinBox *host_port;
    QPushButton *host_button;
    QLabel *code_label;
    QPushButton *copy_button;
    QLabel *addresses_label;

    QLineEdit *join_name;
    QComboBox *join_color;
    QLineEdit *join_code;
    QPushButton *join_button;

    QLabel *session_label;
    QTableWidget *participants;
    QPushButton *reset_button;
    QPushButton *leave_button;
    QCheckBox *save_replays;
    QLabel *errors_label;
    QString shown_code;

    /** "Random" plus every palette colour, with the saved one selected. */
    QComboBox *make_color_combo(QWidget *parent);
};

}

#endif
