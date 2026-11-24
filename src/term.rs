//! Terminal state machine: grid, scrollback, cursor and the VT/xterm escape
//! sequence handling driven by the `vte` parser.

use std::collections::{HashMap, VecDeque};
use std::time::Instant;
use unicode_width::UnicodeWidthChar;
use vte::{Params, Perform};

// ---------------------------------------------------------------------------
// Cells & colors
// ---------------------------------------------------------------------------

/// Packed color: tag in the top byte.
/// 0 = default, 1 = indexed (low byte), 2 = rgb (low 24 bits = 0xRRGGBB).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Color(pub u32);

impl Color {
    pub const DEFAULT: Color = Color(0);
    pub const fn indexed(i: u8) -> Color {
        Color(0x0100_0000 | i as u32)
    }
    pub const fn rgb(r: u8, g: u8, b: u8) -> Color {
        Color(0x0200_0000 | (r as u32) << 16 | (g as u32) << 8 | b as u32)
    }
}

pub mod flags {
    pub const BOLD: u16 = 1 << 0;
    pub const DIM: u16 = 1 << 1;
    pub const ITALIC: u16 = 1 << 2;
    pub const UNDERLINE: u16 = 1 << 3;
    pub const INVERSE: u16 = 1 << 4;
    pub const HIDDEN: u16 = 1 << 5;
    pub const STRIKE: u16 = 1 << 6;
    /// First half of a double-width character.
    pub const WIDE: u16 = 1 << 7;
    /// Second half of a double-width character (holds no character).
    pub const WIDE_SPACER: u16 = 1 << 8;
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cell {
    pub c: char,
    pub fg: Color,
    pub bg: Color,
    pub flags: u16,
    /// OSC 8 hyperlink id (index into `Term::links`), 0 = none. Lives in what
    /// would otherwise be padding, so a cell stays 16 bytes.
    pub link: u16,
}

impl Default for Cell {
    fn default() -> Self {
        Cell { c: ' ', fg: Color::DEFAULT, bg: Color::DEFAULT, flags: 0, link: 0 }
    }
}

impl Cell {
    fn blank(bg: Color) -> Cell {
        Cell { bg, ..Default::default() }
    }
}

#[derive(Clone)]
pub struct Row {
    pub cells: Vec<Cell>,
    /// The line continues on the next row (soft wrap).
    pub wrapped: bool,
    /// Cells at or beyond this column are known to be default blanks, so a
    /// reset only has to clear `[..occ]`. This makes scrolling O(line length).
    occ: usize,
}

impl Row {
    fn new(cols: usize, bg: Color) -> Row {
        let occ = if bg == Color::DEFAULT { 0 } else { cols };
        Row { cells: vec![Cell::blank(bg); cols], wrapped: false, occ }
    }
    fn reset(&mut self, bg: Color) {
        if bg == Color::DEFAULT {
            let occ = self.occ.min(self.cells.len());
            self.cells[..occ].fill(Cell::default());
            self.occ = 0;
        } else {
            self.cells.fill(Cell::blank(bg));
            self.occ = self.cells.len();
        }
        self.wrapped = false;
    }
    #[inline]
    fn touch(&mut self, end: usize) {
        if end > self.occ {
            self.occ = end;
        }
    }
}

// ---------------------------------------------------------------------------
// Grid with scrollback
// ---------------------------------------------------------------------------

pub struct Grid {
    /// Scrollback lines followed by `rows` screen lines.
    pub lines: VecDeque<Row>,
    pub rows: usize,
    pub cols: usize,
    max_scrollback: usize,
    /// Total number of lines ever dropped off the top of the scrollback,
    /// used to keep absolute line numbers (for selections) stable.
    pub dropped: u64,
}

impl Grid {
    fn new(cols: usize, rows: usize, max_scrollback: usize) -> Grid {
        let mut lines = VecDeque::with_capacity(rows + max_scrollback.min(4096));
        for _ in 0..rows {
            lines.push_back(Row::new(cols, Color::DEFAULT));
        }
        Grid { lines, rows, cols, max_scrollback, dropped: 0 }
    }

    #[inline]
    pub fn scrollback_len(&self) -> usize {
        self.lines.len() - self.rows
    }

    #[inline]
    pub fn row_mut(&mut self, y: usize) -> &mut Row {
        let base = self.lines.len() - self.rows;
        &mut self.lines[base + y]
    }

    /// Row visible at screen line `y` when the view is scrolled back by `offset`.
    #[inline]
    pub fn view_row(&self, y: usize, offset: usize) -> &Row {
        &self.lines[self.lines.len() - self.rows - offset + y]
    }

    /// Scroll region `[top, bottom)` up by `n` lines.
    fn scroll_up(&mut self, top: usize, bottom: usize, n: usize, bg: Color) {
        let n = n.min(bottom - top);
        if n == 0 {
            return;
        }
        if top == 0 && bottom == self.rows && self.max_scrollback > 0 {
            // Full-screen scroll: old lines move into scrollback.
            for _ in 0..n {
                let row = if self.scrollback_len() >= self.max_scrollback {
                    self.dropped += 1;
                    let mut r = self.lines.pop_front().unwrap();
                    r.cells.resize(self.cols, Cell::default());
                    r.reset(bg);
                    r
                } else {
                    Row::new(self.cols, bg)
                };
                self.lines.push_back(row);
            }
            return;
        }
        let base = self.lines.len() - self.rows;
        for _ in 0..n {
            let mut r = self.lines.remove(base + top).unwrap();
            r.reset(bg);
            self.lines.insert(base + bottom - 1, r);
        }
    }

    /// Scroll region `[top, bottom)` down by `n` lines.
    fn scroll_down(&mut self, top: usize, bottom: usize, n: usize, bg: Color) {
        let n = n.min(bottom - top);
        let base = self.lines.len() - self.rows;
        for _ in 0..n {
            let mut r = self.lines.remove(base + bottom - 1).unwrap();
            r.reset(bg);
            self.lines.insert(base + top, r);
        }
    }

    fn clear_scrollback(&mut self) {
        let n = self.scrollback_len();
        self.lines.drain(..n);
        self.dropped += n as u64;
    }
}

// ---------------------------------------------------------------------------
// Terminal
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CursorShape {
    Block,
    Underline,
    Bar,
}

#[derive(Clone, Copy, Default)]
struct SavedCursor {
    x: usize,
    y: usize,
    tmpl: Template,
    origin: bool,
    charsets: [bool; 2],
    gl: usize,
}

#[derive(Clone, Copy, Default, PartialEq)]
struct Template {
    fg: Color,
    bg: Color,
    flags: u16,
    link: u16,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MouseMode {
    Off,
    Click,  // 1000
    Drag,   // 1002
    Motion, // 1003
}

/// Theme colors as 0xRRGGBB. Applications may change them (OSC 4/10/11/12).
#[derive(Clone, Copy, PartialEq)]
pub struct Colors {
    pub palette: [u32; 256],
    pub fg: u32,
    pub bg: u32,
    pub cursor: u32,
}

impl Default for Colors {
    fn default() -> Self {
        let mut palette = [0u32; 256];
        for (i, p) in palette.iter_mut().enumerate() {
            *p = (i as u32) * 0x010101;
        }
        Colors { palette, fg: 0xd8d8d8, bg: 0x181818, cursor: 0xe0e0e0 }
    }
}

/// A search hit on one line: columns `start..=end`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Match {
    pub line: i64,
    pub start: usize,
    pub end: usize,
}

/// A selection in absolute line coordinates (line = grid.dropped + index into lines).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Selection {
    pub anchor: (i64, usize),
    pub head: (i64, usize),
}

impl Selection {
    pub fn ordered(&self) -> ((i64, usize), (i64, usize)) {
        if self.anchor <= self.head { (self.anchor, self.head) } else { (self.head, self.anchor) }
    }
}

pub struct Term {
    pub grid: Grid,
    alt_grid: Grid,
    pub alt_active: bool,

    pub cx: usize,
    pub cy: usize,
    pending_wrap: bool,
    tmpl: Template,
    saved: [SavedCursor; 2],

    scroll_top: usize,
    scroll_bottom: usize,
    tabs: Vec<bool>,

    // Character sets: G0/G1, true = DEC special graphics.
    charsets: [bool; 2],
    gl: usize,

    // Modes
    pub app_cursor: bool,
    pub app_keypad: bool,
    autowrap: bool,
    insert: bool,
    origin: bool,
    linefeed_newline: bool,
    pub cursor_visible: bool,
    /// Cursor shape requested by the application (DECSCUSR); None = user default.
    pub cursor_shape: Option<CursorShape>,
    pub bracketed_paste: bool,
    pub mouse_mode: MouseMode,
    pub sgr_mouse: bool,
    pub focus_events: bool,
    pub alternate_scroll: bool,

    pub title: String,
    pub title_changed: bool,
    pub bell: bool,

    /// How many lines the view is scrolled back into history.
    pub display_offset: usize,
    pub selection: Option<Selection>,

    /// Bytes to send back to the application (DA / DSR replies).
    pub responses: Vec<u8>,

    pub colors: Colors,
    base_colors: Colors,
    /// OSC 8 hyperlink targets; index 0 is "no link".
    pub links: Vec<String>,
    link_ids: HashMap<String, u16>,
    /// Working directory reported by the shell (OSC 7).
    pub cwd: Option<String>,
    /// Text the application asked to put on the clipboard (OSC 52).
    pub clipboard_request: Option<String>,
    /// Synchronized output (mode 2026): when it began, if active.
    pub sync_since: Option<Instant>,
    /// Incremented on every visible change.
    pub generation: u64,
    last_printed: char,
}

const SCROLLBACK: usize = 10_000;

impl Term {
    pub fn new(cols: usize, rows: usize) -> Term {
        let cols = cols.max(2);
        let rows = rows.max(1);
        Term {
            grid: Grid::new(cols, rows, SCROLLBACK),
            alt_grid: Grid::new(cols, rows, 0),
            alt_active: false,
            cx: 0,
            cy: 0,
            pending_wrap: false,
            tmpl: Template::default(),
            saved: [SavedCursor::default(); 2],
            scroll_top: 0,
            scroll_bottom: rows,
            tabs: default_tabs(cols),
            charsets: [false; 2],
            gl: 0,
            app_cursor: false,
            app_keypad: false,
            autowrap: true,
            insert: false,
            origin: false,
            linefeed_newline: false,
            cursor_visible: true,
            cursor_shape: None,
            bracketed_paste: false,
            mouse_mode: MouseMode::Off,
            sgr_mouse: false,
            focus_events: false,
            alternate_scroll: true,
            title: String::from("rsterm"),
            title_changed: true,
            bell: false,
            display_offset: 0,
            selection: None,
            responses: Vec::new(),
            colors: Colors::default(),
            base_colors: Colors::default(),
            links: vec![String::new()],
            link_ids: HashMap::new(),
            cwd: None,
            clipboard_request: None,
            sync_since: None,
            generation: 1,
            last_printed: ' ',
        }
    }

    #[inline]
    pub fn cols(&self) -> usize {
        self.grid.cols
    }
    #[inline]
    pub fn rows(&self) -> usize {
        self.grid.rows
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        let cols = cols.max(2);
        let rows = rows.max(1);
        if cols == self.cols() && rows == self.rows() {
            return;
        }
        // Main grid keeps the cursor's line on screen by pushing/pulling scrollback.
        let cy = if self.alt_active { self.saved[0].y } else { self.cy };
        let new_cy = resize_grid(&mut self.grid, cols, rows, cy, true);
        if self.alt_active {
            self.saved[0].y = new_cy;
            self.cy = resize_grid(&mut self.alt_grid, cols, rows, self.cy, false);
        } else {
            self.cy = new_cy;
            resize_grid(&mut self.alt_grid, cols, rows, 0, false);
        }
        self.cx = self.cx.min(cols - 1);
        self.cy = self.cy.min(rows - 1);
        for s in &mut self.saved {
            s.x = s.x.min(cols - 1);
            s.y = s.y.min(rows - 1);
        }
        self.pending_wrap = false;
        self.scroll_top = 0;
        self.scroll_bottom = rows;
        self.tabs = default_tabs(cols);
        self.display_offset = self.display_offset.min(self.grid.scrollback_len());
        self.selection = None;
        self.dirty();
    }

    #[inline]
    fn dirty(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }

    #[inline]
    fn active(&mut self) -> &mut Grid {
        if self.alt_active { &mut self.alt_grid } else { &mut self.grid }
    }

    /// The grid that is currently shown.
    #[inline]
    pub fn screen(&self) -> &Grid {
        if self.alt_active { &self.alt_grid } else { &self.grid }
    }

    pub fn scroll_view(&mut self, delta: isize) {
        let max = self.screen().scrollback_len() as isize;
        let new = (self.display_offset as isize + delta).clamp(0, max) as usize;
        if new != self.display_offset {
            self.display_offset = new;
            self.dirty();
        }
    }

    pub fn reset_view(&mut self) {
        if self.display_offset != 0 {
            self.display_offset = 0;
            self.dirty();
        }
    }

    // -- selection helpers ---------------------------------------------------

    /// Absolute line number of the view row `y`.
    pub fn abs_line(&self, y: usize) -> i64 {
        let g = self.screen();
        (g.dropped + (g.lines.len() - g.rows - self.display_offset + y) as u64) as i64
    }

    pub fn set_selection(&mut self, sel: Option<Selection>) {
        if self.selection != sel {
            self.selection = sel;
            self.dirty();
        }
    }

    pub fn selection_text(&self) -> Option<String> {
        let sel = self.selection?;
        let ((l0, c0), (l1, c1)) = sel.ordered();
        let g = self.screen();
        let mut out = String::new();
        for line in l0..=l1 {
            let idx = line - g.dropped as i64;
            if idx < 0 || idx as usize >= g.lines.len() {
                continue;
            }
            let row = &g.lines[idx as usize];
            let start = if line == l0 { c0 } else { 0 };
            let end = if line == l1 { (c1 + 1).min(g.cols) } else { g.cols };
            let mut s = String::new();
            for cell in &row.cells[start.min(g.cols)..end] {
                if cell.flags & flags::WIDE_SPACER == 0 {
                    s.push(cell.c);
                }
            }
            if line != l1 && !row.wrapped {
                out.push_str(s.trim_end());
                out.push('\n');
            } else if line == l1 && end == g.cols {
                out.push_str(s.trim_end());
            } else {
                out.push_str(&s);
            }
        }
        Some(out)
    }

    pub fn set_scrollback(&mut self, n: usize) {
        self.grid.max_scrollback = n;
        let excess = self.grid.scrollback_len().saturating_sub(n);
        if excess > 0 {
            self.grid.lines.drain(..excess);
            self.grid.dropped += excess as u64;
            self.display_offset = self.display_offset.min(self.grid.scrollback_len());
        }
        self.dirty();
    }

    /// Apply theme colors (also the target of OSC 104/110/111/112 resets).
    pub fn set_colors(&mut self, c: Colors) {
        self.colors = c;
        self.base_colors = c;
        self.dirty();
    }

    fn intern_link(&mut self, uri: String) -> u16 {
        if let Some(&id) = self.link_ids.get(&uri) {
            return id;
        }
        if self.links.len() >= u16::MAX as usize {
            return 0;
        }
        let id = self.links.len() as u16;
        self.links.push(uri.clone());
        self.link_ids.insert(uri, id);
        id
    }

    /// Row at an absolute line number on the shown screen, if it still exists.
    pub fn abs_row(&self, line: i64) -> Option<&Row> {
        let g = self.screen();
        let idx = line - g.dropped as i64;
        (idx >= 0).then(|| g.lines.get(idx as usize)).flatten()
    }

    /// Absolute line numbers of the first and last line on the shown screen.
    pub fn line_range(&self) -> (i64, i64) {
        let g = self.screen();
        (g.dropped as i64, g.dropped as i64 + g.lines.len() as i64 - 1)
    }

    /// Scroll the view so that absolute `line` is visible (centered if it wasn't).
    pub fn scroll_to_line(&mut self, line: i64) {
        let g = self.screen();
        let idx = (line - g.dropped as i64).clamp(0, g.lines.len() as i64 - 1) as usize;
        let top = g.lines.len() - g.rows - self.display_offset;
        if idx >= top && idx < top + g.rows {
            return;
        }
        let want_top = idx.saturating_sub(g.rows / 2).min(g.lines.len() - g.rows);
        self.display_offset = g.lines.len() - g.rows - want_top;
        self.dirty();
    }

    /// Case-insensitive unless the query has uppercase ("smart case").
    pub fn search(&self, query: &str) -> Vec<Match> {
        let fold = !query.chars().any(char::is_uppercase);
        let q: Vec<char> =
            query.chars().map(|c| if fold { c.to_lowercase().next().unwrap_or(c) } else { c }).collect();
        let mut out = Vec::new();
        if q.is_empty() {
            return out;
        }
        let g = self.screen();
        let mut chars = Vec::with_capacity(g.cols);
        let mut cols = Vec::with_capacity(g.cols);
        for (i, row) in g.lines.iter().enumerate() {
            chars.clear();
            cols.clear();
            for (x, cell) in row.cells.iter().enumerate() {
                if cell.flags & flags::WIDE_SPACER != 0 {
                    continue;
                }
                let c = cell.c;
                chars.push(if fold { c.to_lowercase().next().unwrap_or(c) } else { c });
                cols.push(x);
            }
            if chars.len() < q.len() {
                continue;
            }
            let mut s = 0;
            while s + q.len() <= chars.len() {
                if chars[s..s + q.len()] == q[..] {
                    let last = cols[s + q.len() - 1];
                    let wide = row.cells[last].flags & flags::WIDE != 0;
                    out.push(Match {
                        line: g.dropped as i64 + i as i64,
                        start: cols[s],
                        end: last + wide as usize,
                    });
                    s += q.len();
                } else {
                    s += 1;
                }
            }
        }
        out
    }

    /// Column span of the word at (line, col), for double-click selection.
    pub fn word_bounds(&self, line: i64, col: usize) -> (usize, usize) {
        const SEP: &str = " \t'\"`()[]{}<>|,;:│";
        let Some(row) = self.abs_row(line) else { return (col, col) };
        let is_word = |x: usize| {
            let c = row.cells[x].c;
            row.cells[x].flags & flags::WIDE_SPACER != 0 || !SEP.contains(c)
        };
        let col = col.min(row.cells.len() - 1);
        if !is_word(col) {
            return (col, col);
        }
        let mut s = col;
        while s > 0 && is_word(s - 1) {
            s -= 1;
        }
        let mut e = col;
        while e + 1 < row.cells.len() && is_word(e + 1) {
            e += 1;
        }
        (s, e)
    }

    /// Absolute line span of the logical (soft-wrapped) line containing `line`.
    pub fn logical_line(&self, line: i64) -> (i64, i64) {
        let mut a = line;
        while self.abs_row(a - 1).is_some_and(|r| r.wrapped) {
            a -= 1;
        }
        let mut b = line;
        while self.abs_row(b).is_some_and(|r| r.wrapped) && self.abs_row(b + 1).is_some() {
            b += 1;
        }
        (a, b)
    }

    pub fn select_all(&mut self) {
        let (a, b) = self.line_range();
        let cols = self.cols();
        self.set_selection(Some(Selection { anchor: (a, 0), head: (b, cols - 1) }));
    }

    /// Hyperlink (OSC 8) or plain-text URL under (line, col): (url, start col, end col).
    pub fn url_at(&self, line: i64, col: usize) -> Option<(String, usize, usize)> {
        let row = self.abs_row(line)?;
        let col = col.min(row.cells.len() - 1);
        let id = row.cells[col].link;
        if id != 0 {
            let mut s = col;
            while s > 0 && row.cells[s - 1].link == id {
                s -= 1;
            }
            let mut e = col;
            while e + 1 < row.cells.len() && row.cells[e + 1].link == id {
                e += 1;
            }
            return Some((self.links.get(id as usize)?.clone(), s, e));
        }
        // Plain-text detection: expand to a whitespace-delimited token.
        let stop = |c: char| c.is_whitespace() || "<>\"`{}|^│".contains(c);
        let cells = &row.cells;
        if stop(cells[col].c) {
            return None;
        }
        let mut s = col;
        while s > 0 && !stop(cells[s - 1].c) {
            s -= 1;
        }
        let mut e = col;
        while e + 1 < cells.len() && !stop(cells[e + 1].c) {
            e += 1;
        }
        let token: String = cells[s..=e].iter().map(|c| c.c).collect();
        // Find a scheme in the token (ASCII-only offsets map 1:1 to columns).
        if !token.is_ascii() {
            return None;
        }
        let schemes = ["https://", "http://", "file://", "ftp://", "mailto:", "ssh://", "git://"];
        let (off, _) = schemes.iter().filter_map(|p| token.find(p).map(|i| (i, p))).min_by_key(|(i, _)| *i)?;
        let mut url = &token[off..];
        // Trim trailing punctuation, and an unbalanced closing paren/bracket.
        loop {
            let t = url.trim_end_matches(['.', ',', ';', ':', '!', '?', '\'']);
            let t = if (t.ends_with(')') && t.matches('(').count() < t.matches(')').count())
                || (t.ends_with(']') && t.matches('[').count() < t.matches(']').count())
            {
                &t[..t.len() - 1]
            } else {
                t
            };
            if t.len() == url.len() {
                break;
            }
            url = t;
        }
        let (us, ue) = (s + off, s + off + url.len() - 1);
        (col >= us && col <= ue && url.len() > 8).then(|| (url.to_string(), us, ue))
    }

    // -- core operations -----------------------------------------------------

    fn put_char(&mut self, c: char) {
        let c = if self.charsets[self.gl] { dec_special(c) } else { c };
        let width = match c.width() {
            Some(w) if w > 0 => w,
            _ => return, // combining / zero width / control: not rendered
        };
        self.last_printed = c;
        let cols = self.cols();

        if self.pending_wrap && self.autowrap {
            let y = self.cy;
            self.active().row_mut(y).wrapped = true;
            self.cx = 0;
            self.linefeed();
        }
        self.pending_wrap = false;

        if width == 2 && self.cx + 1 >= cols {
            if self.autowrap {
                let (x, y) = (self.cx, self.cy);
                let bg = self.tmpl.bg;
                let row = self.active().row_mut(y);
                row.cells[x] = Cell::blank(bg);
                row.touch(x + 1);
                row.wrapped = true;
                self.cx = 0;
                self.linefeed();
            } else {
                return;
            }
        }

        let (x, y) = (self.cx, self.cy);
        let tmpl = self.tmpl;
        let insert = self.insert;
        let row = self.active().row_mut(y);
        row.touch(if insert { cols } else { (x + width + 1).min(cols) });

        if insert {
            let blank = Cell::blank(tmpl.bg);
            let cells = &mut row.cells;
            cells[x..].rotate_right(width.min(cols - x));
            for c in &mut cells[x..(x + width).min(cols)] {
                *c = blank;
            }
            if cells[cols - 1].flags & flags::WIDE != 0 {
                cells[cols - 1] = blank;
            }
        }

        // Fix up any wide character we partially overwrite.
        fix_wide_overwrite(&mut row.cells, x, width, tmpl.bg);

        let f = tmpl.flags;
        if width == 1 {
            row.cells[x] = Cell { c, fg: tmpl.fg, bg: tmpl.bg, flags: f, link: tmpl.link };
        } else {
            row.cells[x] = Cell { c, fg: tmpl.fg, bg: tmpl.bg, flags: f | flags::WIDE, link: tmpl.link };
            row.cells[x + 1] =
                Cell { c: ' ', fg: tmpl.fg, bg: tmpl.bg, flags: f | flags::WIDE_SPACER, link: tmpl.link };
        }

        if x + width >= cols {
            self.cx = cols - 1;
            self.pending_wrap = true;
        } else {
            self.cx = x + width;
        }
    }

    fn linefeed(&mut self) {
        if self.cy + 1 == self.scroll_bottom {
            let (t, b, bg) = (self.scroll_top, self.scroll_bottom, self.tmpl.bg);
            self.active().scroll_up(t, b, 1, bg);
            // Keep a scrolled-back view anchored on the same content.
            if self.display_offset > 0 && !self.alt_active && t == 0 && b == self.rows() {
                self.display_offset = (self.display_offset + 1).min(self.grid.scrollback_len());
            }
        } else if self.cy + 1 < self.rows() {
            self.cy += 1;
        }
    }

    fn reverse_index(&mut self) {
        if self.cy == self.scroll_top {
            let (t, b, bg) = (self.scroll_top, self.scroll_bottom, self.tmpl.bg);
            self.active().scroll_down(t, b, 1, bg);
        } else if self.cy > 0 {
            self.cy -= 1;
        }
    }

    fn goto(&mut self, x: usize, y: usize) {
        let (min_y, max_y) =
            if self.origin { (self.scroll_top, self.scroll_bottom - 1) } else { (0, self.rows() - 1) };
        self.cx = x.min(self.cols() - 1);
        self.cy = (y + if self.origin { self.scroll_top } else { 0 }).clamp(min_y, max_y);
        self.pending_wrap = false;
    }

    fn goto_x(&mut self, x: usize) {
        self.cx = x.min(self.cols() - 1);
        self.pending_wrap = false;
    }

    fn move_up(&mut self, n: usize) {
        let min = if self.cy >= self.scroll_top { self.scroll_top } else { 0 };
        self.cy = self.cy.saturating_sub(n).max(min);
        self.pending_wrap = false;
    }

    fn move_down(&mut self, n: usize) {
        let max = if self.cy < self.scroll_bottom { self.scroll_bottom - 1 } else { self.rows() - 1 };
        self.cy = (self.cy + n).min(max);
        self.pending_wrap = false;
    }

    fn clear_cells(&mut self, y: usize, x0: usize, x1: usize) {
        let bg = self.tmpl.bg;
        let cols = self.cols();
        let row = self.active().row_mut(y);
        let (x0, x1) = (x0.min(cols), x1.min(cols));
        if x0 >= x1 {
            return;
        }
        // Don't leave half wide chars at the edges.
        if row.cells[x0].flags & flags::WIDE_SPACER != 0 && x0 > 0 {
            row.cells[x0 - 1] = Cell::blank(bg);
        }
        if x1 < cols && row.cells[x1].flags & flags::WIDE_SPACER != 0 {
            row.cells[x1] = Cell::blank(bg);
        }
        if bg != Color::DEFAULT {
            row.touch(x1);
        }
        row.cells[x0..x1].fill(Cell::blank(bg));
        if x1 == cols {
            row.wrapped = false;
        }
    }

    fn erase_display(&mut self, mode: u16) {
        let (rows, cols) = (self.rows(), self.cols());
        match mode {
            0 => {
                self.clear_cells(self.cy, self.cx, cols);
                for y in self.cy + 1..rows {
                    self.clear_cells(y, 0, cols);
                }
            }
            1 => {
                for y in 0..self.cy {
                    self.clear_cells(y, 0, cols);
                }
                self.clear_cells(self.cy, 0, self.cx + 1);
            }
            2 => {
                for y in 0..rows {
                    self.clear_cells(y, 0, cols);
                }
            }
            3 => {
                self.grid.clear_scrollback();
                self.display_offset = 0;
                self.selection = None;
            }
            _ => {}
        }
    }

    fn erase_line(&mut self, mode: u16) {
        let cols = self.cols();
        match mode {
            0 => self.clear_cells(self.cy, self.cx, cols),
            1 => self.clear_cells(self.cy, 0, self.cx + 1),
            2 => self.clear_cells(self.cy, 0, cols),
            _ => {}
        }
    }

    fn insert_chars(&mut self, n: usize) {
        let (x, y, cols, bg) = (self.cx, self.cy, self.cols(), self.tmpl.bg);
        let n = n.min(cols - x);
        let row = self.active().row_mut(y);
        row.touch(cols);
        fix_wide_overwrite(&mut row.cells, x, 1, bg);
        row.cells[x..].rotate_right(n);
        row.cells[x..x + n].fill(Cell::blank(bg));
        if row.cells[cols - 1].flags & flags::WIDE != 0 {
            row.cells[cols - 1] = Cell::blank(bg);
        }
        self.pending_wrap = false;
    }

    fn delete_chars(&mut self, n: usize) {
        let (x, y, cols, bg) = (self.cx, self.cy, self.cols(), self.tmpl.bg);
        let n = n.min(cols - x);
        let row = self.active().row_mut(y);
        row.touch(cols);
        fix_wide_overwrite(&mut row.cells, x, n, bg);
        row.cells[x..].rotate_left(n);
        row.cells[cols - n..].fill(Cell::blank(bg));
        self.pending_wrap = false;
    }

    fn insert_lines(&mut self, n: usize) {
        if self.cy < self.scroll_top || self.cy >= self.scroll_bottom {
            return;
        }
        let (y, b, bg) = (self.cy, self.scroll_bottom, self.tmpl.bg);
        self.active().scroll_down(y, b, n, bg);
        self.cx = 0;
        self.pending_wrap = false;
    }

    fn delete_lines(&mut self, n: usize) {
        if self.cy < self.scroll_top || self.cy >= self.scroll_bottom {
            return;
        }
        let (y, b, bg) = (self.cy, self.scroll_bottom, self.tmpl.bg);
        self.active().scroll_up(y, b, n, bg);
        self.cx = 0;
        self.pending_wrap = false;
    }

    fn save_cursor(&mut self) {
        let i = self.alt_active as usize;
        self.saved[i] = SavedCursor {
            x: self.cx,
            y: self.cy,
            tmpl: self.tmpl,
            origin: self.origin,
            charsets: self.charsets,
            gl: self.gl,
        };
    }

    fn restore_cursor(&mut self) {
        let s = self.saved[self.alt_active as usize];
        self.cx = s.x.min(self.cols() - 1);
        self.cy = s.y.min(self.rows() - 1);
        self.tmpl = s.tmpl;
        self.origin = s.origin;
        self.charsets = s.charsets;
        self.gl = s.gl;
        self.pending_wrap = false;
    }

    fn set_alt_screen(&mut self, on: bool, save: bool, clear: bool) {
        if on == self.alt_active {
            return;
        }
        if on {
            if save {
                self.save_cursor();
            }
            self.alt_active = true;
            if save {
                // keep cursor position in alt screen
                self.saved[1] = self.saved[0];
            }
            if clear {
                let bg = self.tmpl.bg;
                for r in self.alt_grid.lines.iter_mut() {
                    r.reset(bg);
                }
            }
        } else {
            self.alt_active = false;
            if save {
                self.restore_cursor();
            }
        }
        self.display_offset = 0;
        self.selection = None;
        self.scroll_top = 0;
        self.scroll_bottom = self.rows();
    }

    fn full_reset(&mut self) {
        let (c, r) = (self.cols(), self.rows());
        let title = std::mem::take(&mut self.title);
        let generation = self.generation;
        let (colors, base, sb) = (self.colors, self.base_colors, self.grid.max_scrollback);
        let cwd = self.cwd.take();
        *self = Term::new(c, r);
        self.title = title;
        self.generation = generation;
        self.colors = colors;
        self.base_colors = base;
        self.cwd = cwd;
        self.set_scrollback(sb);
    }

    fn set_mode(&mut self, private: bool, mode: u16, on: bool) {
        if !private {
            match mode {
                4 => self.insert = on,
                20 => self.linefeed_newline = on,
                _ => {}
            }
            return;
        }
        match mode {
            1 => self.app_cursor = on,
            3 => {
                // DECCOLM: we don't resize, but clear like xterm does.
                self.erase_display(2);
                self.goto(0, 0);
            }
            6 => {
                self.origin = on;
                self.goto(0, 0);
            }
            7 => self.autowrap = on,
            12 => {} // cursor blink
            25 => self.cursor_visible = on,
            47 => self.set_alt_screen(on, false, false),
            1047 => self.set_alt_screen(on, false, on),
            1048 => {
                if on {
                    self.save_cursor()
                } else {
                    self.restore_cursor()
                }
            }
            1049 => self.set_alt_screen(on, true, on),
            1000 => self.mouse_mode = if on { MouseMode::Click } else { MouseMode::Off },
            1002 => self.mouse_mode = if on { MouseMode::Drag } else { MouseMode::Off },
            1003 => self.mouse_mode = if on { MouseMode::Motion } else { MouseMode::Off },
            1004 => self.focus_events = on,
            1006 => self.sgr_mouse = on,
            1007 => self.alternate_scroll = on,
            2004 => self.bracketed_paste = on,
            2026 => self.sync_since = if on { Some(Instant::now()) } else { None },
            _ => {}
        }
    }

    /// Current state of a mode for DECRQM: Some(on) if recognized.
    fn get_mode(&self, private: bool, mode: u16) -> Option<bool> {
        if !private {
            return match mode {
                4 => Some(self.insert),
                20 => Some(self.linefeed_newline),
                _ => None,
            };
        }
        Some(match mode {
            1 => self.app_cursor,
            6 => self.origin,
            7 => self.autowrap,
            25 => self.cursor_visible,
            47 | 1047 | 1049 => self.alt_active,
            1000 => self.mouse_mode == MouseMode::Click,
            1002 => self.mouse_mode == MouseMode::Drag,
            1003 => self.mouse_mode == MouseMode::Motion,
            1004 => self.focus_events,
            1006 => self.sgr_mouse,
            1007 => self.alternate_scroll,
            2004 => self.bracketed_paste,
            2026 => self.sync_since.is_some(),
            _ => return None,
        })
    }

    fn sgr(&mut self, params: &Params) {
        let mut it = params.iter();
        if params.is_empty() {
            self.tmpl = Template::default();
            return;
        }
        while let Some(p) = it.next() {
            let t = &mut self.tmpl;
            match p[0] {
                0 => *t = Template::default(),
                1 => t.flags |= flags::BOLD,
                2 => t.flags |= flags::DIM,
                3 => t.flags |= flags::ITALIC,
                4 => {
                    if p.get(1) == Some(&0) {
                        t.flags &= !flags::UNDERLINE
                    } else {
                        t.flags |= flags::UNDERLINE
                    }
                }
                7 => t.flags |= flags::INVERSE,
                8 => t.flags |= flags::HIDDEN,
                9 => t.flags |= flags::STRIKE,
                21 => t.flags |= flags::UNDERLINE,
                22 => t.flags &= !(flags::BOLD | flags::DIM),
                23 => t.flags &= !flags::ITALIC,
                24 => t.flags &= !flags::UNDERLINE,
                27 => t.flags &= !flags::INVERSE,
                28 => t.flags &= !flags::HIDDEN,
                29 => t.flags &= !flags::STRIKE,
                n @ 30..=37 => t.fg = Color::indexed((n - 30) as u8),
                38 => {
                    if let Some(c) = parse_ext_color(p, &mut it) {
                        self.tmpl.fg = c
                    }
                }
                39 => t.fg = Color::DEFAULT,
                n @ 40..=47 => t.bg = Color::indexed((n - 40) as u8),
                48 => {
                    if let Some(c) = parse_ext_color(p, &mut it) {
                        self.tmpl.bg = c
                    }
                }
                49 => t.bg = Color::DEFAULT,
                58 => {
                    let _ = parse_ext_color(p, &mut it); // underline color: ignored
                }
                n @ 90..=97 => t.fg = Color::indexed((n - 90 + 8) as u8),
                n @ 100..=107 => t.bg = Color::indexed((n - 100 + 8) as u8),
                _ => {}
            }
        }
    }
}

fn default_tabs(cols: usize) -> Vec<bool> {
    (0..cols).map(|i| i % 8 == 0 && i != 0).collect()
}

/// Clear halves of wide characters that are being partially overwritten by
/// writing `width` cells starting at `x`.
fn fix_wide_overwrite(cells: &mut [Cell], x: usize, width: usize, bg: Color) {
    let cols = cells.len();
    if cells[x].flags & flags::WIDE_SPACER != 0 && x > 0 {
        cells[x - 1] = Cell::blank(bg);
    }
    let end = (x + width).min(cols);
    if end > 0 && cells[end - 1].flags & flags::WIDE != 0 && end < cols {
        cells[end] = Cell::blank(bg);
    }
}

fn resize_grid(g: &mut Grid, cols: usize, rows: usize, cy: usize, keep_history: bool) -> usize {
    for r in g.lines.iter_mut() {
        if r.cells.len() > cols && r.cells[cols - 1].flags & flags::WIDE != 0 {
            r.cells[cols - 1] = Cell::default();
        }
        r.cells.resize(cols, Cell::default());
        r.occ = r.occ.min(cols);
    }
    g.cols = cols;
    let mut cy = cy;
    if rows < g.rows {
        let mut shrink = g.rows - rows;
        // First drop blank lines below the cursor.
        while shrink > 0 && g.rows - 1 > cy {
            let last = g.lines.len() - 1;
            if g.lines[last].cells.iter().all(|c| c.c == ' ' && c.bg == Color::DEFAULT) {
                g.lines.pop_back();
                g.rows -= 1;
                shrink -= 1;
            } else {
                break;
            }
        }
        // Remaining lines scroll off the top (into history on the main screen).
        g.rows -= shrink;
        cy = cy.saturating_sub(shrink);
        if !keep_history {
            g.lines.drain(..shrink);
        }
        // Lines below the cursor that didn't fit get dropped.
        while g.rows > rows {
            g.lines.pop_back();
            g.rows -= 1;
        }
    } else if rows > g.rows {
        let mut grow = rows - g.rows;
        if keep_history {
            let pull = grow.min(g.scrollback_len());
            g.rows += pull;
            cy += pull;
            grow -= pull;
        }
        for _ in 0..grow {
            g.lines.push_back(Row::new(cols, Color::DEFAULT));
            g.rows += 1;
        }
    }
    while g.scrollback_len() > g.max_scrollback {
        g.lines.pop_front();
        g.dropped += 1;
    }
    cy.min(rows - 1)
}

fn parse_ext_color(p: &[u16], it: &mut vte::ParamsIter<'_>) -> Option<Color> {
    // Colon form: 38:5:n or 38:2:[cs:]r:g:b
    if p.len() > 1 {
        return match p[1] {
            5 => p.get(2).map(|&n| Color::indexed(n as u8)),
            2 => {
                let v = if p.len() >= 6 { &p[3..6] } else { p.get(2..5)? };
                Some(Color::rgb(v[0] as u8, v[1] as u8, v[2] as u8))
            }
            _ => None,
        };
    }
    // Semicolon form: 38;5;n or 38;2;r;g;b
    match it.next()?[0] {
        5 => it.next().map(|n| Color::indexed(n[0] as u8)),
        2 => {
            let r = it.next()?[0];
            let g = it.next()?[0];
            let b = it.next()?[0];
            Some(Color::rgb(r as u8, g as u8, b as u8))
        }
        _ => None,
    }
}

/// Parse "rgb:r/g/b" (1-4 hex digits per channel) or "#rrggbb".
fn parse_color(spec: &[u8]) -> Option<u32> {
    let s = std::str::from_utf8(spec).ok()?;
    if let Some(hex) = s.strip_prefix('#') {
        if hex.len() == 6 {
            return u32::from_str_radix(hex, 16).ok();
        }
        return None;
    }
    let body = s.strip_prefix("rgb:")?;
    let mut out = 0u32;
    let mut n = 0;
    for part in body.split('/') {
        if part.is_empty() || part.len() > 4 {
            return None;
        }
        let v = u32::from_str_radix(part, 16).ok()?;
        let max = (1u32 << (4 * part.len())) - 1;
        out = out << 8 | (v * 255 + max / 2) / max;
        n += 1;
    }
    (n == 3).then_some(out)
}

fn xparse_fmt(c: u32) -> String {
    let ch = |s: u32| (c >> s & 0xff) * 0x101;
    format!("rgb:{:04x}/{:04x}/{:04x}", ch(16), ch(8), ch(0))
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn base64_decode(data: &[u8]) -> Option<Vec<u8>> {
    let val = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        } as u32)
    };
    let mut out = Vec::with_capacity(data.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0);
    for &c in data {
        if c == b'=' || c == b'\n' || c == b'\r' {
            continue;
        }
        acc = acc << 6 | val(c)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

fn dec_special(c: char) -> char {
    match c {
        '`' => '◆',
        'a' => '▒',
        'b' => '␉',
        'c' => '␌',
        'd' => '␍',
        'e' => '␊',
        'f' => '°',
        'g' => '±',
        'h' => '␤',
        'i' => '␋',
        'j' => '┘',
        'k' => '┐',
        'l' => '┌',
        'm' => '└',
        'n' => '┼',
        'o' => '⎺',
        'p' => '⎻',
        'q' => '─',
        'r' => '⎼',
        's' => '⎽',
        't' => '├',
        'u' => '┤',
        'v' => '┴',
        'w' => '┬',
        'x' => '│',
        'y' => '≤',
        'z' => '≥',
        '{' => 'π',
        '|' => '≠',
        '}' => '£',
        '~' => '·',
        _ => c,
    }
}

// ---------------------------------------------------------------------------
// vte::Perform
// ---------------------------------------------------------------------------

#[inline]
fn arg(params: &Params, i: usize, default: u16) -> u16 {
    match params.iter().nth(i).map(|p| p[0]) {
        Some(0) | None => default,
        Some(v) => v,
    }
}

impl Perform for Term {
    #[inline]
    fn print(&mut self, c: char) {
        // Fast path: printable ASCII, no pending wrap, not at the last column,
        // no insert mode / DEC graphics, not overwriting a wide char.
        let cu = c as u32;
        let x = self.cx;
        if (0x20..0x7f).contains(&cu)
            && !self.pending_wrap
            && !self.insert
            && !self.charsets[self.gl]
            && x + 1 < self.grid.cols
        {
            let y = self.cy;
            let tmpl = self.tmpl;
            let grid = if self.alt_active { &mut self.alt_grid } else { &mut self.grid };
            let idx = grid.lines.len() - grid.rows + y;
            let row = &mut grid.lines[idx];
            let cell = &mut row.cells[x];
            if cell.flags & (flags::WIDE | flags::WIDE_SPACER) == 0 {
                *cell = Cell { c, fg: tmpl.fg, bg: tmpl.bg, flags: tmpl.flags, link: tmpl.link };
                row.touch(x + 1);
                self.cx = x + 1;
                self.last_printed = c;
                self.generation = self.generation.wrapping_add(1);
                return;
            }
        }
        self.put_char(c);
        self.dirty();
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            0x07 => self.bell = true,
            0x08 => {
                if self.cx > 0 {
                    self.cx -= 1;
                }
                self.pending_wrap = false;
            }
            0x09 => {
                let cols = self.cols();
                let mut x = self.cx + 1;
                while x < cols && !self.tabs[x] {
                    x += 1;
                }
                self.cx = x.min(cols - 1);
            }
            0x0a..=0x0c => {
                self.linefeed();
                if self.linefeed_newline {
                    self.cx = 0;
                }
                self.pending_wrap = false;
            }
            0x0d => {
                self.cx = 0;
                self.pending_wrap = false;
            }
            0x0e => self.gl = 1,
            0x0f => self.gl = 0,
            _ => return,
        }
        self.dirty();
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], bell_terminated: bool) {
        let Some(&cmd) = params.first() else { return };
        let st: &[u8] = if bell_terminated { b"\x07" } else { b"\x1b\\" };
        let rest = || params[1..].join(&b';');
        match cmd {
            b"0" | b"2" => {
                if params.len() > 1 {
                    self.title = String::from_utf8_lossy(&rest()).into_owned();
                    self.title_changed = true;
                }
            }
            b"4" => {
                for pair in params[1..].chunks(2) {
                    let [idx, spec] = pair else { break };
                    let Some(i) = std::str::from_utf8(idx).ok().and_then(|s| s.parse::<u8>().ok()) else {
                        continue;
                    };
                    if *spec == b"?" {
                        let c = self.colors.palette[i as usize];
                        self.responses.extend_from_slice(format!("\x1b]4;{i};{}", xparse_fmt(c)).as_bytes());
                        self.responses.extend_from_slice(st);
                    } else if let Some(c) = parse_color(spec) {
                        self.colors.palette[i as usize] = c;
                    }
                }
                self.dirty();
            }
            b"10" | b"11" | b"12" => {
                let Some(spec) = params.get(1) else { return };
                let which = cmd[1] - b'0';
                let slot = match which {
                    0 => &mut self.colors.fg,
                    1 => &mut self.colors.bg,
                    _ => &mut self.colors.cursor,
                };
                if *spec == b"?" {
                    let c = *slot;
                    self.responses
                        .extend_from_slice(format!("\x1b]1{which};{}", xparse_fmt(c)).as_bytes());
                    self.responses.extend_from_slice(st);
                } else if let Some(c) = parse_color(spec) {
                    *slot = c;
                    self.dirty();
                }
            }
            b"104" => {
                if params.len() == 1 || params[1].is_empty() {
                    self.colors.palette = self.base_colors.palette;
                } else {
                    for idx in &params[1..] {
                        if let Some(i) = std::str::from_utf8(idx).ok().and_then(|s| s.parse::<u8>().ok()) {
                            self.colors.palette[i as usize] = self.base_colors.palette[i as usize];
                        }
                    }
                }
                self.dirty();
            }
            b"110" => {
                self.colors.fg = self.base_colors.fg;
                self.dirty();
            }
            b"111" => {
                self.colors.bg = self.base_colors.bg;
                self.dirty();
            }
            b"112" => {
                self.colors.cursor = self.base_colors.cursor;
                self.dirty();
            }
            b"7" => {
                // file://host/path (percent-encoded)
                let uri = String::from_utf8_lossy(&rest()).into_owned();
                if let Some(after) = uri.strip_prefix("file://") {
                    let path = after.find('/').map_or("", |i| &after[i..]);
                    if !path.is_empty() {
                        self.cwd = Some(percent_decode(path));
                    }
                }
            }
            b"8" => {
                // 8 ; params ; uri   (uri may itself contain ';')
                let uri = if params.len() > 2 { params[2..].join(&b';') } else { Vec::new() };
                self.tmpl.link = if uri.is_empty() {
                    0
                } else {
                    let uri = String::from_utf8_lossy(&uri).into_owned();
                    self.intern_link(uri)
                };
            }
            b"52" => {
                // 52 ; selection ; base64-data   ("?" = query, which we refuse)
                if let Some(data) = params.get(2) {
                    if *data != b"?" {
                        if let Some(bytes) = base64_decode(data) {
                            self.clipboard_request = Some(String::from_utf8_lossy(&bytes).into_owned());
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, inter: &[u8], ignore: bool, action: char) {
        if ignore {
            return;
        }
        let private = inter.first() == Some(&b'?');
        let a0 = arg(params, 0, 1) as usize;
        match (action, inter) {
            ('@', []) => self.insert_chars(a0),
            ('A', []) => self.move_up(a0),
            ('B', []) | ('e', []) => self.move_down(a0),
            ('C', []) | ('a', []) => {
                let x = self.cx + a0;
                self.goto_x(x)
            }
            ('D', []) => {
                let x = self.cx.saturating_sub(a0);
                self.goto_x(x)
            }
            ('E', []) => {
                self.move_down(a0);
                self.cx = 0
            }
            ('F', []) => {
                self.move_up(a0);
                self.cx = 0
            }
            ('G', []) | ('`', []) => self.goto_x(a0 - 1),
            ('H', []) | ('f', []) => {
                let y = arg(params, 0, 1) as usize - 1;
                let x = arg(params, 1, 1) as usize - 1;
                self.goto(x, y)
            }
            ('I', []) => {
                for _ in 0..a0 {
                    self.execute(0x09)
                }
            }
            ('J', []) | ('J', [b'?']) => self.erase_display(arg(params, 0, 0)),
            ('K', []) | ('K', [b'?']) => self.erase_line(arg(params, 0, 0)),
            ('L', []) => self.insert_lines(a0),
            ('M', []) => self.delete_lines(a0),
            ('P', []) => self.delete_chars(a0),
            ('S', []) => {
                let (t, b, bg) = (self.scroll_top, self.scroll_bottom, self.tmpl.bg);
                self.active().scroll_up(t, b, a0, bg)
            }
            ('T', []) => {
                let (t, b, bg) = (self.scroll_top, self.scroll_bottom, self.tmpl.bg);
                self.active().scroll_down(t, b, a0, bg)
            }
            ('X', []) => {
                let (y, x) = (self.cy, self.cx);
                self.clear_cells(y, x, x + a0)
            }
            ('Z', []) => {
                for _ in 0..a0 {
                    let mut x = self.cx;
                    while x > 0 {
                        x -= 1;
                        if self.tabs[x] {
                            break;
                        }
                    }
                    self.cx = x;
                }
            }
            ('b', []) => {
                let c = self.last_printed;
                for _ in 0..a0.min(65535) {
                    self.put_char(c)
                }
            }
            ('c', []) => {
                if arg(params, 0, 0) == 0 {
                    self.responses.extend_from_slice(b"\x1b[?62;22c");
                }
            }
            ('c', [b'>']) => self.responses.extend_from_slice(b"\x1b[>0;10;1c"),
            ('d', []) => {
                let x = self.cx;
                self.goto(x, a0 - 1)
            }
            ('g', []) => match arg(params, 0, 0) {
                0 => {
                    let x = self.cx;
                    self.tabs[x] = false
                }
                3 => self.tabs.fill(false),
                _ => {}
            },
            ('h', _) | ('l', _) => {
                let on = action == 'h';
                for p in params.iter() {
                    self.set_mode(private, p[0], on);
                }
            }
            ('m', []) => self.sgr(params),
            ('n', []) => match arg(params, 0, 0) {
                5 => self.responses.extend_from_slice(b"\x1b[0n"),
                6 => {
                    let y = if self.origin { self.cy - self.scroll_top } else { self.cy };
                    let r = format!("\x1b[{};{}R", y + 1, self.cx + 1);
                    self.responses.extend_from_slice(r.as_bytes());
                }
                _ => {}
            },
            ('n', [b'?']) => {
                if arg(params, 0, 0) == 6 {
                    let r = format!("\x1b[?{};{}R", self.cy + 1, self.cx + 1);
                    self.responses.extend_from_slice(r.as_bytes());
                }
            }
            ('p', [b'?', b'$']) | ('p', [b'$']) => {
                let mode = arg(params, 0, 0);
                let state = match self.get_mode(private, mode) {
                    Some(true) => 1,
                    Some(false) => 2,
                    None => 0,
                };
                let r = format!("\x1b[{}{};{}$y", if private { "?" } else { "" }, mode, state);
                self.responses.extend_from_slice(r.as_bytes());
            }
            ('p', [b'!']) => {
                // DECSTR soft reset
                self.tmpl = Template::default();
                self.cursor_visible = true;
                self.insert = false;
                self.origin = false;
                self.autowrap = true;
                self.app_cursor = false;
                self.app_keypad = false;
                self.scroll_top = 0;
                self.scroll_bottom = self.rows();
                self.charsets = [false; 2];
                self.gl = 0;
                self.pending_wrap = false;
            }
            ('q', [b' ']) => {
                self.cursor_shape = match arg(params, 0, 0) {
                    0 => None,
                    1 | 2 => Some(CursorShape::Block),
                    3 | 4 => Some(CursorShape::Underline),
                    _ => Some(CursorShape::Bar),
                }
            }
            ('r', []) => {
                let rows = self.rows();
                let top = arg(params, 0, 1) as usize - 1;
                let bottom = (arg(params, 1, rows as u16) as usize).min(rows);
                if top + 1 < bottom {
                    self.scroll_top = top;
                    self.scroll_bottom = bottom;
                    self.goto(0, 0);
                }
            }
            ('s', []) => self.save_cursor(),
            ('u', []) => self.restore_cursor(),
            ('t', []) => {
                if arg(params, 0, 0) == 18 {
                    let r = format!("\x1b[8;{};{}t", self.rows(), self.cols());
                    self.responses.extend_from_slice(r.as_bytes());
                }
            }
            _ => {}
        }
        self.dirty();
    }

    fn esc_dispatch(&mut self, inter: &[u8], ignore: bool, byte: u8) {
        if ignore {
            return;
        }
        match (inter, byte) {
            ([], b'7') => self.save_cursor(),
            ([], b'8') => self.restore_cursor(),
            ([], b'D') => self.linefeed(),
            ([], b'E') => {
                self.linefeed();
                self.cx = 0;
            }
            ([], b'H') => {
                let x = self.cx;
                self.tabs[x] = true
            }
            ([], b'M') => self.reverse_index(),
            ([], b'c') => self.full_reset(),
            ([], b'=') => self.app_keypad = true,
            ([], b'>') => self.app_keypad = false,
            ([b'('], b'0') => self.charsets[0] = true,
            ([b'('], _) => self.charsets[0] = false,
            ([b')'], b'0') => self.charsets[1] = true,
            ([b')'], _) => self.charsets[1] = false,
            ([b'#'], b'8') => {
                for y in 0..self.rows() {
                    let cols = self.cols();
                    self.active().row_mut(y).touch(cols);
                    for c in self.active().row_mut(y).cells.iter_mut() {
                        *c = Cell { c: 'E', ..Default::default() };
                    }
                }
            }
            _ => return,
        }
        self.pending_wrap = false;
        self.dirty();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn term(cols: usize, rows: usize) -> (Term, vte::Parser) {
        (Term::new(cols, rows), vte::Parser::new())
    }

    fn feed(t: &mut Term, p: &mut vte::Parser, s: &str) {
        p.advance(t, s.as_bytes());
    }

    fn line(t: &Term, y: usize) -> String {
        let g = t.screen();
        g.view_row(y, t.display_offset)
            .cells
            .iter()
            .filter(|c| c.flags & flags::WIDE_SPACER == 0)
            .map(|c| c.c)
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    #[test]
    fn print_and_newline() {
        let (mut t, mut p) = term(10, 3);
        feed(&mut t, &mut p, "hello\r\nworld");
        assert_eq!(line(&t, 0), "hello");
        assert_eq!(line(&t, 1), "world");
        assert_eq!((t.cx, t.cy), (5, 1));
    }

    #[test]
    fn autowrap_sets_wrapped_and_selection_joins() {
        let (mut t, mut p) = term(5, 3);
        feed(&mut t, &mut p, "abcdefg");
        assert_eq!(line(&t, 0), "abcde");
        assert_eq!(line(&t, 1), "fg");
        assert!(t.screen().view_row(0, 0).wrapped);
        let l0 = t.abs_line(0);
        t.set_selection(Some(Selection { anchor: (l0, 0), head: (l0 + 1, 4) }));
        assert_eq!(t.selection_text().unwrap(), "abcdefg");
    }

    #[test]
    fn pending_wrap_then_cr_does_not_wrap() {
        let (mut t, mut p) = term(5, 3);
        feed(&mut t, &mut p, "abcde\rX");
        assert_eq!(line(&t, 0), "Xbcde");
        assert_eq!(t.cy, 0);
    }

    #[test]
    fn scrolling_into_scrollback_and_row_reuse_is_clean() {
        let (mut t, mut p) = term(8, 2);
        // Fill far past the scrollback limit so rows get recycled.
        for _ in 0..(SCROLLBACK + 50) {
            feed(&mut t, &mut p, "xxxxxxxx\r\n");
        }
        feed(&mut t, &mut p, "ab");
        assert_eq!(line(&t, 1), "ab");
        assert_eq!(t.grid.scrollback_len(), SCROLLBACK);
        // Recycled row must be fully blank past what was written.
        assert!(t.grid.lines.back().unwrap().cells[2..].iter().all(|c| *c == Cell::default()));
    }

    #[test]
    fn scroll_region() {
        let (mut t, mut p) = term(5, 5);
        feed(&mut t, &mut p, "1\r\n2\r\n3\r\n4\r\n5");
        feed(&mut t, &mut p, "\x1b[2;4r\x1b[4;1H\nX");
        assert_eq!(
            (0..5).map(|y| line(&t, y)).collect::<Vec<_>>(),
            ["1", "3", "4", "X", "5"]
        );
        assert_eq!(t.grid.scrollback_len(), 0);
    }

    #[test]
    fn insert_delete_lines_and_chars() {
        let (mut t, mut p) = term(6, 3);
        feed(&mut t, &mut p, "abcdef\x1b[1;3H\x1b[2P");
        assert_eq!(line(&t, 0), "abef");
        feed(&mut t, &mut p, "\x1b[1;2H\x1b[2@");
        assert_eq!(line(&t, 0), "a  bef");
        feed(&mut t, &mut p, "\x1b[2;1Hrow2\x1b[1;1H\x1b[L");
        assert_eq!(line(&t, 0), "");
        assert_eq!(line(&t, 1), "a  bef");
        assert_eq!(line(&t, 2), "row2");
        feed(&mut t, &mut p, "\x1b[M");
        assert_eq!(line(&t, 0), "a  bef");
    }

    #[test]
    fn erase() {
        let (mut t, mut p) = term(6, 2);
        feed(&mut t, &mut p, "abcdef\r\nghijkl\x1b[1;3H\x1b[K");
        assert_eq!(line(&t, 0), "ab");
        feed(&mut t, &mut p, "\x1b[2J");
        assert_eq!(line(&t, 1), "");
    }

    #[test]
    fn wide_chars() {
        let (mut t, mut p) = term(5, 2);
        feed(&mut t, &mut p, "ab漢字");
        // 漢 fits at 2-3, 字 doesn't fit at col 4 -> wraps
        assert_eq!(line(&t, 0), "ab漢");
        assert_eq!(line(&t, 1), "字");
        // Overwriting the spacer half clears the wide char.
        feed(&mut t, &mut p, "\x1b[1;4HZ");
        assert_eq!(line(&t, 0), "ab Z");
    }

    #[test]
    fn alt_screen_restores() {
        let (mut t, mut p) = term(6, 2);
        feed(&mut t, &mut p, "main\x1b[?1049h");
        assert_eq!(line(&t, 0), "");
        feed(&mut t, &mut p, "alt\x1b[?1049l");
        assert_eq!(line(&t, 0), "main");
        assert_eq!((t.cx, t.cy), (4, 0));
    }

    #[test]
    fn sgr_colors() {
        let (mut t, mut p) = term(6, 1);
        feed(&mut t, &mut p, "\x1b[1;38;2;1;2;3;48:5:200mA\x1b[0mB");
        let c = t.screen().view_row(0, 0).cells[0];
        assert_eq!(c.fg, Color::rgb(1, 2, 3));
        assert_eq!(c.bg, Color::indexed(200));
        assert!(c.flags & flags::BOLD != 0);
        assert_eq!(t.screen().view_row(0, 0).cells[1].fg, Color::DEFAULT);
    }

    #[test]
    fn device_status_report() {
        let (mut t, mut p) = term(10, 5);
        feed(&mut t, &mut p, "\x1b[3;4H\x1b[6n\x1b[c");
        assert_eq!(t.responses, b"\x1b[3;4R\x1b[?62;22c");
    }

    #[test]
    fn resize_moves_lines_through_scrollback() {
        let (mut t, mut p) = term(10, 4);
        feed(&mut t, &mut p, "1\r\n2\r\n3\r\n4");
        t.resize(10, 2);
        assert_eq!((line(&t, 0), line(&t, 1)), ("3".into(), "4".into()));
        assert_eq!(t.cy, 1);
        t.resize(10, 4);
        assert_eq!(line(&t, 0), "1");
        assert_eq!(t.cy, 3);
    }

    #[test]
    fn dec_line_drawing() {
        let (mut t, mut p) = term(5, 1);
        feed(&mut t, &mut p, "\x1b(0lqk\x1b(Bq");
        assert_eq!(line(&t, 0), "┌─┐q");
    }
    #[test]
    fn osc8_hyperlinks_and_url_detection() {
        let (mut t, mut p) = term(60, 2);
        feed(&mut t, &mut p, "x \x1b]8;;https://example.com/a\x1b\\link\x1b]8;;\x1b\\ y");
        let l = t.abs_line(0);
        assert_eq!(t.url_at(l, 3), Some(("https://example.com/a".into(), 2, 5)));
        assert_eq!(t.url_at(l, 7), None);
        feed(&mut t, &mut p, "\r\nsee (https://rust-lang.org/learn), ok");
        let l = t.abs_line(1);
        assert_eq!(t.url_at(l, 10), Some(("https://rust-lang.org/learn".into(), 5, 31)));
    }

    #[test]
    fn osc_clipboard_cwd_and_color_query() {
        let (mut t, mut p) = term(20, 2);
        feed(&mut t, &mut p, "\x1b]52;c;aGVsbG8=\x07");
        assert_eq!(t.clipboard_request.as_deref(), Some("hello"));
        feed(&mut t, &mut p, "\x1b]7;file://host/home/me/my%20dir\x07");
        assert_eq!(t.cwd.as_deref(), Some("/home/me/my dir"));
        let mut c = Colors::default();
        c.bg = 0x102030;
        t.set_colors(c);
        feed(&mut t, &mut p, "\x1b]11;?\x07");
        assert_eq!(t.responses, b"\x1b]11;rgb:1010/2020/3030\x07");
        feed(&mut t, &mut p, "\x1b]11;#ffffff\x07\x1b]111\x07");
        assert_eq!(t.colors.bg, 0x102030);
    }

    #[test]
    fn search_and_word_bounds() {
        let (mut t, mut p) = term(20, 3);
        feed(&mut t, &mut p, "foo bar\r\nBar baz bar\r\n");
        let m = t.search("bar");
        assert_eq!(m.len(), 3);
        assert_eq!((m[0].start, m[0].end), (4, 6));
        assert_eq!(t.search("Bar").len(), 1); // smart case
        let l = t.abs_line(1);
        assert_eq!(t.word_bounds(l, 5), (4, 6));
    }

    #[test]
    fn decrqm_and_synchronized_output() {
        let (mut t, mut p) = term(10, 2);
        feed(&mut t, &mut p, "\x1b[?2026h");
        assert!(t.sync_since.is_some());
        feed(&mut t, &mut p, "\x1b[?2026$p\x1b[?2026l\x1b[?2026$p\x1b[?9999$p");
        assert_eq!(t.responses, b"\x1b[?2026;1$y\x1b[?2026;2$y\x1b[?9999;0$y");
        assert!(t.sync_since.is_none());
    }
}
