#ifndef __SUPERSHUCKIE_HEX_VIEW_WIDGET_HPP__
#define __SUPERSHUCKIE_HEX_VIEW_WIDGET_HPP__

#include <QAbstractScrollArea>
#include <QElapsedTimer>
#include <QStaticText>
#include <QString>
#include <array>
#include <cstdint>
#include <optional>
#include <utility>
#include <vector>

namespace SuperShuckie64 {

/**
 * A hex view of one memory region that paints only the rows on screen.
 *
 * It does not read memory itself: it asks for the bytes it shows through window_changed() and is
 * handed them with set_data() whenever a new sample arrives. Bytes that change between samples are
 * highlighted for two seconds, then fade out over two more.
 */
class HexViewWidget: public QAbstractScrollArea {
    Q_OBJECT
public:
    HexViewWidget(QWidget *parent);

    /** Show the region [base, base + length). Resets the view to its start. */
    void set_region(std::uint32_t base, std::uint32_t length, bool writable, int address_digits);

    /** Forget the region (no game). */
    void clear_region();

    bool has_region() const noexcept { return this->region_length != 0; }
    std::uint32_t region_base() const noexcept { return this->base; }
    std::uint32_t region_size() const noexcept { return this->region_length; }

    /** New bytes for [address, address + bytes.size()); only the first valid_length are mapped. */
    void set_data(std::uint32_t address, const std::uint8_t *bytes, std::size_t length, std::size_t valid_length);

    /** The bytes last handed over, if they cover [address, address + length). */
    std::optional<std::vector<std::uint8_t>> bytes_at(std::uint32_t address, std::size_t length) const;

    void set_bytes_per_row(int bytes_per_row);
    void set_group_size(int group_size);
    void set_big_endian(bool big_endian);
    int bytes_per_row() const noexcept { return this->row_bytes; }
    int group_size() const noexcept { return this->group; }
    bool is_big_endian() const noexcept { return this->big_endian; }

    /** What each byte stands for in the character pane (empty for unknown). */
    void set_glyphs(const std::array<QString, 256> &glyphs);

    /** Move the cursor to address (and select it), scrolling it into view. */
    void go_to(std::uint32_t address, std::uint32_t select_length = 1);

    std::uint32_t cursor_address() const noexcept { return this->cursor; }
    std::pair<std::uint32_t, std::uint32_t> selection() const noexcept;

    /** The first address and length on screen. */
    std::pair<std::uint32_t, std::uint32_t> visible_window() const;

    /** Row the view is scrolled to (for restoring). */
    int top_row() const;
    void set_top_row(int row);

    /** Byte ranges drawn as frozen. */
    void set_frozen_ranges(std::vector<std::pair<std::uint32_t, std::uint32_t>> ranges);

    /** Allow typing over bytes. */
    void set_edit_mode(bool edit);
    bool edit_mode() const noexcept { return this->editing; }

    /** Outline bytes that were just edited until a few samples have come in. */
    void mark_pending(std::uint32_t address, std::size_t length);

    QSize sizeHint() const override;

signals:
    void window_changed(std::uint32_t address, std::uint32_t length);
    void cursor_changed(std::uint32_t address);
    void bytes_typed(std::uint32_t address, QByteArray bytes);
    void context_menu_requested(QPoint global_position);

protected:
    void paintEvent(QPaintEvent *event) override;
    void resizeEvent(QResizeEvent *event) override;
    void scrollContentsBy(int dx, int dy) override;
    void mousePressEvent(QMouseEvent *event) override;
    void mouseMoveEvent(QMouseEvent *event) override;
    void mouseReleaseEvent(QMouseEvent *event) override;
    void keyPressEvent(QKeyEvent *event) override;
    void contextMenuEvent(QContextMenuEvent *event) override;
    void focusOutEvent(QFocusEvent *event) override;

private:
    std::uint32_t base = 0;
    std::uint32_t region_length = 0;
    bool writable = false;
    int address_digits = 8;

    int row_bytes = 16;
    int group = 1;
    bool big_endian = false;

    // Last sample.
    std::uint32_t data_address = 0;
    std::vector<std::uint8_t> data;
    std::size_t data_valid = 0;
    /** Milliseconds left before each byte's change highlight is gone. */
    std::vector<std::uint16_t> heat;
    QElapsedTimer heat_clock;

    std::uint32_t cursor = 0;
    std::uint32_t anchor = 0;
    /** Keep centring the cursor on resizes until the user scrolls (go_to() before the final layout). */
    bool center_pending = false;
    /** The view is scrolling itself (not the user). */
    bool self_scrolling = false;
    void center_cursor();
    bool cursor_visible(int visible_rows) const noexcept;
    bool text_pane = false;
    bool dragging = false;

    bool editing = false;
    int nibble = 0;
    std::uint8_t typed_high = 0;
    std::uint8_t typed_high_nibble_value(std::uint8_t current) const noexcept;

    std::vector<std::pair<std::uint32_t, std::uint32_t>> frozen;
    std::vector<std::pair<std::uint32_t, std::uint32_t>> pending;

    std::array<QString, 256> glyphs;

    QFont font;
    int char_width = 8;
    int line_height = 16;

    // Every character is drawn on its own at a multiple of char_width: the font's real advance is
    // usually fractional, so a whole row drawn as one string drifts away from the highlights.
    std::array<QStaticText, 16> digit_texts;
    QStaticText unknown_text;
    std::array<QStaticText, 256> glyph_texts;
    QStaticText prepared_text(const QString &text) const;
    void prepare_glyph_texts();

    std::pair<std::uint32_t, std::uint32_t> last_window = {0, 0};

    int total_rows() const noexcept;
    int visible_rows() const noexcept;
    void update_scroll_range();
    void emit_window_if_changed();

    int address_column_width() const noexcept;
    int hex_x(int byte_index_in_row) const noexcept;
    int cell_chars() const noexcept;
    int text_x() const noexcept;
    int header_height() const noexcept { return this->line_height + 4; }

    /** Which byte (and nibble, and pane) is under a viewport position. */
    bool hit_test(QPoint position, std::uint32_t &address, int &hit_nibble, bool &in_text) const;

    void move_cursor(std::int64_t delta, bool extend);
    void set_cursor(std::uint32_t address, bool extend);
    void ensure_cursor_visible();

    /** Index into `data` for an address, or -1 if not sampled/mapped. */
    std::int64_t data_index(std::uint32_t address) const noexcept;

    bool is_frozen(std::uint32_t address) const noexcept;
    bool is_pending(std::uint32_t address) const noexcept;

    /** The displayed position of byte `k` within its group (endianness applied). */
    int display_byte_in_group(int k) const noexcept;
};

}

#endif
