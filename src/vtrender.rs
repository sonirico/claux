//! Renders a `vt100::Screen` (fed from control-mode `%output` bytes) into a
//! ratatui buffer, cell for cell. This replaces the `ansi-to-tui` text
//! reparse the polling preview used: vt100 already tracks a real terminal
//! grid sized to the exact preview area (via `refresh-client -C`), so there
//! is no re-wrapping to get wrong.

use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::Widget;

/// Map a vt100 color to its ratatui equivalent. `Default` maps to `None` so
/// the ratatui `Style` leaves the cell on the widget's base color instead of
/// forcing black/white.
fn map_color(c: vt100::Color) -> Option<Color> {
    match c {
        vt100::Color::Default => None,
        vt100::Color::Idx(i) => Some(Color::Indexed(i)),
        vt100::Color::Rgb(r, g, b) => Some(Color::Rgb(r, g, b)),
    }
}

/// Build the ratatui `Style` for one vt100 cell. Reverse video is expressed
/// as `Modifier::REVERSED` rather than swapping fg/bg here, so the terminal
/// backend applies the same reverse semantics it would for any other
/// reversed cell (matching real terminal behavior for e.g. reversed
/// default-color cells).
fn cell_style(cell: &vt100::Cell) -> Style {
    let mut style = Style::default();
    if let Some(fg) = map_color(cell.fgcolor()) {
        style = style.fg(fg);
    }
    if let Some(bg) = map_color(cell.bgcolor()) {
        style = style.bg(bg);
    }
    let mut modifiers = Modifier::empty();
    if cell.bold() {
        modifiers |= Modifier::BOLD;
    }
    if cell.italic() {
        modifiers |= Modifier::ITALIC;
    }
    if cell.underline() {
        modifiers |= Modifier::UNDERLINED;
    }
    if cell.inverse() {
        modifiers |= Modifier::REVERSED;
    }
    style.add_modifier(modifiers)
}

/// A ratatui widget that draws one frame of a vt100 screen. Built fresh each
/// draw call from a reference to the parser's current screen; owns nothing.
pub struct VtScreen<'a> {
    screen: &'a vt100::Screen,
    bottom: bool,
}

impl<'a> VtScreen<'a> {
    pub fn new(screen: &'a vt100::Screen) -> Self {
        Self {
            screen,
            bottom: false,
        }
    }

    /// Like `new`, but when the screen has more rows than the render area,
    /// skips the topmost `rows - area.height` screen rows so the bottom rows
    /// fill the area instead.
    pub fn bottom_anchored(screen: &'a vt100::Screen) -> Self {
        Self {
            screen,
            bottom: true,
        }
    }
}

impl Widget for VtScreen<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let (rows, cols) = self.screen.size();
        let row_off = if self.bottom {
            rows.saturating_sub(area.height)
        } else {
            0
        };
        for r in 0..(rows - row_off) {
            if r >= area.height {
                break;
            }
            let row = row_off + r;
            for col in 0..cols {
                if col >= area.width {
                    break;
                }
                let Some(cell) = self.screen.cell(row, col) else {
                    continue;
                };
                let pos = Position::new(area.x + col, area.y + r);
                let Some(buf_cell) = buf.cell_mut(pos) else {
                    continue;
                };
                if cell.is_wide_continuation() {
                    // The wide char to its left already covers this column;
                    // ratatui skips zero-width filler cells on its own.
                    continue;
                }
                let symbol = if cell.has_contents() {
                    cell.contents()
                } else {
                    " "
                };
                buf_cell.set_symbol(symbol);
                buf_cell.set_style(cell_style(cell));
            }
        }
        if !self.screen.hide_cursor() {
            let (crow, ccol) = self.screen.cursor_position();
            if crow >= row_off && crow - row_off < area.height && ccol < area.width {
                let pos = Position::new(area.x + ccol, area.y + (crow - row_off));
                if let Some(buf_cell) = buf.cell_mut(pos) {
                    buf_cell.set_style(buf_cell.style().add_modifier(Modifier::REVERSED));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;

    fn parser_from(bytes: &[u8], rows: u16, cols: u16) -> vt100::Parser {
        let mut p = vt100::Parser::new(rows, cols, 0);
        p.process(bytes);
        p
    }

    #[test]
    fn plain_colored_char_maps_fg_and_symbol() {
        // SGR 32 = green fg, then 'x'.
        let parser = parser_from(b"\x1b[32mx", 1, 5);
        let widget = VtScreen::new(parser.screen());
        let area = Rect::new(0, 0, 5, 1);
        let mut buf = Buffer::empty(area);
        widget.render(area, &mut buf);
        let cell = buf.cell((0, 0)).unwrap();
        assert_eq!(cell.symbol(), "x");
        assert_eq!(cell.style().fg, Some(Color::Indexed(2)));
    }

    #[test]
    fn cursor_cell_is_reversed_when_visible() {
        let parser = parser_from(b"ab", 1, 5);
        let widget = VtScreen::new(parser.screen());
        let area = Rect::new(0, 0, 5, 1);
        let mut buf = Buffer::empty(area);
        widget.render(area, &mut buf);
        // Cursor sits after "ab", i.e. column 2.
        let cursor_cell = buf.cell((2, 0)).unwrap();
        assert!(
            cursor_cell
                .style()
                .add_modifier
                .contains(Modifier::REVERSED)
        );
        let non_cursor_cell = buf.cell((0, 0)).unwrap();
        assert!(
            !non_cursor_cell
                .style()
                .add_modifier
                .contains(Modifier::REVERSED)
        );
    }

    #[test]
    fn bottom_anchored_shows_last_rows() {
        let parser = parser_from(b"l1\r\nl2\r\nl3\r\nl4", 4, 5);
        let widget = VtScreen::bottom_anchored(parser.screen());
        let area = Rect::new(0, 0, 5, 2);
        let mut buf = Buffer::empty(area);
        widget.render(area, &mut buf);
        assert_eq!(buf.cell((0, 0)).unwrap().symbol(), "l");
        assert_eq!(buf.cell((1, 0)).unwrap().symbol(), "3");
        assert_eq!(buf.cell((0, 1)).unwrap().symbol(), "l");
        assert_eq!(buf.cell((1, 1)).unwrap().symbol(), "4");
    }

    #[test]
    fn bottom_anchored_cursor_outside_window_not_drawn() {
        let parser = parser_from(b"l1\r\nl2\r\nl3\r\nl4\x1b[1;1H", 4, 5);
        let widget = VtScreen::bottom_anchored(parser.screen());
        let area = Rect::new(0, 0, 5, 2);
        let mut buf = Buffer::empty(area);
        widget.render(area, &mut buf);
        for x in 0..area.width {
            for y in 0..area.height {
                let cell = buf.cell((x, y)).unwrap();
                assert!(!cell.style().add_modifier.contains(Modifier::REVERSED));
            }
        }
    }

    #[test]
    fn hidden_cursor_is_not_drawn_reversed() {
        // \x1b[?25l hides the cursor (DECTCEM reset).
        let parser = parser_from(b"\x1b[?25lab", 1, 5);
        let widget = VtScreen::new(parser.screen());
        let area = Rect::new(0, 0, 5, 1);
        let mut buf = Buffer::empty(area);
        widget.render(area, &mut buf);
        let cell_at_cursor_pos = buf.cell((2, 0)).unwrap();
        assert!(
            !cell_at_cursor_pos
                .style()
                .add_modifier
                .contains(Modifier::REVERSED)
        );
    }
}
