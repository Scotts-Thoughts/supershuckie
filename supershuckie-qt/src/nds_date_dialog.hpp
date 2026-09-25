#ifndef __SUPERSHUCKIE_NDS_DATE_DIALOG_HPP__
#define __SUPERSHUCKIE_NDS_DATE_DIALOG_HPP__

#include <QDialog>
#include <vector>
#include <supershuckie/supershuckie.h>

class QLabel;
class QListWidget;
class QListWidgetItem;
class QPushButton;
class QSpinBox;

namespace SuperShuckie64 {

class MainWindow;

struct NDSDatePreset {
    QString name;
    SuperShuckieNintendoDSDate date;
};

class NDSDateDialog: public QDialog {
    Q_OBJECT
    friend MainWindow;

public:
    NDSDateDialog(MainWindow *main_window);
    int exec() override;

    /** e.g. "Mon 2026-09-21 22:00", with the seconds only when they are not zero */
    static QString describe_date(const SuperShuckieNintendoDSDate &date);
    static bool same_date(const SuperShuckieNintendoDSDate &a, const SuperShuckieNintendoDSDate &b);

    static std::vector<NDSDatePreset> load_presets(const SuperShuckieFrontendRaw *frontend);

private:
    MainWindow *main_window;

    QListWidget *presets;
    QPushButton *add_preset;
    QPushButton *update_preset;
    QPushButton *rename_preset;
    QPushButton *remove_preset;
    std::vector<NDSDatePreset> original_presets;

    QSpinBox *year;
    QSpinBox *month;
    QSpinBox *day;
    QSpinBox *hour;
    QSpinBox *minute;
    QSpinBox *second;
    QLabel *weekday;

    QPushButton *default_button;
    bool should_reload_core = false;

    SuperShuckieNintendoDSDate entered_date() const;
    void enter_date(const SuperShuckieNintendoDSDate &date);

    void set_preset(QListWidgetItem *item, const NDSDatePreset &preset);
    NDSDatePreset preset_at(const QListWidgetItem *item) const;
    std::vector<NDSDatePreset> current_presets() const;
    bool presets_changed() const;

    void accept() override;
    void reject() override;

private slots:
    void save_and_reload();
    void on_date_changed();
    void on_preset_selected();
    void on_preset_double_clicked(QListWidgetItem *item);
    void on_add_preset();
    void on_update_preset();
    void on_rename_preset();
    void on_remove_preset();
    void refresh_preset_buttons();
};

}

#endif
