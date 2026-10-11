//! Terminal emulator buffer: a `vte` parser driving a custom grid + scrollback.
//!
//! The daemon keeps one of these per session and re-emits a full redraw on
//! (re)attach. This is the "full emulator + replay" half of the architecture:
//! the child's PTY bytes are fed into [`Emulator::ingest`], which updates the
//! grid and scrollback, and [`Emulator::render_snapshot`] serialises the current
//! screen + scrollback back into an escape-sequence stream a client replays.
//!
//! # Backend decision
//! We use the **custom vte grid** (the `vte` crate's state-machine `Parser`
//! driving our own [`grid`] module). Rationale: the `vt100` crate pulls in a
//! *second* vte version (0.11) and exposes its own grid with less control over
//! scrollback/reflow and no cheap per-cell escape serialisation, while
//! `wezterm-term` is far heavier than the engine needs. A hand-rolled grid keeps
//! the crate small, gives exact control over the `scrollback_cap` and reflow, and
//! mirrors the proven shell-rs grid so this stays byte-compatible with the Shell
//! app's renderer. Both `vte` and `vt100` stay declared in the manifest (owned by
//! the bootstrap card) so this remains a one-line flip if ever needed.

#[path = "grid.rs"]
mod grid;

use grid::{char_width, Attr, Cell, Color, Grid, Row};
use std::collections::VecDeque;

/// Cursor state: position plus the pending auto-wrap flag (the cursor sits at the
/// right margin and the next printable char must start a new line).
#[derive(Clone, Copy, Debug, Default)]
struct Cursor {
    row: usize,
    col: usize,
    wrap_pending: bool,
}

/// A point-in-time snapshot of the terminal state used for save/restore
/// (DECSC/DECRC, CSI s/u, and the smcup/rmcup primary-state save).
#[derive(Clone, Copy, Debug)]
struct SavedState {
    cursor: Cursor,
    attr: Attr,
    wrap_enabled: bool,
    scroll_top: usize,
    scroll_bottom: usize,
}

impl SavedState {
    /// Fit the saved cursor and scroll region inside a `rows` x `cols` screen.
    fn clamp(&mut self, rows: usize, cols: usize) {
        let bottom = rows - 1;
        self.cursor.row = self.cursor.row.min(bottom);
        self.cursor.col = self.cursor.col.min(cols - 1);
        self.cursor.wrap_pending = false;
        self.scroll_top = self.scroll_top.min(bottom);
        self.scroll_bottom = self.scroll_bottom.max(self.scroll_top).min(bottom);
    }
}

/// A vte-driven terminal emulator buffer.
pub struct Emulator {
    primary: Grid,
    alt: Grid,
    in_alt: bool,
    cols: usize,
    rows: usize,
    cursor: Cursor,
    attr: Attr,
    wrap_enabled: bool,
    scroll_top: usize,
    scroll_bottom: usize,
    /// Saved state for DECSC/DECRC and CSI s/u.
    saved: Option<SavedState>,
    /// Primary-mode state saved at smcup, restored at rmcup.
    saved_primary: Option<SavedState>,
    cursor_visible: bool,
    parser: vte::Parser,
}

impl Emulator {
    pub fn new(rows: usize, cols: usize, scrollback_cap: usize) -> Self {
        let rows = rows.max(1);
        let cols = cols.max(1);
        Emulator {
            primary: Grid::new(rows, cols, scrollback_cap),
            alt: Grid::new(rows, cols, 0),
            in_alt: false,
            cols,
            rows,
            cursor: Cursor::default(),
            attr: Attr::default(),
            wrap_enabled: true,
            scroll_top: 0,
            scroll_bottom: rows - 1,
            saved: None,
            saved_primary: None,
            cursor_visible: true,
            parser: vte::Parser::new(),
        }
    }

    /// Feed raw bytes from the child's PTY into the emulator.
    pub fn ingest(&mut self, bytes: &[u8]) {
        let mut parser = std::mem::take(&mut self.parser);
        parser.advance(self, bytes);
        self.parser = parser;
    }

    /// Resize the viewport, reflowing existing content to the new column width.
    pub fn resize(&mut self, rows: usize, cols: usize) {
        if rows == 0 || cols == 0 {
            return;
        }
        self.primary.reflow(cols, rows);
        self.alt.reflow(cols, rows);
        self.cols = cols;
        self.rows = rows;

        // Re-pad both grids to the exact new row count and clamp cursor / region.
        let bottom = rows - 1;
        for grid in [&mut self.primary, &mut self.alt] {
            grid.rows = rows;
            grid.lines.truncate(rows);
            while grid.lines.len() < rows {
                grid.lines.push(Row::blank(cols));
            }
            grid.fit_lines();
        }
        self.scroll_top = self.scroll_top.min(bottom);
        self.scroll_bottom = self.scroll_bottom.max(self.scroll_top).min(bottom);
        let (_, cursor) = self.active_parts();
        cursor.row = cursor.row.min(bottom);
        cursor.col = cursor.col.min(cols - 1);
        cursor.wrap_pending = false;
        // Saved states (DECSC / CSI s, and the primary state parked at smcup)
        // are restored verbatim later, so they must fit the new size too.
        for saved in [&mut self.saved, &mut self.saved_primary].into_iter().flatten() {
            saved.clamp(rows, cols);
        }
    }

    /// Full terminal reset (ESC c / RIS).
    pub fn reset(&mut self) {
        self.primary.clear(Cell::default());
        self.alt.clear(Cell::default());
        self.in_alt = false;
        self.cursor = Cursor::default();
        self.attr = Attr::default();
        self.wrap_enabled = true;
        self.scroll_top = 0;
        self.scroll_bottom = self.rows - 1;
        self.saved = None;
        self.saved_primary = None;
        self.primary.clear_scrollback();
    }

    /// Serialise the current screen + scrollback (and truecolor/alt-screen state)
    /// as an escape-sequence stream that reproduces this buffer when replayed.
    pub fn render_snapshot(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let grid = if self.in_alt {
            &self.alt
        } else {
            &self.primary
        };

        // Establish the target mode and a clean slate before spooling.
        if self.in_alt {
            out.extend_from_slice(b"\x1b[?1049h");
        } else {
            out.extend_from_slice(b"\x1b[?1049l");
        }
        out.extend_from_slice(b"\x1b[r\x1b[2J\x1b[H");

        // Spool scrollback then screen, one line per row, with CRLF after every
        // line except the last. Writing more rows than the viewport causes the
        // top rows to scroll into the consumer's scrollback, so the last `rows`
        // lines land on-screen and the remainder become its scrollback.
        let mut lines: Vec<&Row> = Vec::new();
        lines.extend(grid.scrollback.iter());
        lines.extend(grid.lines.iter());
        let total = lines.len();
        for (i, line) in lines.iter().enumerate() {
            self.render_line(line, &mut out);
            if i + 1 < total {
                out.push(b'\r');
                out.push(b'\n');
            }
        }

        // Restore the cursor to its live position (1-based).
        let r = self.cursor.row + 1;
        let c = (self.cursor.col.min(self.cols.saturating_sub(1))) + 1;
        out.extend_from_slice(format!("\x1b[{};{}H", r, c).as_bytes());
        out
    }

    /// Render one row of cells into `out`, emitting a full SGR resync whenever
    /// the cell attribute changes.
    fn render_line(&self, line: &Row, out: &mut Vec<u8>) {
        out.push(b'\r');
        let mut last_attr: Option<Attr> = None;
        let mut col = 0usize;
        while col < line.cells.len() {
            let cell = &line.cells[col];
            if cell.continued {
                col += 1;
                continue;
            }
            if last_attr != Some(cell.attr) {
                out.extend_from_slice(&sgr_bytes(&cell.attr));
                last_attr = Some(cell.attr);
            }
            let mut s = String::new();
            s.push(cell.ch);
            for cc in &cell.combining {
                s.push(*cc);
            }
            out.extend_from_slice(s.as_bytes());
            col += cell.width.max(1) as usize;
        }
    }

    /// The active grid (by mode).
    fn active_grid(&self) -> &Grid {
        if self.in_alt {
            &self.alt
        } else {
            &self.primary
        }
    }

    /// Borrow the active grid and the cursor simultaneously (disjoint fields).
    fn active_parts(&mut self) -> (&mut Grid, &mut Cursor) {
        if self.in_alt {
            (&mut self.alt, &mut self.cursor)
        } else {
            (&mut self.primary, &mut self.cursor)
        }
    }

    /// Write one character at the cursor, handling pending wrap, wide chars,
    /// and zero-width combining marks.
    fn write_char(&mut self, c: char) {
        let cols = self.cols;
        let rows = self.rows;
        let wrap_enabled = self.wrap_enabled;
        let bottom = self.scroll_bottom;
        let top = self.scroll_top;
        let attr = self.attr;

        let (grid, cursor) = self.active_parts();

        // Consume a pending wrap: the cursor moved to the next line first.
        if wrap_enabled && cursor.wrap_pending {
            cursor.wrap_pending = false;
            // The row we are leaving was full and its content continues onto the
            // next row: set its wrap flag so reflow can rejoin the logical line.
            grid.lines[cursor.row].wrapped = true;
            if cursor.row == bottom {
                grid.scroll_up(top, bottom);
            } else if cursor.row + 1 < rows {
                // Below the scroll region on the last line: stay put.
                cursor.row += 1;
            }
            cursor.col = 0;
        }

        // A wide char can never fit a 1-column screen: treat it as narrow.
        let w = match char_width(c) {
            2 if cols < 2 => 1,
            w => w,
        };
        if w == 0 {
            // Zero-width (combining): attach to the previously written cell.
            if cursor.col > 0 {
                grid.lines[cursor.row].cells[cursor.col - 1]
                    .combining
                    .push(c);
            } else {
                grid.lines[cursor.row].cells[0].combining.push(c);
            }
            return;
        }

        // Make room for a wide character (2 cells).
        if w == 2 && cursor.col + 2 > cols {
            if wrap_enabled {
                grid.lines[cursor.row].wrapped = true;
                if cursor.row == bottom {
                    grid.scroll_up(top, bottom);
                } else if cursor.row + 1 < rows {
                    cursor.row += 1;
                }
                cursor.col = 0;
            } else {
                cursor.col = cols - 2;
            }
        }
        // With autowrap off (DECAWM reset) the cursor sticks at the right
        // margin and each further char overwrites the last column.
        if cursor.col + w as usize > cols {
            cursor.col = cols - w as usize;
        }

        let row = cursor.row;
        let col = cursor.col;

        if w == 2 {
            grid.lines[row].cells[col] = Cell {
                ch: c,
                width: 2,
                continued: false,
                combining: Vec::new(),
                attr,
            };
            grid.lines[row].cells[col + 1] = Cell {
                ch: ' ',
                width: 0,
                continued: true,
                combining: Vec::new(),
                attr,
            };
            cursor.col += 2;
        } else {
            grid.lines[row].cells[col] = Cell {
                ch: c,
                width: 1,
                continued: false,
                combining: Vec::new(),
                attr,
            };
            cursor.col += 1;
        }

        if cursor.col >= cols {
            cursor.col = cols;
            cursor.wrap_pending = true;
        }
    }

    fn line_feed(&mut self, n: usize) {
        let top = self.scroll_top;
        let bottom = self.scroll_bottom;
        let rows = self.rows;
        let (grid, cursor) = self.active_parts();
        for _ in 0..n {
            if cursor.row >= bottom {
                grid.scroll_up(top, bottom);
                if cursor.row > bottom {
                    cursor.row = bottom;
                }
            } else if cursor.row + 1 < rows {
                cursor.row += 1;
            }
        }
        cursor.wrap_pending = false;
    }

    fn reverse_index(&mut self) {
        let top = self.scroll_top;
        let bottom = self.scroll_bottom;
        let (grid, cursor) = self.active_parts();
        if cursor.row == top {
            grid.scroll_down(top, bottom);
        } else if cursor.row > 0 {
            cursor.row -= 1;
        }
        cursor.wrap_pending = false;
    }

    fn carriage_return(&mut self) {
        self.cursor.col = 0;
        self.cursor.wrap_pending = false;
    }

    fn tab(&mut self) {
        let cols = self.cols;
        let (_, cursor) = self.active_parts();
        let next = ((cursor.col / 8) + 1) * 8;
        cursor.col = next.min(cols.saturating_sub(1));
        cursor.wrap_pending = false;
    }

    fn backspace(&mut self) {
        let (_, cursor) = self.active_parts();
        if cursor.col > 0 {
            cursor.col -= 1;
        }
        cursor.wrap_pending = false;
    }

    fn move_up(&mut self, n: u64) {
        let (_, cursor) = self.active_parts();
        cursor.row = cursor.row.saturating_sub(n as usize);
        cursor.wrap_pending = false;
    }

    fn move_down(&mut self, n: u64) {
        let rows = self.rows;
        let (_, cursor) = self.active_parts();
        cursor.row = (cursor.row + n as usize).min(rows - 1);
        cursor.wrap_pending = false;
    }

    fn move_right(&mut self, n: u64) {
        let cols = self.cols;
        let (_, cursor) = self.active_parts();
        cursor.col = (cursor.col + n as usize).min(cols - 1);
        cursor.wrap_pending = false;
    }

    fn move_left(&mut self, n: u64) {
        let (_, cursor) = self.active_parts();
        cursor.col = cursor.col.saturating_sub(n as usize);
        cursor.wrap_pending = false;
    }

    fn cursor_to(&mut self, row: usize, col: usize) {
        let rows = self.rows;
        let cols = self.cols;
        let (_, cursor) = self.active_parts();
        cursor.row = row.min(rows - 1);
        cursor.col = col.min(cols - 1);
        cursor.wrap_pending = false;
    }

    /// Erase in display (CSI J).
    fn erase_display(&mut self, mode: u64) {
        let rows = self.rows;
        let cols = self.cols;
        let (grid, cursor) = self.active_parts();
        let r = cursor.row;
        let c = cursor.col;
        match mode {
            0 => {
                for cell in grid.lines[r].cells.iter_mut().skip(c) {
                    *cell = Cell::default();
                }
                for rr in (r + 1)..rows {
                    grid.lines[rr] = Row::blank(cols);
                }
            }
            1 => {
                for rr in 0..r {
                    grid.lines[rr] = Row::blank(cols);
                }
                for cell in grid.lines[r].cells.iter_mut().take(c.min(cols)) {
                    *cell = Cell::default();
                }
            }
            2 => {
                for rr in 0..rows {
                    grid.lines[rr] = Row::blank(cols);
                }
            }
            3 => grid.clear_scrollback(),
            _ => {}
        }
    }

    /// Erase in line (CSI K).
    fn erase_line(&mut self, mode: u64) {
        let cols = self.cols;
        let (grid, cursor) = self.active_parts();
        let c = cursor.col.min(cols);
        match mode {
            0 => {
                for cell in grid.lines[cursor.row].cells.iter_mut().skip(c) {
                    *cell = Cell::default();
                }
            }
            1 => {
                for cell in grid.lines[cursor.row].cells.iter_mut().take(c.min(cols)) {
                    *cell = Cell::default();
                }
            }
            2 => {
                grid.lines[cursor.row] = Row::blank(cols);
            }
            _ => {}
        }
    }

    fn set_scroll_region(&mut self, top: usize, bottom: usize) {
        self.scroll_top = top.min(self.rows - 1);
        self.scroll_bottom = bottom.min(self.rows - 1).max(self.scroll_top);
        self.cursor.row = 0;
        self.cursor.col = 0;
        self.cursor.wrap_pending = false;
    }

    fn set_alt_screen(&mut self, enter: bool) {
        if enter {
            if self.in_alt {
                return;
            }
            self.saved_primary = Some(SavedState {
                cursor: self.cursor,
                attr: self.attr,
                wrap_enabled: self.wrap_enabled,
                scroll_top: self.scroll_top,
                scroll_bottom: self.scroll_bottom,
            });
            self.in_alt = true;
            self.alt.clear(Cell::default());
            self.cursor = Cursor::default();
            self.attr = Attr::default();
            self.wrap_enabled = true;
            self.scroll_top = 0;
            self.scroll_bottom = self.rows - 1;
        } else {
            self.in_alt = false;
            if let Some(s) = self.saved_primary.take() {
                self.cursor = s.cursor;
                self.attr = s.attr;
                self.wrap_enabled = s.wrap_enabled;
                self.scroll_top = s.scroll_top;
                self.scroll_bottom = s.scroll_bottom;
            }
        }
    }

    /// Apply an SGR sequence to the current attribute state.
    fn apply_sgr(&mut self, params: &vte::Params) {
        let vals: Vec<u64> = params.iter().flatten().map(|&x| u64::from(x)).collect();
        let mut i = 0;
        while i < vals.len() {
            let v = vals[i];
            match v {
                0 => self.attr = Attr::default(),
                1 => self.attr.bold = true,
                2 => self.attr.dim = true,
                3 => self.attr.italic = true,
                4 => self.attr.underline = true,
                5 | 6 => self.attr.blink = true,
                7 => self.attr.inverse = true,
                8 => {}
                9 => self.attr.strikethrough = true,
                21 => self.attr.underline = true,
                22 => {
                    self.attr.bold = false;
                    self.attr.dim = false;
                }
                23 => self.attr.italic = false,
                24 => self.attr.underline = false,
                25 => self.attr.blink = false,
                27 => self.attr.inverse = false,
                29 => self.attr.strikethrough = false,
                30..=37 => self.attr.fg = Color::Indexed((v - 30) as u8),
                38 => {
                    if vals.get(i + 1) == Some(&2) {
                        let r = *vals.get(i + 2).unwrap_or(&0);
                        let g = *vals.get(i + 3).unwrap_or(&0);
                        let b = *vals.get(i + 4).unwrap_or(&0);
                        self.attr.fg = Color::Rgb(r as u8, g as u8, b as u8);
                        i += 4;
                    } else if vals.get(i + 1) == Some(&5) {
                        let n = *vals.get(i + 2).unwrap_or(&0);
                        self.attr.fg = Color::Indexed(n as u8);
                        i += 2;
                    }
                }
                39 => self.attr.fg = Color::Default,
                40..=47 => self.attr.bg = Color::Indexed((v - 40) as u8),
                48 => {
                    if vals.get(i + 1) == Some(&2) {
                        let r = *vals.get(i + 2).unwrap_or(&0);
                        let g = *vals.get(i + 3).unwrap_or(&0);
                        let b = *vals.get(i + 4).unwrap_or(&0);
                        self.attr.bg = Color::Rgb(r as u8, g as u8, b as u8);
                        i += 4;
                    } else if vals.get(i + 1) == Some(&5) {
                        let n = *vals.get(i + 2).unwrap_or(&0);
                        self.attr.bg = Color::Indexed(n as u8);
                        i += 2;
                    }
                }
                49 => self.attr.bg = Color::Default,
                90..=97 => self.attr.fg = Color::Indexed((v - 90 + 8) as u8),
                100..=107 => self.attr.bg = Color::Indexed((v - 100 + 8) as u8),
                _ => {}
            }
            i += 1;
        }
    }

    // ---- Public accessors (used by the session type and tests) ----

    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn cols(&self) -> usize {
        self.cols
    }

    pub fn cursor_pos(&self) -> (usize, usize) {
        (self.cursor.row, self.cursor.col)
    }

    pub fn in_alt_screen(&self) -> bool {
        self.in_alt
    }

    pub fn scrollback_len(&self) -> usize {
        self.primary.scrollback.len()
    }

    pub fn viewport(&self) -> &[Row] {
        &self.active_grid().lines
    }

    pub fn scrollback(&self) -> &VecDeque<Row> {
        &self.primary.scrollback
    }
}

/// Build the SGR bytes that fully (re)establish `attr` after a reset.
fn sgr_bytes(attr: &Attr) -> Vec<u8> {
    let mut parts: Vec<u8> = Vec::new();
    parts.extend_from_slice(b"\x1b[0");
    if attr.bold {
        parts.extend_from_slice(b";1");
    }
    if attr.dim {
        parts.extend_from_slice(b";2");
    }
    if attr.italic {
        parts.extend_from_slice(b";3");
    }
    if attr.underline {
        parts.extend_from_slice(b";4");
    }
    if attr.blink {
        parts.extend_from_slice(b";5");
    }
    if attr.inverse {
        parts.extend_from_slice(b";7");
    }
    if attr.strikethrough {
        parts.extend_from_slice(b";9");
    }
    match attr.fg {
        Color::Rgb(r, g, b) => {
            parts.extend_from_slice(format!(";38;2;{};{};{}", r, g, b).as_bytes())
        }
        Color::Indexed(n) => parts.extend_from_slice(format!(";38;5;{}", n).as_bytes()),
        Color::Default => parts.extend_from_slice(b";39"),
    }
    match attr.bg {
        Color::Rgb(r, g, b) => {
            parts.extend_from_slice(format!(";48;2;{};{};{}", r, g, b).as_bytes())
        }
        Color::Indexed(n) => parts.extend_from_slice(format!(";48;5;{}", n).as_bytes()),
        Color::Default => parts.extend_from_slice(b";49"),
    }
    parts.push(b'm');
    parts
}

impl vte::Perform for Emulator {
    fn print(&mut self, c: char) {
        self.write_char(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            0x07 | 0x00 => {}
            0x08 => self.backspace(),
            0x09 => self.tab(),
            0x0a | 0x0b | 0x0c => self.line_feed(1),
            0x0d => self.carriage_return(),
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        match byte {
            b'7' => {
                self.saved = Some(SavedState {
                    cursor: self.cursor,
                    attr: self.attr,
                    wrap_enabled: self.wrap_enabled,
                    scroll_top: self.scroll_top,
                    scroll_bottom: self.scroll_bottom,
                });
            }
            b'8' => {
                if let Some(s) = self.saved {
                    self.cursor = s.cursor;
                    self.attr = s.attr;
                    self.wrap_enabled = s.wrap_enabled;
                    self.scroll_top = s.scroll_top;
                    self.scroll_bottom = s.scroll_bottom;
                }
            }
            b'M' => self.reverse_index(),
            b'D' => self.line_feed(1),
            b'E' => {
                self.carriage_return();
                self.line_feed(1);
            }
            b'c' => self.reset(),
            // Charset designations (ESC ( B, ESC ) 0, ...) and tab-stop setting
            // (ESC H) are accepted but not applied.
            _ => {}
        }
        let _ = intermediates;
    }

    fn csi_dispatch(
        &mut self,
        params: &vte::Params,
        intermediates: &[u8],
        _ignore: bool,
        action: char,
    ) {
        let private = matches!(intermediates.first(), Some(b'?'));
        let p = |idx: usize| -> u64 {
            params
                .iter()
                .nth(idx)
                .and_then(|g| g.first().copied())
                .map(u64::from)
                .unwrap_or(1)
        };

        match action {
            'A' => self.move_up(p(0)),
            'B' => self.move_down(p(0)),
            'C' => self.move_right(p(0)),
            'D' => self.move_left(p(0)),
            'E' => {
                self.move_down(p(0));
                self.carriage_return();
            }
            'F' => {
                self.move_up(p(0));
                self.carriage_return();
            }
            'G' | '`' => {
                let c = (p(0) as usize).saturating_sub(1);
                self.cursor_to(self.cursor.row, c);
            }
            'H' | 'f' => {
                let r = (p(0) as usize).saturating_sub(1);
                let c = (p(1) as usize).saturating_sub(1);
                self.cursor_to(r, c);
            }
            'd' => {
                let r = (p(0) as usize).saturating_sub(1);
                self.cursor_to(r, self.cursor.col);
            }
            'J' => self.erase_display(p(0)),
            'K' => self.erase_line(p(0)),
            'L' => {
                let n = p(0) as usize;
                let top = self.scroll_top;
                let bottom = self.scroll_bottom;
                let (grid, cursor) = self.active_parts();
                grid.insert_lines(top, bottom, cursor.row, n);
            }
            'M' => {
                let n = p(0) as usize;
                let top = self.scroll_top;
                let bottom = self.scroll_bottom;
                let (grid, cursor) = self.active_parts();
                grid.delete_lines(top, bottom, cursor.row, n);
            }
            'P' => {
                let n = p(0) as usize;
                let (grid, cursor) = self.active_parts();
                grid.delete_chars(cursor.row, cursor.col, n);
            }
            '@' => {
                let n = p(0) as usize;
                let (grid, cursor) = self.active_parts();
                grid.insert_chars(cursor.row, cursor.col, n);
            }
            'X' => {
                let n = p(0) as usize;
                let (grid, cursor) = self.active_parts();
                grid.erase_chars(cursor.row, cursor.col, n);
            }
            'S' => {
                let n = p(0) as usize;
                let top = self.scroll_top;
                let bottom = self.scroll_bottom;
                let (grid, _) = self.active_parts();
                for _ in 0..n {
                    grid.scroll_up(top, bottom);
                }
            }
            'T' => {
                let n = p(0) as usize;
                let top = self.scroll_top;
                let bottom = self.scroll_bottom;
                let (grid, _) = self.active_parts();
                for _ in 0..n {
                    grid.scroll_down(top, bottom);
                }
            }
            'r' => {
                // DECSTBM. Omitted params mean the full screen (top=1, bottom=rows),
                // which is distinct from an explicit value. Read presence explicitly.
                let rows = self.rows;
                let top = params
                    .iter()
                    .nth(0)
                    .and_then(|g| g.first())
                    .map(|&v| v as usize)
                    .unwrap_or(1);
                let bottom = params
                    .iter()
                    .nth(1)
                    .and_then(|g| g.first())
                    .map(|&v| v as usize)
                    .unwrap_or(rows);
                let top1 = top.saturating_sub(1).min(rows - 1);
                let bottom1 = bottom.saturating_sub(1).min(rows - 1);
                self.set_scroll_region(top1, bottom1.max(top1));
            }
            'm' => self.apply_sgr(params),
            's' => {
                self.saved = Some(SavedState {
                    cursor: self.cursor,
                    attr: self.attr,
                    wrap_enabled: self.wrap_enabled,
                    scroll_top: self.scroll_top,
                    scroll_bottom: self.scroll_bottom,
                });
            }
            'u' => {
                if let Some(s) = self.saved {
                    self.cursor = s.cursor;
                    self.attr = s.attr;
                    self.wrap_enabled = s.wrap_enabled;
                    self.scroll_top = s.scroll_top;
                    self.scroll_bottom = s.scroll_bottom;
                }
            }
            'h' | 'l' => {
                let set = action == 'h';
                if private {
                    match p(0) {
                        7 => self.wrap_enabled = set,
                        25 => self.cursor_visible = set,
                        47 | 1047 | 1049 => self.set_alt_screen(set),
                        _ => {}
                    }
                }
                // Non-private mode set (DECCOLM etc.) is not handled.
            }
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, _params: &[&[u8]], _bell_terminated: bool) {
        // OSC 0/1/2 (title etc.) are intentionally ignored by the grid.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row_text(row: &Row) -> String {
        let mut s = String::new();
        let mut i = 0;
        while i < row.cells.len() {
            let c = &row.cells[i];
            if c.continued {
                i += 1;
                continue;
            }
            s.push(c.ch);
            for cc in &c.combining {
                s.push(*cc);
            }
            i += c.width.max(1) as usize;
        }
        // Trailing blank cells are padding, not content.
        s.trim_end().to_string()
    }

    fn em_text(e: &Emulator) -> String {
        let mut lines: Vec<String> = Vec::new();
        for r in e.scrollback() {
            lines.push(row_text(r));
        }
        for r in e.viewport() {
            lines.push(row_text(r));
        }
        lines.join("\n")
    }

    fn trimmed_text(e: &Emulator) -> String {
        em_text(e)
            .lines()
            .map(|l| l.trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn contains_probe(e: &Emulator) -> bool {
        em_text(e).contains("__SESS_PROBE_123__")
    }

    #[test]
    fn prints_and_wraps() {
        let mut e = Emulator::new(3, 5, 100);
        e.ingest(b"HelloWorld");
        // wraps at column 5, so "Hello" on row0, "World" on row1
        assert_eq!(row_text(&e.viewport()[0]), "Hello");
        assert_eq!(row_text(&e.viewport()[1]), "World");
        assert_eq!(e.cursor_pos().0, 1);
    }

    #[test]
    fn crlf_feeds_and_scrollback() {
        let mut e = Emulator::new(3, 10, 100);
        for i in 0..6 {
            e.ingest(format!("L{}\r\n", i).as_bytes());
        }
        // 3 visible rows + a final newline: the history accumulates the scrolled
        // lines. All six L0..L5 lines must survive somewhere in scrollback+screen.
        let all = em_text(&e);
        for i in 0..6 {
            assert!(
                all.contains(&format!("L{i}")),
                "L{i} should be retained, got: {all}"
            );
        }
        assert!(
            e.scrollback_len() > 0,
            "some lines should have scrolled off"
        );
        // The newest line ends up visible.
        let screen = trimmed_text(&e);
        assert!(
            screen.contains("L5"),
            "screen should show L5, got: {screen}"
        );
    }

    #[test]
    fn backspace_and_cr() {
        let mut e = Emulator::new(2, 10, 100);
        e.ingest(b"ab\x08c");
        assert_eq!(row_text(&e.viewport()[0]), "ac");
        e.ingest(b"\r");
        assert_eq!(e.cursor_pos(), (0, 0));
    }

    #[test]
    fn alt_screen_roundtrip() {
        let mut e = Emulator::new(3, 10, 100);
        e.ingest(b"main");
        assert!(!e.in_alt_screen());
        e.ingest(b"\x1b[?1049h"); // smcup
        assert!(e.in_alt_screen(), "should be in alt screen");
        assert!(
            e.viewport().iter().all(|r| row_text(r).is_empty()),
            "alt screen should be cleared"
        );
        e.ingest(b"alt");
        assert_eq!(row_text(&e.viewport()[0]), "alt");
        e.ingest(b"\x1b[?1049l"); // rmcup
        assert!(!e.in_alt_screen(), "should be back on primary");
        assert_eq!(
            row_text(&e.viewport()[0]),
            "main",
            "primary content restored"
        );
    }

    #[test]
    fn sgr_truecolor_and_256() {
        let mut e = Emulator::new(2, 20, 100);
        e.ingest(b"\x1b[38;2;255;0;0mR\x1b[0m");
        let front = e.viewport()[0].cells[0].ch;
        assert_eq!(front, 'R');
        assert_eq!(e.viewport()[0].cells[0].attr.fg, Color::Rgb(255, 0, 0));

        let mut e2 = Emulator::new(2, 20, 100);
        e2.ingest(b"\x1b[38;5;200mN");
        assert_eq!(e2.viewport()[0].cells[0].attr.fg, Color::Indexed(200));
        // 16-color fallback (bright red via 91 = 8+9)
        let mut e3 = Emulator::new(2, 20, 100);
        e3.ingest(b"\x1b[91mN");
        assert_eq!(e3.viewport()[0].cells[0].attr.fg, Color::Indexed(9));
    }

    #[test]
    fn insert_delete_chars() {
        let mut e = Emulator::new(1, 10, 100);
        e.ingest(b"abc");
        e.ingest(b"\x1b[1;1H\x1b[2@"); // insert 2 at col 1
        assert_eq!(row_text(&e.viewport()[0]), "  abc");
        e.ingest(b"\x1b[1;1H\x1b[2P"); // delete 2 at col 1
        assert_eq!(row_text(&e.viewport()[0]), "abc");
    }

    #[test]
    fn reflow_rewraps() {
        // A 2-row grid so the content exactly fills it (no trailing blank row to
        // complicate the reflow split). At width 8 "abcdefghij" wraps to
        // "abcdefgh" + "ij"; reflowing to width 4 re-broken it into
        // "abcd","efgh","ij" with the last rows staying visible.
        let mut e = Emulator::new(2, 8, 100);
        e.ingest(b"abcdefghij");
        assert_eq!(row_text(&e.viewport()[0]), "abcdefgh");
        assert_eq!(row_text(&e.viewport()[1]), "ij");
        e.resize(2, 4);
        // Content preserved and re-wrapped; the newest rows stay on screen.
        assert_eq!(e.scrollback_len(), 1, "one reflowed line should scroll off");
        assert_eq!(row_text(&e.viewport()[0]), "efgh", "reflowed screen line 0");
        assert_eq!(row_text(&e.viewport()[1]), "ij", "reflowed screen line 1");
        assert_eq!(em_text(&e), "abcd\nefgh\nij", "whole history re-wrapped");
    }

    #[test]
    fn snapshot_roundtrip_reproduces_grid() {
        let mut e = Emulator::new(3, 10, 50);
        // Build some content that exercises color, scrollback, and a cursor move.
        e.ingest(b"\x1b[38;2;1;2;3mHello\nWorld\r\n");
        e.ingest(b"\x1b[0mColored\nFirst\r\nSek\x08c\r\n");
        let snapshot = e.render_snapshot();

        let mut r = Emulator::new(3, 10, 50);
        r.ingest(&snapshot);

        assert_eq!(
            trimmed_text(&r),
            trimmed_text(&e),
            "replayed screen + scrollback should match original"
        );
        assert_eq!(
            r.scrollback_len(),
            e.scrollback_len(),
            "scrollback should match"
        );
        assert_eq!(r.cursor_pos(), e.cursor_pos(), "cursor should match");
        // Truecolor survived the round trip (the colored row lives in scrollback).
        let colors: Vec<Color> = r
            .scrollback()
            .iter()
            .chain(r.viewport().iter())
            .flat_map(|row| row.cells.iter())
            .map(|c| c.attr.fg)
            .collect();
        assert!(colors.contains(&Color::Rgb(1, 2, 3)));
    }

    /// The structural invariants every grid operation relies on for indexing.
    fn assert_invariants(e: &Emulator, ctx: &str) {
        for (name, grid) in [("primary", &e.primary), ("alt", &e.alt)] {
            assert_eq!(grid.lines.len(), e.rows, "{ctx}: {name} row count");
            for (i, row) in grid.lines.iter().enumerate() {
                assert_eq!(row.cells.len(), e.cols, "{ctx}: {name} row {i} width");
            }
        }
        assert!(e.cursor.row < e.rows, "{ctx}: cursor row {}", e.cursor.row);
        assert!(e.cursor.col <= e.cols, "{ctx}: cursor col {}", e.cursor.col);
        assert!(e.scroll_top <= e.scroll_bottom, "{ctx}: scroll region inverted");
        assert!(e.scroll_bottom < e.rows, "{ctx}: scroll bottom {}", e.scroll_bottom);
    }

    // Regression: reflow used to leave rows shorter than the width, so the next
    // printed char after any resize indexed past the end of its row.
    #[test]
    fn print_after_resize_does_not_panic() {
        let mut e = Emulator::new(24, 80, 100);
        e.ingest(b"sh-3.2$ ");
        e.resize(24, 100);
        e.ingest(b"echo hi\x1b[1;10H\x1b[5P\x1b[3@");
        assert_invariants(&e, "after resize");
        assert_eq!(row_text(&e.viewport()[0]), "sh-3.2$ e   i");
    }

    // Regression: with autowrap off the cursor sticks at the right margin and
    // overwrites the last column instead of writing past the row.
    #[test]
    fn autowrap_off_overwrites_last_column() {
        let mut e = Emulator::new(2, 5, 100);
        e.ingest(b"\x1b[?7labcdefg");
        assert_eq!(row_text(&e.viewport()[0]), "abcdg");
        assert_eq!(row_text(&e.viewport()[1]), "");
        e.ingest("\u{6f22}".as_bytes()); // a wide char lands in the last two columns
        assert_eq!(row_text(&e.viewport()[0]), "abc\u{6f22}");
        assert_invariants(&e, "autowrap off");
    }

    // Regression: saved cursors (smcup's parked primary state, DECSC, CSI s)
    // used to be restored at their pre-resize position, past the new bottom.
    #[test]
    fn saved_state_is_clamped_on_shrink() {
        let mut e = Emulator::new(50, 80, 100);
        e.ingest(b"\x1b[50;1H$ \x1b[?1049h");
        e.resize(24, 80);
        e.ingest(b"\x1b[?1049lx");
        assert_invariants(&e, "rmcup after shrink");

        let mut e = Emulator::new(50, 80, 100);
        e.ingest(b"\x1b[45;70H\x1b7\x1b[s");
        e.resize(24, 40);
        e.ingest(b"\x1b8x\x1b[ux\x1b[L\x1b[M");
        assert_invariants(&e, "DECRC after shrink");
    }

    // Reflow must not grow a wide char's trailing half into an extra blank.
    #[test]
    fn reflow_keeps_wide_chars_two_cells() {
        let mut e = Emulator::new(2, 10, 100);
        e.ingest("\u{6f22}\u{5b57}x".as_bytes());
        e.resize(2, 12);
        e.resize(2, 10);
        assert_eq!(row_text(&e.viewport()[0]), "\u{6f22}\u{5b57}x");
    }

    // Deterministic stress: random printable text, controls, wide/combining
    // chars, CSI/ESC sequences, and resizes (down to 1x1) must never panic or
    // break the grid invariants, and a snapshot must always replay cleanly.
    #[test]
    fn random_input_and_resizes_never_panic() {
        struct Rng(u64);
        impl Rng {
            fn next(&mut self) -> u64 {
                self.0 ^= self.0 << 13;
                self.0 ^= self.0 >> 7;
                self.0 ^= self.0 << 17;
                self.0
            }
            fn below(&mut self, n: u64) -> u64 {
                self.next() % n
            }
        }
        const FINALS: &[u8] = b"ABCDEFGHJKLMPSTX@dfrsumhl`";
        const PRIVATE_MODES: &[u64] = &[7, 25, 47, 1047, 1049];

        for seed in 1..=150u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15));
            let mut e = Emulator::new(24, 80, 50);
            for step in 0..300 {
                let mut bytes: Vec<u8> = Vec::new();
                match rng.below(10) {
                    0 => {
                        let rows = 1 + rng.below(60) as usize;
                        let cols = 1 + rng.below(160) as usize;
                        e.resize(rows, cols);
                    }
                    1 => {
                        let mode = PRIVATE_MODES[rng.below(PRIVATE_MODES.len() as u64) as usize];
                        let set = if rng.below(2) == 0 { 'h' } else { 'l' };
                        bytes.extend_from_slice(format!("\x1b[?{mode}{set}").as_bytes());
                    }
                    2 | 3 => {
                        bytes.extend_from_slice(b"\x1b[");
                        if rng.below(4) == 0 {
                            bytes.push(b"?><"[rng.below(3) as usize]);
                        }
                        for i in 0..rng.below(3) {
                            if i > 0 {
                                bytes.push(b';');
                            }
                            if rng.below(4) > 0 {
                                bytes.extend_from_slice(rng.below(90).to_string().as_bytes());
                            }
                        }
                        bytes.push(FINALS[rng.below(FINALS.len() as u64) as usize]);
                    }
                    4 => {
                        bytes.push(0x1b);
                        bytes.push(b"78MDEc"[rng.below(6) as usize]);
                    }
                    5 => bytes.push(b"\r\n\x08\t\x0b"[rng.below(5) as usize]),
                    6 => bytes.extend_from_slice(
                        ["\u{6f22}", "\u{1f600}", "e\u{301}", "\u{ff21}"][rng.below(4) as usize]
                            .as_bytes(),
                    ),
                    _ => {
                        for _ in 0..1 + rng.below(120) {
                            bytes.push(b' ' + rng.below(95) as u8);
                        }
                    }
                }
                e.ingest(&bytes);
                assert_invariants(&e, &format!("seed {seed} step {step} after {bytes:?}"));
            }
            let mut replay = Emulator::new(e.rows, e.cols, 50);
            replay.ingest(&e.render_snapshot());
            assert_invariants(&replay, &format!("seed {seed} replay"));
        }
    }

    #[test]
    fn live_child_prompt_renders() {
        use portable_pty::{native_pty_system, CommandBuilder, PtySize};
        use std::io::{Read, Write};

        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                ..PtySize::default()
            })
            .expect("open pty");
        let mut child = pair
            .slave
            .spawn_command(CommandBuilder::new("/bin/sh"))
            .expect("spawn shell");
        let mut reader = pair.master.try_clone_reader().expect("reader");
        let mut writer = pair.master.take_writer().expect("writer");

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut em = Emulator::new(24, 80, 2000);
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => {
                        let _ = tx.send(em);
                        break;
                    }
                    Ok(n) => {
                        em.ingest(&buf[..n]);
                        if contains_probe(&em) {
                            let _ = tx.send(em);
                            break;
                        }
                    }
                }
            }
        });

        // Give the shell a fraction of a second, then drive it from the PTY.
        std::thread::sleep(std::time::Duration::from_millis(400));
        writer
            .write_all(b"echo __SESS_PROBE_123__\n")
            .expect("write cmd");
        writer.flush().expect("flush");

        let em = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("child output should arrive in time");
        let _ = child.kill();

        assert!(
            contains_probe(&em),
            "live child output should appear in the grid; text was:\n{}",
            trimmed_text(&em)
        );
    }
}
