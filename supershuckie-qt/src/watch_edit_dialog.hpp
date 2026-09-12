#ifndef __SUPERSHUCKIE_WATCH_EDIT_DIALOG_HPP__
#define __SUPERSHUCKIE_WATCH_EDIT_DIALOG_HPP__

#include <QDialog>
#include <QJsonObject>
#include <cstdint>
#include <optional>

class QCheckBox;
class QComboBox;
class QLabel;
class QLineEdit;
class QSpinBox;

namespace SuperShuckie64 {

class MemoryToolsController;

/** Add or edit a RAM watch. */
class WatchEditDialog: public QDialog {
    Q_OBJECT
public:
    WatchEditDialog(MemoryToolsController *controller, QWidget *parent, const QJsonObject &watch);

    /**
     * Show the dialog for `watch` (id 0 to add). Returns the saved watch's id, or nothing if the
     * dialog was cancelled.
     */
    static std::optional<std::uint32_t> edit(MemoryToolsController *controller, QWidget *parent, const QJsonObject &watch);

    /** A new watch's JSON for the given address and value type. */
    static QJsonObject new_watch(std::uint32_t address, std::uint32_t value_type, std::uint8_t size, bool big_endian, const QString &label);

private slots:
    void on_type_changed();
    void on_region_chosen(int index);
    void on_pause_changed();
    void on_freeze_toggled();
    void accept() override;

private:
    MemoryToolsController *controller;
    QJsonObject watch;

    QLineEdit *label_edit;
    QLineEdit *address_edit;
    QComboBox *region_combo;
    QComboBox *type_combo;
    QSpinBox *size_spin;
    QComboBox *endian_combo;
    QComboBox *display_combo;
    QComboBox *table_combo;
    QComboBox *group_combo;
    QLineEdit *notes_edit;
    QCheckBox *trace_check;
    QComboBox *pause_combo;
    QLineEdit *pause_value;
    QWidget *freeze_row;
    QCheckBox *freeze_check;
    QLineEdit *freeze_value;
    QLabel *error_label;

    std::uint32_t selected_type() const;
};

}

#endif
