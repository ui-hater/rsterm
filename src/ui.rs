//! In-window UI drawn with the same cell renderer as the terminal: tab bar,
//! context menu, settings screen and search bar. Everything here is a grid
//! of cells painted over (or above) the terminal's cells, so the whole
//! window is still one instanced draw call.

use crate::config::{self, Config};
use crate::font::{Glyphs, STYLE_BOLD};
use crate::render::{CellInstance, pack};
use crate::term::Match;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    NewTab,
    CloseTab,
    NextTab,
    PrevTab,
    GotoTab(usize),
    Copy,
    Paste,
    SelectAll,
    Find,
    ZoomIn,
    ZoomOut,
    ZoomReset,
    Settings,
    ReloadConfig,
    ClearScrollback,
    OpenMenu,
    Quit,
}

pub struct MenuItem {
    pub label: &'static str,
    pub shortcut: &'static str,
    /// None = separator.
    pub action: Option<Action>,
}

const fn item(label: &'static str, shortcut: &'static str, a: Action) -> MenuItem {
    MenuItem { label, shortcut, action: Some(a) }
}
const SEP: MenuItem = MenuItem { label: "", shortcut: "", action: None };

pub const MENU: &[MenuItem] = &[
    item("New Tab", "Ctrl+Shift+T", Action::NewTab),
    item("Close Tab", "Ctrl+Shift+W", Action::CloseTab),
    item("Next Tab", "Ctrl+Tab", Action::NextTab),
    item("Previous Tab", "Ctrl+Shift+Tab", Action::PrevTab),
    SEP,
    item("Copy", "Ctrl+Shift+C", Action::Copy),
    item("Paste", "Ctrl+Shift+V", Action::Paste),
    item("Select All", "Ctrl+Shift+A", Action::SelectAll),
    item("Find…", "Ctrl+Shift+F", Action::Find),
    item("Clear Scrollback", "Ctrl+Shift+K", Action::ClearScrollback),
    SEP,
    item("Zoom In", "Ctrl+Shift+=", Action::ZoomIn),
    item("Zoom Out", "Ctrl+Shift+-", Action::ZoomOut),
    item("Reset Zoom", "Ctrl+Shift+0", Action::ZoomReset),
    SEP,
    item("Settings…", "Ctrl+,", Action::Settings),
    item("Reload Config", "Ctrl+Shift+,", Action::ReloadConfig),
    item("Quit", "Ctrl+Shift+Q", Action::Quit),
];

pub fn menu_step(sel: usize, dir: isize) -> usize {
    let n = MENU.len() as isize;
    let mut i = sel as isize;
    loop {
        i = (i + dir).rem_euclid(n);
        if MENU[i as usize].action.is_some() {
            return i as usize;
        }
    }
}

// ---------------------------------------------------------------------------
// Overlay state
// ---------------------------------------------------------------------------

pub struct MenuState {
    pub sel: usize,
    /// Requested top-left cell (window grid coordinates).
    pub at: (usize, usize),
}

pub struct SearchState {
    pub query: String,
    pub matches: Vec<Match>,
    pub current: Option<usize>,
}

impl SearchState {
    pub fn status(&self) -> String {
        match (self.current, self.matches.len()) {
            (_, 0) if self.query.is_empty() => "Type to search".into(),
            (_, 0) => "No matches".into(),
            (Some(i), n) => format!("Match {} of {}", i + 1, n),
            (None, n) => format!("{n} matches"),
        }
    }
}

pub enum Kind {
    Bool,
    Number { min: f64, max: f64, step: f64 },
    Choice(Vec<String>),
    Text,
}

pub struct Setting {
    pub key: &'static str,
    pub label: &'static str,
    pub kind: Kind,
}

pub struct SettingsState {
    pub sel: usize,
    pub items: Vec<Setting>,
    /// Text being edited for a `Kind::Text` item.
    pub editing: Option<String>,
    pub scroll: usize,
}

impl SettingsState {
    pub fn new() -> SettingsState {
        let s = |v: &[&str]| Kind::Choice(v.iter().map(|s| s.to_string()).collect());
        let items = vec![
            Setting { key: "theme", label: "Theme", kind: s(&config::THEMES.iter().map(|t| t.name).collect::<Vec<_>>()) },
            Setting { key: "font-family", label: "Font family", kind: Kind::Choice(config::monospace_families()) },
            Setting { key: "font-size", label: "Font size", kind: Kind::Number { min: 4.0, max: 96.0, step: 1.0 } },
            Setting { key: "cursor-style", label: "Cursor style", kind: s(&["block", "underline", "bar"]) },
            Setting { key: "cursor-style-blink", label: "Blinking cursor", kind: Kind::Bool },
            Setting { key: "scrollback-limit", label: "Scrollback lines", kind: Kind::Number { min: 0.0, max: 1_000_000.0, step: 1000.0 } },
            Setting { key: "window-padding", label: "Window padding", kind: Kind::Number { min: 0.0, max: 100.0, step: 1.0 } },
            Setting { key: "tab-bar", label: "Show tab bar", kind: s(&["auto", "always", "never"]) },
            Setting { key: "copy-on-select", label: "Copy on select", kind: Kind::Bool },
            Setting { key: "visual-bell", label: "Visual bell", kind: Kind::Bool },
            Setting { key: "bold-is-bright", label: "Bold text uses bright colors", kind: Kind::Bool },
            Setting { key: "window-inherit-working-directory", label: "New tabs open in current directory", kind: Kind::Bool },
            Setting { key: "clipboard-write", label: "Allow programs to set clipboard", kind: Kind::Bool },
            Setting { key: "command", label: "Command (empty = login shell)", kind: Kind::Text },
        ];
        SettingsState { sel: 0, items, editing: None, scroll: 0 }
    }

    /// Change the selected setting by `dir` steps (←/→). Returns true if changed.
    pub fn adjust(&self, cfg: &mut Config, dir: i32) -> bool {
        let it = &self.items[self.sel];
        let cur = cfg.get(it.key);
        let new = match &it.kind {
            Kind::Bool => (if cur == "true" { "false" } else { "true" }).to_string(),
            Kind::Number { min, max, step } => {
                let v: f64 = cur.parse().unwrap_or(*min);
                fmt_num((v + step * dir as f64).clamp(*min, *max))
            }
            Kind::Choice(opts) => {
                if opts.is_empty() {
                    return false;
                }
                let i = opts.iter().position(|o| o.eq_ignore_ascii_case(&cur)).map_or(0, |i| {
                    (i as i32 + dir).rem_euclid(opts.len() as i32) as usize
                });
                opts[i].clone()
            }
            Kind::Text => return false,
        };
        new != cur && cfg.set(it.key, &new).is_ok()
    }
}

pub fn fmt_num(v: f64) -> String {
    if v.fract() == 0.0 { format!("{}", v as i64) } else { format!("{v}") }
}

pub enum Overlay {
    None,
    Menu(MenuState),
    Search(SearchState),
    Settings(SettingsState),
}

// ---------------------------------------------------------------------------
// Colors
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
pub struct UiColors {
    pub fg: u32,
    pub panel: u32,
    pub dim: u32,
    pub accent_fg: u32,
    pub accent_bg: u32,
}

pub fn mix(a: u32, b: u32, t: f32) -> u32 {
    let ch = |s: u32| {
        let (x, y) = ((a >> s & 0xff) as f32, (b >> s & 0xff) as f32);
        ((x + (y - x) * t).round() as u32) << s
    };
    ch(16) | ch(8) | ch(0)
}

impl UiColors {
    pub fn new(fg: u32, bg: u32, sel_fg: u32, sel_bg: u32) -> UiColors {
        UiColors {
            fg,
            panel: mix(bg, fg, 0.10),
            dim: mix(fg, bg, 0.40),
            accent_fg: sel_fg,
            accent_bg: sel_bg,
        }
    }
}

// ---------------------------------------------------------------------------
// Layout
// ---------------------------------------------------------------------------

/// Rectangle in window grid cells: x, y, w, h.
pub type CellRect = (usize, usize, usize, usize);

pub struct TabBarLayout {
    /// (x start, width) per tab.
    pub tabs: Vec<(usize, usize)>,
    pub plus_x: usize,
}

pub fn tab_bar_layout(cols: usize, n: usize) -> TabBarLayout {
    let avail = cols.saturating_sub(4);
    let w = (avail / n.max(1)).clamp(4, 32);
    let tabs = (0..n).map(|i| (i * w, w)).take_while(|(x, w)| x + w <= avail).collect();
    TabBarLayout { tabs, plus_x: cols.saturating_sub(3) }
}

pub fn menu_rect(m: &MenuState, cols: usize, rows: usize) -> CellRect {
    let inner = MENU.iter().map(|i| i.label.chars().count() + i.shortcut.chars().count() + 4).max().unwrap_or(10);
    let w = (inner + 2).min(cols);
    let h = (MENU.len() + 2).min(rows);
    let x = m.at.0.min(cols - w);
    let y = m.at.1.min(rows - h);
    (x, y, w, h)
}

pub fn settings_rect(cols: usize, rows: usize) -> CellRect {
    let w = cols.min(72).max(cols.min(20));
    let h = rows.min(24);
    ((cols - w) / 2, (rows - h) / 2, w, h)
}

/// Rows available for setting items inside the panel.
pub fn settings_visible(h: usize) -> usize {
    h.saturating_sub(6).max(1)
}

// ---------------------------------------------------------------------------
// Painting
// ---------------------------------------------------------------------------

pub struct Painter<'a> {
    pub cells: &'a mut [CellInstance],
    pub cols: usize,
    pub rows: usize,
    pub glyphs: &'a mut Glyphs,
}

impl Painter<'_> {
    pub fn put(&mut self, x: usize, y: usize, c: char, fg: u32, bg: u32, style: u32) {
        if x >= self.cols || y >= self.rows {
            return;
        }
        let glyph = self.glyphs.get(c, style);
        self.cells[y * self.cols + x] = CellInstance { glyph, fg: pack(fg), bg: pack(bg), flags: 0 };
    }

    pub fn fill(&mut self, (x, y, w, h): CellRect, bg: u32) {
        for yy in y..(y + h).min(self.rows) {
            for xx in x..(x + w).min(self.cols) {
                self.cells[yy * self.cols + xx] = CellInstance { glyph: 0, fg: 0, bg: pack(bg), flags: 0 };
            }
        }
    }

    /// Write `s` starting at (x, y), clipped to `max` cells. Returns cells used.
    pub fn text(&mut self, x: usize, y: usize, s: &str, max: usize, fg: u32, bg: u32, style: u32) -> usize {
        let mut used = 0;
        let count = s.chars().count();
        for (i, c) in s.chars().enumerate() {
            let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
            if w == 0 {
                continue;
            }
            if used + w > max || (used + w == max && i + 1 < count && max > 1) {
                if used < max {
                    self.put(x + used, y, '…', fg, bg, style);
                    used += 1;
                }
                break;
            }
            if w == 2 {
                let g = self.glyphs.get(c, style);
                if x + used + 1 < self.cols && y < self.rows {
                    let i = y * self.cols + x + used;
                    self.cells[i] = CellInstance { glyph: g, fg: pack(fg), bg: pack(bg), flags: 0 };
                    let g2 = if g != 0 { g + self.glyphs.cell_w } else { 0 };
                    self.cells[i + 1] = CellInstance { glyph: g2, fg: pack(fg), bg: pack(bg), flags: 0 };
                }
            } else {
                self.put(x + used, y, c, fg, bg, style);
            }
            used += w;
        }
        used
    }

    pub fn frame(&mut self, (x, y, w, h): CellRect, fg: u32, bg: u32) {
        if w < 2 || h < 2 {
            return;
        }
        self.fill((x, y, w, h), bg);
        for xx in x + 1..x + w - 1 {
            self.put(xx, y, '─', fg, bg, 0);
            self.put(xx, y + h - 1, '─', fg, bg, 0);
        }
        for yy in y + 1..y + h - 1 {
            self.put(x, yy, '│', fg, bg, 0);
            self.put(x + w - 1, yy, '│', fg, bg, 0);
        }
        self.put(x, y, '╭', fg, bg, 0);
        self.put(x + w - 1, y, '╮', fg, bg, 0);
        self.put(x, y + h - 1, '╰', fg, bg, 0);
        self.put(x + w - 1, y + h - 1, '╯', fg, bg, 0);
    }

    pub fn tab_bar(&mut self, y: usize, titles: &[(String, bool, bool)], c: &UiColors) {
        let lay = tab_bar_layout(self.cols, titles.len());
        self.fill((0, y, self.cols, 1), c.panel);
        for (i, &(x, w)) in lay.tabs.iter().enumerate() {
            let (title, active, activity) = &titles[i];
            let (fg, bg) = if *active { (c.accent_fg, c.accent_bg) } else { (c.fg, c.panel) };
            self.fill((x, y, w, 1), bg);
            let mark = if *activity && !active { "•" } else { " " };
            let label = format!("{mark}{}: {title}", i + 1);
            self.text(x, y, &label, w.saturating_sub(1), fg, bg, if *active { STYLE_BOLD } else { 0 });
            if !active {
                self.put(x + w - 1, y, '│', c.dim, bg, 0);
            }
        }
        self.text(lay.plus_x, y, " + ", 3, c.fg, c.panel, STYLE_BOLD);
    }

    pub fn menu(&mut self, m: &MenuState, c: &UiColors) {
        let (x, y, w, h) = menu_rect(m, self.cols, self.rows);
        self.frame((x, y, w, h), c.dim, c.panel);
        for (i, it) in MENU.iter().enumerate().take(h.saturating_sub(2)) {
            let yy = y + 1 + i;
            if it.action.is_none() {
                for xx in x + 1..x + w - 1 {
                    self.put(xx, yy, '─', c.dim, c.panel, 0);
                }
                continue;
            }
            let (fg, bg) = if i == m.sel { (c.accent_fg, c.accent_bg) } else { (c.fg, c.panel) };
            self.fill((x + 1, yy, w - 2, 1), bg);
            self.text(x + 2, yy, it.label, w - 4, fg, bg, 0);
            let sc = it.shortcut.chars().count();
            self.text(x + w - 2 - sc, yy, it.shortcut, sc, if i == m.sel { fg } else { c.dim }, bg, 0);
        }
    }

    pub fn settings(&mut self, s: &mut SettingsState, cfg: &Config, c: &UiColors) {
        let (x, y, w, h) = settings_rect(self.cols, self.rows);
        self.frame((x, y, w, h), c.dim, c.panel);
        self.text(x + 2, y, " Settings ", w - 4, c.fg, c.panel, STYLE_BOLD);
        let vis = settings_visible(h);
        if s.sel < s.scroll {
            s.scroll = s.sel;
        } else if s.sel >= s.scroll + vis {
            s.scroll = s.sel + 1 - vis;
        }
        let label_w = (w.saturating_sub(4) / 2).min(36);
        for (row, i) in (s.scroll..s.items.len()).take(vis).enumerate() {
            let it = &s.items[i];
            let yy = y + 2 + row;
            let sel = i == s.sel;
            let (fg, bg) = if sel { (c.accent_fg, c.accent_bg) } else { (c.fg, c.panel) };
            self.fill((x + 1, yy, w - 2, 1), bg);
            self.text(x + 2, yy, it.label, label_w, fg, bg, 0);
            let value = match (&it.kind, &s.editing) {
                (Kind::Text, Some(e)) if sel => format!("{e}▏"),
                (Kind::Bool, _) => (if cfg.get(it.key) == "true" { "[x] on" } else { "[ ] off" }).to_string(),
                (Kind::Text, _) => {
                    let v = cfg.get(it.key);
                    if v.is_empty() { "(login shell)".into() } else { v }
                }
                _ => format!("‹ {} ›", cfg.get(it.key)),
            };
            let vx = x + 3 + label_w;
            self.text(vx, yy, &value, (x + w - 2).saturating_sub(vx), fg, bg, if sel { STYLE_BOLD } else { 0 });
        }
        let help = if s.editing.is_some() {
            "Type, Enter to save, Esc to cancel"
        } else {
            "↑↓ choose  ←→ change  Enter edit/toggle  Esc close"
        };
        self.text(x + 2, y + h - 3, help, w - 4, c.dim, c.panel, 0);
        let path = config::config_path();
        let saved = format!("Saved to {}", path.display());
        self.text(x + 2, y + h - 2, &saved, w - 4, c.dim, c.panel, 0);
    }

    pub fn search_bar(&mut self, y: usize, s: &SearchState, c: &UiColors) {
        self.fill((0, y, self.cols, 1), c.accent_bg);
        let mut x = self.text(0, y, " Find: ", self.cols, c.accent_fg, c.accent_bg, STYLE_BOLD);
        x += self.text(x, y, &s.query, self.cols.saturating_sub(x + 20), c.accent_fg, c.accent_bg, 0);
        self.put(x, y, '▏', c.accent_fg, c.accent_bg, 0);
        let status = format!("{}  Enter ↑  Shift+Enter ↓  Esc ", s.status());
        let sw = status.chars().count();
        if self.cols > x + sw + 2 {
            self.text(self.cols - sw, y, &status, sw, c.accent_fg, c.accent_bg, 0);
        }
    }
}
