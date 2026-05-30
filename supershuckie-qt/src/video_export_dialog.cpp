#include "video_export_dialog.hpp"
#include "main_window.hpp"

#include <QGridLayout>
#include <QLabel>
#include <QComboBox>
#include <QCheckBox>
#include <QSpinBox>
#include <QLineEdit>
#include <QPushButton>
#include <QFileDialog>

using namespace SuperShuckie64;

VideoExportDialog::VideoExportDialog(MainWindow *parent): QDialog(parent), parent(parent) {
    this->setWindowTitle("Export video from replay");

    auto replays = wrap_array_std(supershuckie_frontend_get_all_replays_for_rom(parent->frontend, nullptr));

    // Detect total frames for the loaded replay (best-effort) to size the spinboxes.
    std::uint32_t total_frames = 0;
    supershuckie_frontend_get_replay_playback_time(parent->frontend, &total_frames, nullptr);
    int max_frame = total_frames > 0 ? static_cast<int>(total_frames) : 1000000000;

    // Only show the NDS layout option when running a Nintendo DS game.
    bool is_nds = supershuckie_frontend_get_emulator_type(parent->frontend) == SuperShuckieEmulatorType::SuperShuckieEmulatorType__NintendoDS;

    QGridLayout *layout = new QGridLayout(this);
    int row = 0;

    // Replay
    layout->addWidget(new QLabel("Replay", this), row, 0, Qt::AlignLeft);
    this->replay_box = new QComboBox(this);
    for(const auto &replay : replays) {
        this->replay_box->addItem(QString::fromStdString(replay));
    }
    layout->addWidget(this->replay_box, row, 1, 1, 2);
    row++;

    // Range
    this->whole_replay_box = new QCheckBox("Export entire replay (use crop range)", this);
    this->whole_replay_box->setChecked(true);
    layout->addWidget(this->whole_replay_box, row, 0, 1, 3, Qt::AlignLeft);
    row++;

    layout->addWidget(new QLabel("Start frame", this), row, 0, Qt::AlignLeft);
    this->start_frame_box = new QSpinBox(this);
    this->start_frame_box->setMinimum(0);
    this->start_frame_box->setMaximum(max_frame);
    this->start_frame_box->setValue(0);
    layout->addWidget(this->start_frame_box, row, 1, 1, 2);
    row++;

    layout->addWidget(new QLabel("End frame", this), row, 0, Qt::AlignLeft);
    this->end_frame_box = new QSpinBox(this);
    this->end_frame_box->setMinimum(0);
    this->end_frame_box->setMaximum(max_frame);
    this->end_frame_box->setValue(max_frame);
    layout->addWidget(this->end_frame_box, row, 1, 1, 2);
    row++;

    // Preset
    layout->addWidget(new QLabel("Preset", this), row, 0, Qt::AlignLeft);
    this->preset_box = new QComboBox(this);
    this->preset_box->addItem("MP4 (H.264)");
    this->preset_box->addItem("Lossless (FFV1/MKV)");
    this->preset_box->addItem("Custom (ffmpeg args)");
    layout->addWidget(this->preset_box, row, 1, 1, 2);
    row++;

    // Custom args
    layout->addWidget(new QLabel("Custom args", this), row, 0, Qt::AlignLeft);
    this->custom_args_box = new QLineEdit(this);
    layout->addWidget(this->custom_args_box, row, 1, 1, 2);
    row++;

    // Scale
    layout->addWidget(new QLabel("Scale", this), row, 0, Qt::AlignLeft);
    this->scale_box = new QSpinBox(this);
    this->scale_box->setMinimum(1);
    this->scale_box->setMaximum(16);
    this->scale_box->setValue(1);
    this->scale_box->setSuffix("x");
    layout->addWidget(this->scale_box, row, 1, 1, 2);
    row++;

    // NDS layout
    this->layout_label = new QLabel("Layout", this);
    layout->addWidget(this->layout_label, row, 0, Qt::AlignLeft);
    this->layout_box = new QComboBox(this);
    this->layout_box->addItem("Vertical stack");
    this->layout_box->addItem("Horizontal stack");
    this->layout_box->addItem("Top only");
    this->layout_box->addItem("Bottom only");
    layout->addWidget(this->layout_box, row, 1, 1, 2);
    if(!is_nds) {
        this->layout_label->hide();
        this->layout_box->hide();
    }
    row++;

    // Output path
    layout->addWidget(new QLabel("Output", this), row, 0, Qt::AlignLeft);
    this->output_path_box = new QLineEdit(this);
    layout->addWidget(this->output_path_box, row, 1);
    this->browse_button = new QPushButton("Browse…", this);
    layout->addWidget(this->browse_button, row, 2);
    row++;

    // OK / Cancel
    this->ok_button = new QPushButton("OK", this);
    QPushButton *cancel_button = new QPushButton("Cancel", this);
    connect(this->ok_button, SIGNAL(clicked()), this, SLOT(accept()));
    connect(cancel_button, SIGNAL(clicked()), this, SLOT(reject()));
    layout->addWidget(this->ok_button, row, 1);
    layout->addWidget(cancel_button, row, 2);
    row++;

    connect(this->whole_replay_box, SIGNAL(toggled(bool)), this, SLOT(update_range_enabled()));
    connect(this->preset_box, SIGNAL(currentIndexChanged(int)), this, SLOT(update_preset_enabled()));
    connect(this->preset_box, SIGNAL(currentIndexChanged(int)), this, SLOT(update_default_output_path()));
    connect(this->replay_box, SIGNAL(currentIndexChanged(int)), this, SLOT(update_default_output_path()));
    connect(this->browse_button, SIGNAL(clicked()), this, SLOT(do_browse()));

    this->update_range_enabled();
    this->update_preset_enabled();
    this->update_default_output_path();

    this->ok_button->setEnabled(this->replay_box->count() > 0);
}

void VideoExportDialog::update_range_enabled() {
    bool whole = this->whole_replay_box->isChecked();
    this->start_frame_box->setEnabled(!whole);
    this->end_frame_box->setEnabled(!whole);
}

void VideoExportDialog::update_preset_enabled() {
    this->custom_args_box->setEnabled(this->preset_box->currentIndex() == 2);
}

void VideoExportDialog::update_default_output_path() {
    QString replay = this->replay_box->currentText();
    if(replay.isEmpty()) {
        return;
    }
    const char *ext = this->preset_box->currentIndex() == 1 ? ".mkv" : ".mp4";
    this->output_path_box->setText(replay + ext);
}

void VideoExportDialog::do_browse() {
    const char *ext = this->preset_box->currentIndex() == 1 ? "Matroska video (*.mkv)" : "MP4 video (*.mp4)";
    QString path = QFileDialog::getSaveFileName(this, "Export video to", this->output_path_box->text(), ext);
    if(!path.isEmpty()) {
        this->output_path_box->setText(path);
    }
}

void VideoExportDialog::accept() {
    if(this->replay_box->count() == 0 || this->output_path_box->text().isEmpty()) {
        return;
    }
    QDialog::accept();
}

std::string VideoExportDialog::replay_name() const {
    return this->replay_box->currentText().toStdString();
}

bool VideoExportDialog::use_range() const {
    return !this->whole_replay_box->isChecked();
}

std::uint32_t VideoExportDialog::start_frame() const {
    return static_cast<std::uint32_t>(this->start_frame_box->value());
}

std::uint32_t VideoExportDialog::end_frame() const {
    return static_cast<std::uint32_t>(this->end_frame_box->value());
}

std::uint32_t VideoExportDialog::preset() const {
    return static_cast<std::uint32_t>(this->preset_box->currentIndex());
}

std::string VideoExportDialog::custom_args() const {
    return this->custom_args_box->text().toStdString();
}

std::uint32_t VideoExportDialog::scale() const {
    return static_cast<std::uint32_t>(this->scale_box->value());
}

std::uint32_t VideoExportDialog::layout() const {
    return static_cast<std::uint32_t>(this->layout_box->currentIndex());
}

std::string VideoExportDialog::output_path() const {
    return this->output_path_box->text().toStdString();
}

int VideoExportDialog::exec() {
    this->parent->stop_timer();
    int return_value = QDialog::exec();
    this->parent->start_timer();
    return return_value;
}
