//! Grid + scrollback data structures for the vte emulator, including reflow.
//!
//! This module is declared inside `emulator.rs` via a `#[path = "grid.rs"] mod grid;`
//! (see the ownership note there) so it is `crate::emulator::grid`. It holds the
//! terminal's visible screen, a capped scrollback buffer, cell/attribute types,
//! and the resize reflow algorithm.

use std::collections::VecDeque;

/// A terminal color: TRUE-color (24-bit), indexed 256/16, or the palette default.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Color {
    Rgb(u8, u8, u8),
    Indexed(u8),
    Default,
}

impl Default for Color {
    fn default() -> Self {
        Color::Default
    }
}

/// Graphic rendition (SGR) attributes applied to a cell.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Attr {
    /// Foreground color.
    pub fg: Color,
    /// Background color.
    pub bg: Color,
    /// Bold intensity.
    pub bold: bool,
    /// Dim / faint intensity.
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    /// Slow blink.
    pub blink: bool,
    /// Reverse video (swap fg/bg at render time).
    pub inverse: bool,
    pub strikethrough: bool,
}

/// A single character cell.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Cell {
    /// The base character; ' ' for blanks and the trailing half of wide chars.
    pub ch: char,
    /// Display width in cells: 0 (combining / trailing half), 1, or 2 (wide char).
    pub width: u8,
    /// True when this cell is the trailing blank of a width-2 wide char.
    pub continued: bool,
    /// Zero-width combining marks that modify `ch`, in typed order.
    pub combining: Vec<char>,
    pub attr: Attr,
}

impl Default for Cell {
    fn default() -> Self {
        Cell {
            ch: ' ',
            width: 1,
            continued: false,
            combining: Vec::new(),
            attr: Attr::default(),
        }
    }
}

impl Cell {
    pub fn blank() -> Self {
        Cell::default()
    }

    pub fn is_blank(&self) -> bool {
        self.ch == ' ' && self.combining.is_empty() && self.attr == Attr::default()
    }
}

/// One row of `cols` cells plus the auto-wrap flag.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Row {
    pub cells: Vec<Cell>,
    /// True when this row was filled to the right margin and continues onto the
    /// next row. This is the wrap flag used by reflow to re-join logical lines.
    pub wrapped: bool,
}

impl Row {
    pub fn blank(cols: usize) -> Row {
        Row {
            cells: vec![Cell::default(); cols],
            wrapped: false,
        }
    }
}

/// A grid: the visible screen (`rows` lines) plus a capped scrollback buffer.
#[derive(Clone, Debug)]
pub struct Grid {
    pub cols: usize,
    pub rows: usize,
    pub lines: Vec<Row>,
    /// Lines that have scrolled off the top of the screen (oldest first).
    /// Only populated when `scrollback_cap > 0` (the alternate screen sets it 0).
    pub scrollback: VecDeque<Row>,
    pub scrollback_cap: usize,
}

impl Grid {
    pub fn new(rows: usize, cols: usize, scrollback_cap: usize) -> Grid {
        Grid {
            cols,
            rows,
            lines: (0..rows).map(|_| Row::blank(cols)).collect(),
            scrollback: VecDeque::new(),
            scrollback_cap,
        }
    }

    #[inline]
    pub fn blank_row(&self) -> Row {
        Row::blank(self.cols)
    }

    #[inline]
    pub fn cell(&self, row: usize, col: usize) -> &Cell {
        &self.lines[row].cells[col]
    }

    /// Clear every screen line to `fill` and drop all scrollback.
    pub fn clear(&mut self, fill: Cell) {
        for r in 0..self.rows {
            self.lines[r] = Row {
                cells: vec![fill.clone(); self.cols],
                wrapped: false,
            };
        }
        self.scrollback.clear();
    }

    /// Scroll the region `[top, bottom]` (inclusive, 0-based) up by one row.
    /// When `top == 0` and the grid keeps scrollback, the line leaving the top
    /// of the screen is pushed onto the scrollback (honouring
    /// `scrollback_cap`). When `top > 0` (a partial scroll region) the line is
    /// discarded and the region shifts up only. A blank row is introduced at
    /// `bottom`.
    pub fn scroll_up(&mut self, top: usize, bottom: usize) {
        let top = top.min(self.rows.saturating_sub(1));
        let bottom = bottom.min(self.rows.saturating_sub(1));
        let top = top.min(bottom);
        if self.rows == 0 {
            return;
        }
        let blank = self.blank_row();
        if top == 0 {
            let evicted = self.lines.remove(0);
            if self.scrollback_cap > 0 {
                self.scrollback.push_back(evicted);
                while self.scrollback.len() > self.scrollback_cap {
                    self.scrollback.pop_front();
                }
            }
            self.lines.push(blank);
        } else {
            // Partial region: shift `[top, bottom]` up, blank the bottom. The
            // rows above `top` stay put; the row that was at `top` is lost.
            for r in top..bottom {
                self.lines[r] = std::mem::replace(&mut self.lines[r + 1], blank.clone());
            }
            self.lines[bottom] = blank;
        }
    }

    /// Scroll the region `[top, bottom]` down by one row: a blank row is
    /// introduced at `top` and the row at `bottom` is lost. Scrollback is never
    /// populated by a downward scroll.
    pub fn scroll_down(&mut self, top: usize, bottom: usize) {
        let top = top.min(self.rows.saturating_sub(1));
        let bottom = bottom.min(self.rows.saturating_sub(1));
        let top = top.min(bottom);
        if self.rows == 0 {
            return;
        }
        let blank = self.blank_row();
        for r in (top..bottom).rev() {
            self.lines[r + 1] = std::mem::replace(&mut self.lines[r], blank.clone());
        }
        self.lines[top] = blank;
    }

    /// Insert `n` blank rows at `row` inside the region `[top, bottom]`, pushing
    /// the remainder down; rows that overflow `bottom` are dropped.
    pub fn insert_lines(&mut self, top: usize, bottom: usize, row: usize, n: usize) {
        let bottom = bottom.min(self.rows.saturating_sub(1));
        let row = row.clamp(top, bottom);
        let n = n.min(bottom - row + 1);
        for _ in 0..n {
            self.lines.insert(row, self.blank_row());
            self.lines.truncate(self.rows);
        }
    }

    /// Delete `n` lines starting at `row` inside the region `[top, bottom]`,
    /// pulling lines from below up; blanks fill the bottom of the region.
    pub fn delete_lines(&mut self, top: usize, bottom: usize, row: usize, n: usize) {
        let bottom = bottom.min(self.rows.saturating_sub(1));
        let row = row.clamp(top, bottom);
        let n = n.min(bottom - row + 1);
        for _ in 0..n {
            self.lines.remove(row);
            self.lines.push(self.blank_row());
        }
    }

    /// Delete `n` characters at `row`, starting at `col`, shifting the rest of
    /// the row left and blanking the right side.
    pub fn delete_chars(&mut self, row: usize, col: usize, n: usize) {
        if row >= self.rows {
            return;
        }
        let col = col.min(self.cols);
        let n = n.min(self.cols - col);
        let line = &mut self.lines[row].cells;
        line.drain(col..col + n);
        line.resize(self.cols, Cell::default());
    }

    /// Insert `n` blank characters at `row, col`, shifting the right side over.
    pub fn insert_chars(&mut self, row: usize, col: usize, n: usize) {
        if row >= self.rows {
            return;
        }
        let col = col.min(self.cols);
        let n = n.min(self.cols - col);
        let line = &mut self.lines[row].cells;
        line.splice(col..col, (0..n).map(|_| Cell::default()));
        line.truncate(self.cols);
    }

    /// Erase `n` characters at `row, col` (leaving the rest of the row intact).
    pub fn erase_chars(&mut self, row: usize, col: usize, n: usize) {
        if row >= self.rows {
            return;
        }
        let col = col.min(self.cols);
        let n = n.min(self.cols - col);
        for c in self.lines[row].cells.iter_mut().skip(col).take(n) {
            *c = Cell::default();
        }
    }

    pub fn clear_scrollback(&mut self) {
        self.scrollback.clear();
    }

    /// Reflow the grid to a new size. Existing logical lines (scrollback joined
    /// with the screen, using each row's wrap flag to merge wrapped runs) are
    /// re-broken at `new_cols` and re-distributed so the *last* `new_rows` lines
    /// are visible and the earlier ones become scrollback (the standard resize
    /// behaviour: the viewport stays anchored to the bottom of the history).
    pub fn reflow(&mut self, new_cols: usize, new_rows: usize) {
        if new_cols == 0 || new_rows == 0 {
            return;
        }

        // Collect every logical row (scrollback oldest->newest, then screen),
        // joining rows whose wrap flag says they continue onto the next one.
        let all: Vec<Row> = self
            .scrollback
            .drain(..)
            .chain(self.lines.drain(..))
            .collect();
        let mut logical: Vec<Vec<Cell>> = Vec::new();
        let mut cur: Vec<Cell> = Vec::new();
        for row in all {
            cur.extend(row.cells);
            if !row.wrapped {
                logical.push(std::mem::take(&mut cur));
            }
        }
        if !cur.is_empty() || logical.is_empty() {
            logical.push(cur);
        }

        // Re-break each logical line at the new width. Trailing default-blank
        // padding cells (a row is always full-width) are trimmed first so they
        // don't inflate the reflowed line into extra empty rows.
        let mut new_rows_buf: Vec<Row> = Vec::new();
        for mut line in logical {
            while line.last().map(|c| c.is_blank()).unwrap_or(false) {
                line.pop();
            }
            // A line with no real content (a blank screen row) stays a single
            // blank row at the new width rather than being re-broken.
            if line.is_empty() {
                new_rows_buf.push(Row::blank(new_cols));
                continue;
            }
            let mut cells: Vec<Cell> = Vec::with_capacity(new_cols);
            let mut i = 0;
            while i < line.len() {
                let cell = &line[i];
                let cw = cell.width.max(1) as usize;
                if cw == 2 && cells.len() + cw > new_cols {
                    // A wide char cannot span a column boundary: drop its
                    // trailing half and clamp it to width 1 at the last column.
                    let mut cc = cell.clone();
                    cc.width = 1;
                    cc.continued = false;
                    cc.combining = Vec::new();
                    cells.push(cc);
                    i += 1;
                    continue;
                }
                if cells.len() + cw > new_cols {
                    // Row is full and more content follows: wrap it.
                    new_rows_buf.push(Row {
                        cells: std::mem::take(&mut cells),
                        wrapped: true,
                    });
                    continue;
                }
                if cw == 2 {
                    let mut cc = cell.clone();
                    cc.continued = false;
                    cells.push(cc);
                    cells.push(Cell {
                        ch: ' ',
                        width: 0,
                        continued: true,
                        combining: Vec::new(),
                        attr: cell.attr,
                    });
                    i += 1;
                } else {
                    let mut cc = cell.clone();
                    cc.continued = false;
                    cc.width = 1;
                    cells.push(cc);
                    i += 1;
                }
            }
            new_rows_buf.push(Row {
                cells,
                wrapped: false,
            });
        }

        // The last `new_rows` logical rows are the visible screen; everything
        // before them is scrollback (capped).
        let total = new_rows_buf.len();
        if total <= new_rows {
            self.lines = new_rows_buf;
            self.scrollback.clear();
            while self.lines.len() < new_rows {
                self.lines.push(Row::blank(new_cols));
            }
        } else {
            let split = total - new_rows;
            let scroll_part: Vec<Row> = new_rows_buf.drain(..split).collect();
            self.scrollback = scroll_part.into();
            self.lines = new_rows_buf;
            while self.scrollback.len() > self.scrollback_cap {
                self.scrollback.pop_front();
            }
        }

        self.cols = new_cols;
        self.rows = new_rows;
    }

    /// Grow each cached line with the reserve `cells` for a `blank` cell.
    /// Forces a re-layout so line lengths match `cols` exactly.
    pub fn fit_lines(&mut self) {
        for line in &mut self.lines {
            line.cells.resize(self.cols, Cell::default());
        }
    }
}

/// Approximate display width of a character in cells: 0 (zero-width / combining
/// / control), 1 (normal), or 2 (East Asian Wide/Fullwidth). Full grapheme
/// segmentation is out of scope for the cell model; this is the same heuristic
/// used by the Shell grid and is accurate for TUI/box-art content.
pub fn char_width(c: char) -> u8 {
    let cp = c as u32;
    if cp < 0x20 || (0x7f..0xa0).contains(&cp) {
        return 0;
    }
    // Zero-width combining marks (a representative, well-covered set).
    if matches!(
        cp,
        0x0300..=0x036f
            | 0x0483..=0x0489
            | 0x0591..=0x05bd
            | 0x05bf
            | 0x05c1..=0x05c2
            | 0x05c4..=0x05c5
            | 0x05c7
            | 0x0610..=0x061a
            | 0x064b..=0x065f
            | 0x0670
            | 0x06d6..=0x06ed
            | 0x0711
            | 0x0730..=0x074a
            | 0x07a6..=0x07b0
            | 0x07eb..=0x07f3
            | 0x0816..=0x082d
            | 0x0859..=0x085b
            | 0x08d3..=0x0902
            | 0x093a
            | 0x093c
            | 0x0941..=0x0948
            | 0x094d
            | 0x0951..=0x0957
            | 0x0962..=0x0963
            | 0x200b..=0x200f
            | 0x2060..=0x2064
            | 0xfe00..=0xfe0f
            | 0xfe20..=0xfe2f
            | 0x1d165..=0x1d169
            | 0x1d16d..=0x1d172
            | 0x1d17b..=0x1d182
            | 0x1d185..=0x1d18b
            | 0x1d1aa..=0x1d1ad
    ) {
        return 0;
    }
    // East Asian Wide / Fullwidth.
    if matches!(
        cp,
        0x1100..=0x115f
            | 0x2329..=0x232a
            | 0x2e80..=0x303e
            | 0x3041..=0x33ff
            | 0x3400..=0x4dbf
            | 0x4e00..=0x9fff
            | 0xa000..=0xa4cf
            | 0xa960..=0xa97f
            | 0xac00..=0xd7a3
            | 0xf900..=0xfaff
            | 0xfe10..=0xfe19
            | 0xfe30..=0xfe6f
            | 0xff00..=0xff60
            | 0xffe0..=0xffe6
            | 0x1b000..=0x1b001
            | 0x1f200..=0x1f251
            | 0x20000..=0x2fffd
            | 0x30000..=0x3fffd
    ) {
        return 2;
    }
    1
}
