#include "select_replay_dialog.hpp"
#include "main_window.hpp"

#include <QDateTime>
#include <QDir>
#include <QFileInfo>
#include <QGridLayout>
#include <QHeaderView>
#include <QLabel>
#include <QLocale>
#include <QPushButton>
#include <QTreeWidget>

using namespace SuperShuckie64;

enum ReplayColumn {
    Name,
    Filmed,
    Size,
    ColumnCount
};

// Each column sorts on the raw value in Qt::UserRole rather than the displayed text. Name sorts
// on the frontend's listing order, which already compares trailing numbers numerically.
class ReplayItem: public QTreeWidgetItem {
public:
    using QTreeWidgetItem::QTreeWidgetItem;
    bool operator<(const QTreeWidgetItem &other) const override {
        int column = this->treeWidget() != nullptr ? this->treeWidget()->sortColumn() : Name;
        return this->data(column, Qt::UserRole).toLongLong() < other.data(column, Qt::UserRole).toLongLong();
    }
};

static int last_sort_column = Name;
static Qt::SortOrder last_sort_order = Qt::AscendingOrder;

SelectReplayDialog::SelectReplayDialog(MainWindow *parent, const std::vector<std::string> &replays, const QString &title, const QString &message): QDialog(parent), parent(parent) {
    this->setWindowTitle(title);

    auto *layout = new QGridLayout(this);

    QLabel *message_text = new QLabel(message, this);
    message_text->setAlignment(Qt::AlignHCenter);
    layout->addWidget(message_text, 0, 0);

    QString dir;
    char dir_buffer[4096];
    if(parent->frontend != nullptr && supershuckie_frontend_get_replays_dir_for_current_rom(parent->frontend, dir_buffer, sizeof(dir_buffer))) {
        dir = QString::fromUtf8(dir_buffer);
    }

    this->tree = new QTreeWidget(this);
    this->tree->setColumnCount(ColumnCount);
    this->tree->setHeaderLabels({ "Name", "Filmed", "Size" });
    this->tree->headerItem()->setTextAlignment(Size, Qt::AlignRight | Qt::AlignVCenter);
    this->tree->setRootIsDecorated(false);
    this->tree->setUniformRowHeights(true);
    this->tree->setAllColumnsShowFocus(true);
    this->tree->setEditTriggers(QAbstractItemView::NoEditTriggers);
    this->tree->header()->setStretchLastSection(false);
    this->tree->header()->setSectionResizeMode(Name, QHeaderView::Stretch);
    this->tree->header()->setSectionResizeMode(Filmed, QHeaderView::ResizeToContents);
    this->tree->header()->setSectionResizeMode(Size, QHeaderView::ResizeToContents);

    QLocale locale;
    for(std::size_t i = 0; i < replays.size(); i++) {
        auto name = QString::fromStdString(replays[i]);
        auto *item = new ReplayItem(this->tree);
        item->setText(Name, name);
        item->setData(Name, Qt::UserRole, static_cast<qlonglong>(i));

        // The replay file is created when recording starts and later edits (bookmarks) write it in
        // place, so its creation time is when it was filmed. A copied file gets a new creation time
        // but keeps its modification time (the end of recording), so take whichever is earlier.
        QFileInfo info(QDir(dir).filePath(name + ".replay"));
        if(!dir.isEmpty() && info.exists()) {
            QDateTime filmed = info.lastModified();
            QDateTime created = info.birthTime();
            if(created.isValid() && created < filmed) {
                filmed = created;
            }
            item->setText(Filmed, locale.toString(filmed, QLocale::ShortFormat));
            item->setData(Filmed, Qt::UserRole, filmed.toMSecsSinceEpoch());
            item->setText(Size, locale.formattedDataSize(info.size()));
            item->setData(Size, Qt::UserRole, static_cast<qlonglong>(info.size()));
        }
        else {
            item->setData(Filmed, Qt::UserRole, static_cast<qlonglong>(-1));
            item->setData(Size, Qt::UserRole, static_cast<qlonglong>(-1));
        }
        item->setTextAlignment(Size, Qt::AlignRight | Qt::AlignVCenter);
    }

    this->tree->setSortingEnabled(true);
    this->tree->sortByColumn(last_sort_column, last_sort_order);
    connect(this->tree->header(), &QHeaderView::sortIndicatorChanged, this, [](int column, Qt::SortOrder order) {
        last_sort_column = column;
        last_sort_order = order;
    });

    connect(this->tree, &QTreeWidget::itemActivated, this, &QDialog::accept);

    layout->addWidget(this->tree, 5, 0);

    auto *save = new QPushButton("OK", this);
    connect(save, SIGNAL(clicked()), this, SLOT(accept()));
    layout->addWidget(save, 9999, 0);

    this->resize(560, 380);
}

QString SelectReplayDialog::text() const {
    auto *item = this->tree->currentItem();
    if(item == nullptr) {
        return "";
    }
    return item->text(Name);
}

std::optional<std::string> SelectReplayDialog::ask(MainWindow *parent, const QString &title, const QString &message) {
    auto replays = wrap_array_std(supershuckie_frontend_get_all_replays_for_rom(parent->frontend, nullptr));
    auto *dialog = new SelectReplayDialog(parent, replays, title, message);
    int exec_result = dialog->exec();
    auto text = dialog->text().toStdString();
    delete dialog;

    if(exec_result != QDialog::Accepted || text.empty()) {
        return std::nullopt;
    }

    return text;
}

int SelectReplayDialog::exec() {
    this->parent->stop_timer();
    int return_value = QDialog::exec();
    this->parent->start_timer();
    return return_value;
}
