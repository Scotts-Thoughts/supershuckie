#ifndef __SUPERSHUCKIE_VIDEO_EXPORT_DIALOG_HPP__
#define __SUPERSHUCKIE_VIDEO_EXPORT_DIALOG_HPP__

#include <QDialog>
#include <cstdint>
#include <string>
#include <vector>

class QComboBox;
class QCheckBox;
class QSpinBox;
class QLineEdit;
class QPushButton;
class QLabel;

namespace SuperShuckie64 {

class MainWindow;

class VideoExportDialog: public QDialog {
    Q_OBJECT
    friend MainWindow;

public:
    VideoExportDialog(MainWindow *parent);
    int exec() override;

    std::string replay_name() const;
    bool use_range() const;
    std::uint32_t start_frame() const;
    std::uint32_t end_frame() const;
    std::uint32_t preset() const;
    std::string custom_args() const;
    std::uint32_t scale() const;
    std::uint32_t layout() const;
    std::string output_path() const;

private:
    MainWindow *parent;

    QComboBox *replay_box;
    QCheckBox *whole_replay_box;
    QSpinBox *start_frame_box;
    QSpinBox *end_frame_box;
    QComboBox *preset_box;
    QLineEdit *custom_args_box;
    QSpinBox *scale_box;
    QComboBox *layout_box;
    QLineEdit *output_path_box;
    QPushButton *browse_button;
    QLabel *layout_label;
    QPushButton *ok_button;

    void accept() override;

private slots:
    void update_range_enabled();
    void update_preset_enabled();
    void update_default_output_path();
    void do_browse();
};

}

#endif
