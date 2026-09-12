#include <QPainter>
#include <QScrollBar>
#include <QMouseEvent>
#include <QKeyEvent>
#include <QFontDatabase>
#include <QContextMenuEvent>
#include <algorithm>

#include "hex_view_widget.hpp"

using namespace SuperShuckie64;

// Changed bytes fade out over this long.
static constexpr int HEAT_FADE_MS = 1000;

// Samples an edit stays outlined for while waiting for one that shows it.
static constexpr std::uint32_t PENDING_SAMPLES = 3;

HexViewWidget::HexViewWidget(QWidget *parent): QAbstractScrollArea(parent) {
    this->font = QFontDatabase::systemFont(QFontDatabase::FixedFont);
    this->viewport()->setFont(this->font);
    QFontMetrics metrics(this->font);
    this->char_width = std::max(1, metrics.horizontalAdvance(QLatin1Char('0')));
    this->line_height = metrics.height() + 2;

    this->setHorizontalScrollBarPolicy(Qt::ScrollBarAsNeeded);
    this->setVerticalScrollBarPolicy(Qt::ScrollBarAlwaysOn);
    this->setFocusPolicy(Qt::StrongFocus);
    this->viewport()->setCursor(Qt::IBeamCursor);

    for(auto &glyph : this->glyphs) {
        glyph.clear();
    }
}

void HexViewWidget::set_region(std::uint32_t base, std::uint32_t length, bool writable, int address_digits) {
    this->base = base;
    this->region_length = length;
    this->writable = writable;
    this->address_digits = address_digits;
    this->data.clear();
    this->heat.clear();
    this->data_valid = 0;
    this->pending.clear();
    this->cursor = base;
    this->anchor = base;
    this->nibble = 0;
    this->last_window = {0, 0};
    this->update_scroll_range();
    this->verticalScrollBar()->setValue(0);
    this->emit_window_if_changed();
    this->viewport()->update();
    this->updateGeometry();
    emit this->cursor_changed(this->cursor);
}

void HexViewWidget::clear_region() {
    this->region_length = 0;
    this->data.clear();
    this->heat.clear();
    this->data_valid = 0;
    this->last_window = {0, 0};
    this->update_scroll_range();
    this->viewport()->update();
}

std::int64_t HexViewWidget::data_index(std::uint32_t address) const noexcept {
    if(address < this->data_address) {
        return -1;
    }
    auto index = static_cast<std::uint64_t>(address - this->data_address);
    if(index >= this->data_valid) {
        return -1;
    }
    return static_cast<std::int64_t>(index);
}

void HexViewWidget::set_data(std::uint32_t address, const std::uint8_t *bytes, std::size_t length, std::size_t valid_length) {
    valid_length = std::min(valid_length, length);
    auto elapsed = this->heat_clock.isValid() ? this->heat_clock.restart() : 0;
    if(!this->heat_clock.isValid()) {
        this->heat_clock.start();
    }
    int decay = static_cast<int>(std::min<qint64>(255, elapsed * 255 / HEAT_FADE_MS));

    bool changed = address != this->data_address || length != this->data.size() || valid_length != this->data_valid;
    std::vector<std::uint8_t> new_heat(length, 0);
    for(std::size_t i = 0; i < valid_length; i++) {
        std::uint32_t a = address + static_cast<std::uint32_t>(i);
        auto old_index = this->data_index(a);
        if(old_index < 0) {
            continue;
        }
        int h = std::max(0, static_cast<int>(this->heat[old_index]) - decay);
        if(this->data[old_index] != bytes[i]) {
            h = 255;
            changed = true;
        }
        if(h != 0) {
            changed = true;
        }
        new_heat[i] = static_cast<std::uint8_t>(h);
    }

    this->data_address = address;
    this->data.assign(bytes, bytes + length);
    this->data_valid = valid_length;
    this->heat = std::move(new_heat);

    if(!this->pending.empty()) {
        // `second` counts down the samples left before the outline goes away.
        for(auto &p : this->pending) {
            p.second = p.second > 0 ? p.second - 1 : 0;
        }
        this->pending.erase(std::remove_if(this->pending.begin(), this->pending.end(), [](auto &p) { return (p.second & 0xFF) == 0; }), this->pending.end());
        changed = true;
    }

    if(changed) {
        this->viewport()->update();
    }
}

std::optional<std::vector<std::uint8_t>> HexViewWidget::bytes_at(std::uint32_t address, std::size_t length) const {
    auto index = this->data_index(address);
    if(index < 0 || static_cast<std::size_t>(index) + length > this->data_valid) {
        return std::nullopt;
    }
    return std::vector<std::uint8_t>(this->data.begin() + index, this->data.begin() + index + length);
}

void HexViewWidget::set_bytes_per_row(int bytes_per_row) {
    bytes_per_row = std::max(this->group, bytes_per_row);
    if(bytes_per_row == this->row_bytes) {
        return;
    }
    // Keep the cursor's row in view.
    this->row_bytes = bytes_per_row;
    this->update_scroll_range();
    this->ensure_cursor_visible();
    this->emit_window_if_changed();
    this->updateGeometry();
    this->viewport()->update();
}

void HexViewWidget::set_group_size(int group_size) {
    if(group_size == this->group || this->row_bytes % group_size != 0) {
        return;
    }
    this->group = group_size;
    this->nibble = 0;
    this->updateGeometry();
    this->viewport()->update();
}

void HexViewWidget::set_big_endian(bool big_endian) {
    this->big_endian = big_endian;
    this->viewport()->update();
}

void HexViewWidget::set_glyphs(const std::array<QString, 256> &glyphs) {
    this->glyphs = glyphs;
    this->viewport()->update();
}

void HexViewWidget::set_frozen_ranges(std::vector<std::pair<std::uint32_t, std::uint32_t>> ranges) {
    if(ranges != this->frozen) {
        this->frozen = std::move(ranges);
        this->viewport()->update();
    }
}

void HexViewWidget::set_edit_mode(bool edit) {
    this->editing = edit;
    this->nibble = 0;
    this->viewport()->update();
}

void HexViewWidget::mark_pending(std::uint32_t address, std::size_t length) {
    for(std::size_t i = 0; i < length; i++) {
        this->pending.emplace_back(address + static_cast<std::uint32_t>(i), PENDING_SAMPLES);
    }
    this->viewport()->update();
}

bool HexViewWidget::is_frozen(std::uint32_t address) const noexcept {
    for(auto &[start, length] : this->frozen) {
        if(address >= start && static_cast<std::uint64_t>(address) < static_cast<std::uint64_t>(start) + length) {
            return true;
        }
    }
    return false;
}

bool HexViewWidget::is_pending(std::uint32_t address) const noexcept {
    for(auto &p : this->pending) {
        if(p.first == address) {
            return true;
        }
    }
    return false;
}

std::pair<std::uint32_t, std::uint32_t> HexViewWidget::selection() const noexcept {
    auto start = std::min(this->cursor, this->anchor);
    auto end = std::max(this->cursor, this->anchor);
    return {start, end - start + 1};
}

int HexViewWidget::total_rows() const noexcept {
    return static_cast<int>((static_cast<std::uint64_t>(this->region_length) + this->row_bytes - 1) / this->row_bytes);
}

int HexViewWidget::visible_rows() const noexcept {
    return std::max(1, (this->viewport()->height() - this->header_height()) / this->line_height);
}

void HexViewWidget::update_scroll_range() {
    auto *bar = this->verticalScrollBar();
    bar->setRange(0, std::max(0, this->total_rows() - this->visible_rows()));
    bar->setPageStep(this->visible_rows());
    bar->setSingleStep(1);

    auto *hbar = this->horizontalScrollBar();
    int content_width = this->text_x() + this->row_bytes * this->char_width + this->char_width;
    hbar->setRange(0, std::max(0, content_width - this->viewport()->width()));
    hbar->setPageStep(this->viewport()->width());
}

std::pair<std::uint32_t, std::uint32_t> HexViewWidget::visible_window() const {
    if(this->region_length == 0) {
        return {0, 0};
    }
    std::uint64_t first = static_cast<std::uint64_t>(this->verticalScrollBar()->value()) * this->row_bytes;
    std::uint64_t length = static_cast<std::uint64_t>(this->visible_rows() + 1) * this->row_bytes;
    length = std::min<std::uint64_t>(length, this->region_length - std::min<std::uint64_t>(first, this->region_length));
    return {this->base + static_cast<std::uint32_t>(first), static_cast<std::uint32_t>(length)};
}

void HexViewWidget::emit_window_if_changed() {
    auto window = this->visible_window();
    if(window != this->last_window) {
        this->last_window = window;
        emit this->window_changed(window.first, window.second);
    }
}

int HexViewWidget::top_row() const {
    return this->verticalScrollBar()->value();
}

void HexViewWidget::set_top_row(int row) {
    this->verticalScrollBar()->setValue(row);
    this->emit_window_if_changed();
}

int HexViewWidget::address_column_width() const noexcept {
    return (2 + this->address_digits + 2) * this->char_width;
}

int HexViewWidget::cell_chars() const noexcept {
    return this->group * 2 + 1;
}

int HexViewWidget::display_byte_in_group(int k) const noexcept {
    if(this->group == 1 || this->big_endian) {
        return k;
    }
    return this->group - 1 - k;
}

int HexViewWidget::hex_x(int byte_index_in_row) const noexcept {
    int group_index = byte_index_in_row / this->group;
    int k = byte_index_in_row % this->group;
    return this->address_column_width() + (group_index * this->cell_chars() + this->display_byte_in_group(k) * 2) * this->char_width;
}

int HexViewWidget::text_x() const noexcept {
    return this->address_column_width() + (this->row_bytes / this->group) * this->cell_chars() * this->char_width + this->char_width;
}

QSize HexViewWidget::sizeHint() const {
    int width = this->text_x() + this->row_bytes * this->char_width + this->char_width * 2 + this->verticalScrollBar()->sizeHint().width() + this->frameWidth() * 2;
    int height = this->header_height() + this->line_height * 24 + this->frameWidth() * 2;
    return QSize(width, height);
}

void HexViewWidget::center_cursor() {
    if(this->region_length == 0) {
        return;
    }
    // A third of the way down rather than at the very edge.
    int row = static_cast<int>((this->cursor - this->base) / this->row_bytes);
    this->update_scroll_range();
    this->self_scrolling = true;
    this->verticalScrollBar()->setValue(std::max(0, row - this->visible_rows() / 3));
    this->self_scrolling = false;
}

bool HexViewWidget::cursor_visible(int visible_rows) const noexcept {
    if(this->region_length == 0) {
        return false;
    }
    int row = static_cast<int>((this->cursor - this->base) / this->row_bytes);
    int top = this->verticalScrollBar()->value();
    return row >= top && row < top + visible_rows;
}

void HexViewWidget::paintEvent(QPaintEvent *) {
    QPainter painter(this->viewport());
    painter.setFont(this->font);
    auto palette = this->palette();
    auto rect = this->viewport()->rect();
    painter.fillRect(rect, palette.base());

    if(this->region_length == 0) {
        return;
    }

    int x_offset = -this->horizontalScrollBar()->value();
    painter.translate(x_offset, 0);

    QFontMetrics metrics(this->font);
    int ascent = metrics.ascent() + 1;
    QColor text_color = palette.color(QPalette::Text);
    QColor dim_color = text_color;
    dim_color.setAlpha(110);
    QColor header_color = palette.color(QPalette::PlaceholderText);
    QColor highlight = palette.color(QPalette::Highlight);
    QColor selection_color = highlight;
    selection_color.setAlpha(110);
    QColor frozen_color(70, 130, 255, 80);
    QColor heat_base(255, 150, 0);

    // Column header: the offset of each group.
    painter.setPen(header_color);
    {
        QString header(this->address_column_width() / this->char_width, QLatin1Char(' '));
        for(int g = 0; g < this->row_bytes / this->group; g++) {
            header += QString::asprintf("%02X", g * this->group).leftJustified(this->cell_chars(), QLatin1Char(' '));
        }
        painter.drawText(0, ascent + 1, header);
    }
    painter.drawLine(0, this->header_height() - 2, this->text_x() + this->row_bytes * this->char_width, this->header_height() - 2);

    auto [selection_start, selection_length] = this->selection();
    std::uint64_t selection_end = static_cast<std::uint64_t>(selection_start) + selection_length;
    std::uint64_t region_end = static_cast<std::uint64_t>(this->base) + this->region_length;

    int first_row = this->verticalScrollBar()->value();
    int rows = std::min(this->visible_rows() + 1, this->total_rows() - first_row);

    QString hex_text;
    QString char_text;
    for(int r = 0; r < rows; r++) {
        std::uint64_t row_address = static_cast<std::uint64_t>(this->base) + static_cast<std::uint64_t>(first_row + r) * this->row_bytes;
        int y = this->header_height() + r * this->line_height;

        painter.setPen(header_color);
        painter.drawText(0, y + ascent, QString::asprintf("0x%0*llX", this->address_digits, static_cast<unsigned long long>(row_address)));

        hex_text.fill(QLatin1Char(' '), (this->row_bytes / this->group) * this->cell_chars());
        char_text.fill(QLatin1Char(' '), this->row_bytes);
        bool wide_glyphs = false;
        bool any_unknown = false;

        for(int i = 0; i < this->row_bytes; i++) {
            std::uint64_t address64 = row_address + i;
            if(address64 >= region_end) {
                break;
            }
            auto address = static_cast<std::uint32_t>(address64);
            int hx = this->hex_x(i);
            int tx = this->text_x() + i * this->char_width;
            auto index = this->data_index(address);

            // Backgrounds.
            if(index >= 0 && this->heat[index] != 0) {
                QColor c = heat_base;
                c.setAlpha(this->heat[index] * 150 / 255);
                painter.fillRect(hx, y, this->char_width * 2, this->line_height, c);
                painter.fillRect(tx, y, this->char_width, this->line_height, c);
            }
            if(this->is_frozen(address)) {
                painter.fillRect(hx, y, this->char_width * 2, this->line_height, frozen_color);
                painter.fillRect(tx, y, this->char_width, this->line_height, frozen_color);
            }
            if(address >= selection_start && address64 < selection_end) {
                painter.fillRect(hx, y, this->char_width * 2, this->line_height, selection_color);
                painter.fillRect(tx, y, this->char_width, this->line_height, selection_color);
            }

            int text_column = (hx - this->address_column_width()) / this->char_width;
            if(index >= 0) {
                std::uint8_t byte = this->data[index];
                if(this->editing && address == this->cursor && this->nibble == 1) {
                    // Half-typed: show the typed high nibble.
                    byte = this->typed_high_nibble_value(byte);
                }
                static const char *digits = "0123456789ABCDEF";
                hex_text[text_column] = QLatin1Char(digits[byte >> 4]);
                hex_text[text_column + 1] = QLatin1Char(digits[byte & 0xF]);

                const QString &glyph = this->glyphs[byte];
                if(glyph.isEmpty()) {
                    char_text[i] = QLatin1Char('.');
                }
                else {
                    char_text[i] = glyph[0];
                    wide_glyphs = wide_glyphs || glyph[0].unicode() > 0x2FF;
                }
            }
            else {
                hex_text[text_column] = QLatin1Char('-');
                hex_text[text_column + 1] = QLatin1Char('-');
                char_text[i] = QLatin1Char(' ');
                any_unknown = true;
            }

            if(this->is_pending(address)) {
                QPen pen(QColor(255, 190, 0));
                pen.setStyle(Qt::DashLine);
                painter.setPen(pen);
                painter.drawRect(hx, y, this->char_width * 2 - 1, this->line_height - 1);
            }
        }
        (void)any_unknown;

        painter.setPen(text_color);
        painter.drawText(this->address_column_width(), y + ascent, hex_text);
        if(wide_glyphs) {
            for(int i = 0; i < this->row_bytes; i++) {
                painter.drawText(this->text_x() + i * this->char_width, y + ascent, QString(char_text[i]));
            }
        }
        else {
            painter.drawText(this->text_x(), y + ascent, char_text);
        }
    }

    // Cursor.
    if(this->cursor >= this->base && static_cast<std::uint64_t>(this->cursor) < region_end) {
        std::uint64_t offset = this->cursor - this->base;
        int row = static_cast<int>(offset / this->row_bytes) - first_row;
        int i = static_cast<int>(offset % this->row_bytes);
        if(row >= 0 && row <= this->visible_rows()) {
            int y = this->header_height() + row * this->line_height;
            int hx = this->hex_x(i);
            int tx = this->text_x() + i * this->char_width;
            QPen pen(highlight);
            pen.setWidth(this->hasFocus() ? 2 : 1);
            painter.setPen(pen);
            painter.setBrush(Qt::NoBrush);
            if(this->editing) {
                // Underline the nibble being typed in the pane being typed in.
                if(this->text_pane) {
                    painter.drawLine(tx, y + this->line_height - 2, tx + this->char_width, y + this->line_height - 2);
                }
                else {
                    int nx = hx + this->nibble * this->char_width;
                    painter.drawLine(nx, y + this->line_height - 2, nx + this->char_width, y + this->line_height - 2);
                }
                QPen thin(dim_color);
                painter.setPen(thin);
                painter.drawRect(this->text_pane ? hx : tx, y, (this->text_pane ? 2 : 1) * this->char_width - 1, this->line_height - 1);
            }
            else {
                painter.drawRect(hx, y, this->char_width * 2 - 1, this->line_height - 1);
                painter.drawRect(tx, y, this->char_width - 1, this->line_height - 1);
            }
        }
    }
}

std::uint8_t HexViewWidget::typed_high_nibble_value(std::uint8_t current) const noexcept {
    return static_cast<std::uint8_t>((this->typed_high << 4) | (current & 0x0F));
}

void HexViewWidget::resizeEvent(QResizeEvent *event) {
    // Rows that fitted before this resize (the viewport shrinks or grows with the widget).
    int chrome = this->height() - this->viewport()->height();
    int old_rows = std::max(1, (event->oldSize().height() - chrome - this->header_height()) / this->line_height);
    bool was_visible = event->oldSize().isValid() && this->cursor_visible(old_rows);

    QAbstractScrollArea::resizeEvent(event);
    this->update_scroll_range();
    if(this->center_pending) {
        this->center_cursor();
    }
    else if(was_visible && !this->cursor_visible(this->visible_rows())) {
        this->self_scrolling = true;
        this->ensure_cursor_visible();
        this->self_scrolling = false;
    }
    this->emit_window_if_changed();
}

void HexViewWidget::scrollContentsBy(int, int) {
    if(!this->self_scrolling) {
        this->center_pending = false;
    }
    this->emit_window_if_changed();
    this->viewport()->update();
}

bool HexViewWidget::hit_test(QPoint position, std::uint32_t &address, int &hit_nibble, bool &in_text) const {
    if(this->region_length == 0) {
        return false;
    }
    int x = position.x() + this->horizontalScrollBar()->value();
    int y = std::max(position.y() - this->header_height(), 0);
    std::int64_t row = this->verticalScrollBar()->value() + y / this->line_height;
    row = std::clamp<std::int64_t>(row, 0, this->total_rows() - 1);

    int index;
    hit_nibble = 0;
    if(x >= this->text_x() - this->char_width / 2) {
        in_text = true;
        index = std::clamp((x - this->text_x()) / this->char_width, 0, this->row_bytes - 1);
    }
    else {
        in_text = false;
        int relative = std::max(0, x - this->address_column_width());
        int cell_width = this->cell_chars() * this->char_width;
        int group_index = std::min(relative / cell_width, this->row_bytes / this->group - 1);
        int within = std::min(relative - group_index * cell_width, this->group * 2 * this->char_width - 1);
        int displayed = within / (2 * this->char_width);
        hit_nibble = (within % (2 * this->char_width)) >= this->char_width ? 1 : 0;
        index = group_index * this->group + this->display_byte_in_group(displayed);
    }

    std::uint64_t a = static_cast<std::uint64_t>(this->base) + static_cast<std::uint64_t>(row) * this->row_bytes + index;
    a = std::min<std::uint64_t>(a, static_cast<std::uint64_t>(this->base) + this->region_length - 1);
    address = static_cast<std::uint32_t>(a);
    return true;
}

void HexViewWidget::set_cursor(std::uint32_t address, bool extend) {
    if(this->region_length == 0) {
        return;
    }
    std::uint64_t last = static_cast<std::uint64_t>(this->base) + this->region_length - 1;
    address = static_cast<std::uint32_t>(std::clamp<std::uint64_t>(address, this->base, last));
    this->cursor = address;
    if(!extend) {
        this->anchor = address;
    }
    this->nibble = 0;
    this->ensure_cursor_visible();
    this->viewport()->update();
    emit this->cursor_changed(this->cursor);
}

void HexViewWidget::move_cursor(std::int64_t delta, bool extend) {
    std::int64_t target = static_cast<std::int64_t>(this->cursor) + delta;
    target = std::clamp<std::int64_t>(target, this->base, static_cast<std::int64_t>(this->base) + this->region_length - 1);
    this->set_cursor(static_cast<std::uint32_t>(target), extend);
}

void HexViewWidget::ensure_cursor_visible() {
    if(this->region_length == 0) {
        return;
    }
    int row = static_cast<int>((this->cursor - this->base) / this->row_bytes);
    auto *bar = this->verticalScrollBar();
    bool was_self_scrolling = this->self_scrolling;
    this->self_scrolling = true;
    if(row < bar->value()) {
        bar->setValue(row);
    }
    else if(row >= bar->value() + this->visible_rows()) {
        bar->setValue(row - this->visible_rows() + 1);
    }
    this->self_scrolling = was_self_scrolling;
    this->emit_window_if_changed();
}

void HexViewWidget::go_to(std::uint32_t address, std::uint32_t select_length) {
    if(this->region_length == 0) {
        return;
    }
    this->set_cursor(address, false);
    if(select_length > 1) {
        std::uint64_t last = std::min<std::uint64_t>(static_cast<std::uint64_t>(address) + select_length - 1, static_cast<std::uint64_t>(this->base) + this->region_length - 1);
        this->anchor = address;
        this->cursor = static_cast<std::uint32_t>(last);
    }
    int row = static_cast<int>((address - this->base) / this->row_bytes);
    auto *bar = this->verticalScrollBar();
    if(row < bar->value() || row >= bar->value() + this->visible_rows()) {
        this->center_cursor();
    }
    this->center_pending = true;
    this->emit_window_if_changed();
    this->viewport()->update();
    emit this->cursor_changed(this->cursor);
}

void HexViewWidget::mousePressEvent(QMouseEvent *event) {
    this->center_pending = false;
    if(event->button() != Qt::LeftButton && event->button() != Qt::RightButton) {
        return;
    }
    std::uint32_t address;
    int hit_nibble;
    bool in_text;
    if(!this->hit_test(event->position().toPoint(), address, hit_nibble, in_text)) {
        return;
    }
    if(event->button() == Qt::RightButton) {
        // Keep a selection the click lands in.
        auto [start, length] = this->selection();
        if(address >= start && address < start + length) {
            return;
        }
    }
    this->text_pane = in_text;
    this->set_cursor(address, (event->modifiers() & Qt::ShiftModifier) != 0);
    if(this->editing && !in_text) {
        this->nibble = hit_nibble;
    }
    this->dragging = event->button() == Qt::LeftButton;
}

void HexViewWidget::mouseMoveEvent(QMouseEvent *event) {
    if(!this->dragging) {
        return;
    }
    std::uint32_t address;
    int hit_nibble;
    bool in_text;
    if(this->hit_test(event->position().toPoint(), address, hit_nibble, in_text) && address != this->cursor) {
        this->set_cursor(address, true);
    }
}

void HexViewWidget::mouseReleaseEvent(QMouseEvent *) {
    this->dragging = false;
}

void HexViewWidget::contextMenuEvent(QContextMenuEvent *event) {
    emit this->context_menu_requested(event->globalPos());
}

void HexViewWidget::focusOutEvent(QFocusEvent *event) {
    this->nibble = 0;
    QAbstractScrollArea::focusOutEvent(event);
    this->viewport()->update();
}

void HexViewWidget::keyPressEvent(QKeyEvent *event) {
    this->center_pending = false;
    if(this->region_length == 0) {
        QAbstractScrollArea::keyPressEvent(event);
        return;
    }

    bool extend = (event->modifiers() & Qt::ShiftModifier) != 0;
    bool control = (event->modifiers() & (Qt::ControlModifier | Qt::MetaModifier)) != 0;
    int page = this->visible_rows() * this->row_bytes;

    switch(event->key()) {
        case Qt::Key_Left:
            if(this->editing && !this->text_pane && this->nibble == 1 && !extend) {
                this->nibble = 0;
                this->viewport()->update();
            }
            else {
                this->move_cursor(-1, extend);
            }
            return;
        case Qt::Key_Right:
            this->move_cursor(1, extend);
            return;
        case Qt::Key_Up:
            this->move_cursor(-this->row_bytes, extend);
            return;
        case Qt::Key_Down:
            this->move_cursor(this->row_bytes, extend);
            return;
        case Qt::Key_PageUp:
            this->move_cursor(-page, extend);
            return;
        case Qt::Key_PageDown:
            this->move_cursor(page, extend);
            return;
        case Qt::Key_Home:
            if(control) {
                this->set_cursor(this->base, extend);
            }
            else {
                this->set_cursor(this->cursor - (this->cursor - this->base) % this->row_bytes, extend);
            }
            return;
        case Qt::Key_End:
            if(control) {
                this->set_cursor(this->base + this->region_length - 1, extend);
            }
            else {
                this->set_cursor(this->cursor - (this->cursor - this->base) % this->row_bytes + this->row_bytes - 1, extend);
            }
            return;
        case Qt::Key_Tab:
            this->text_pane = !this->text_pane;
            this->nibble = 0;
            this->viewport()->update();
            return;
        case Qt::Key_Escape:
            if(this->nibble != 0) {
                this->nibble = 0;
                this->viewport()->update();
                return;
            }
            break;
        default:
            break;
    }

    if(this->editing && this->writable && !control) {
        QString text = event->text();
        if(!text.isEmpty()) {
            if(this->text_pane) {
                // Find the byte the table maps this character to.
                for(int b = 0; b < 256; b++) {
                    if(this->glyphs[b] == text) {
                        emit this->bytes_typed(this->cursor, QByteArray(1, static_cast<char>(b)));
                        this->mark_pending(this->cursor, 1);
                        this->move_cursor(1, false);
                        return;
                    }
                }
                QAbstractScrollArea::keyPressEvent(event);
                return;
            }

            bool ok = false;
            int digit = QString(text[0]).toInt(&ok, 16);
            if(ok && text.size() == 1) {
                if(this->nibble == 0) {
                    this->typed_high = static_cast<std::uint8_t>(digit);
                    this->nibble = 1;
                    this->viewport()->update();
                }
                else {
                    auto value = static_cast<char>((this->typed_high << 4) | digit);
                    emit this->bytes_typed(this->cursor, QByteArray(1, value));
                    this->mark_pending(this->cursor, 1);
                    this->move_cursor(1, false);
                }
                return;
            }
        }
    }

    QAbstractScrollArea::keyPressEvent(event);
}
