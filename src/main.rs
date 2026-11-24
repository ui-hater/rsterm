//! rsterm — a GPU-rendered terminal emulator that exposes its text over AT-SPI.

mod a11y;
mod config;
mod font;
mod input;
mod pty;
mod render;
mod session;
mod term;
mod ui;

use config::{Config, TabBar};
use render::{CellInstance, Renderer, pack};
use session::{Session, Spawn};
use std::sync::Arc;
use std::time::{Duration, Instant};
use term::{Colors, CursorShape, Match, MouseMode, Selection, Term, flags};
use ui::{Action, Kind, Overlay, Painter, UiColors};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{CursorIcon, Window, WindowId};

const A11Y_INTERVAL: Duration = Duration::from_millis(40);
const BLINK: Duration = Duration::from_millis(530);
const SYNC_TIMEOUT: Duration = Duration::from_millis(150);
const FLASH: Duration = Duration::from_millis(120);
const MULTI_CLICK: Duration = Duration::from_millis(400);

#[derive(Debug)]
enum UserEvent {
    AccessKit(accesskit_winit::Event),
    Wakeup(u64),
    Exited(u64),
    ConfigChanged,
    /// Debug builds only: one step of RSTERM_DEBUG_SCRIPT.
    #[cfg(debug_assertions)]
    Debug(String),
}

impl From<accesskit_winit::Event> for UserEvent {
    fn from(e: accesskit_winit::Event) -> Self {
        UserEvent::AccessKit(e)
    }
}

// ---------------------------------------------------------------------------
// Terminal cell building
// ---------------------------------------------------------------------------

#[inline]
fn resolve(c: term::Color, default: u32, pal: &[u32; 256]) -> u32 {
    match c.0 >> 24 {
        1 => pal[(c.0 & 0xff) as usize],
        2 => c.0 & 0xff_ffff,
        _ => default,
    }
}

fn dim(rgb: u32) -> u32 {
    let f = |s: u32| ((rgb >> s & 0xff) * 2 / 3) << s;
    f(16) | f(8) | f(0)
}

/// Per-frame view state that affects how terminal cells look.
struct View<'a> {
    focused: bool,
    cursor_on: bool,
    cursor_default: CursorShape,
    bold_is_bright: bool,
    selection: (u32, u32),
    matches: &'a [Match],
    current: Option<Match>,
    hover: Option<(i64, usize, usize)>,
    flash: bool,
}

fn build_cells(term: &Term, glyphs: &mut font::Glyphs, v: &View<'_>, out: &mut [CellInstance]) {
    let (rows, cols) = (term.rows(), term.cols());
    let grid = term.screen();
    let off = term.display_offset;
    let colors = &term.colors;
    let pal = &colors.palette;
    let default_bg = if v.flash { ui::mix(colors.bg, colors.fg, 0.25) } else { colors.bg };

    let sel = term.selection.map(|s| s.ordered());
    let cursor_y = term.cy + off;
    let show_cursor = term.cursor_visible && cursor_y < rows && (v.cursor_on || !v.focused);
    let top = term.abs_line(0);
    // Matches are sorted by line; skip to the first one in view.
    let first_match = v.matches.partition_point(|m| m.line < top);

    for y in 0..rows {
        let row = grid.view_row(y, off);
        let abs = top + y as i64;
        let row_out = &mut out[y * cols..(y + 1) * cols];
        let mut prev_glyph = 0u32;
        for x in 0..cols {
            let cell = row.cells[x];
            let f = cell.flags;
            let fg_c = if v.bold_is_bright && f & flags::BOLD != 0 && cell.fg.0 >> 24 == 1 && cell.fg.0 & 0xff < 8 {
                term::Color::indexed((cell.fg.0 & 0xff) as u8 + 8)
            } else {
                cell.fg
            };
            let mut fg = resolve(fg_c, colors.fg, pal);
            let mut bg = resolve(cell.bg, default_bg, pal);
            if f & flags::INVERSE != 0 {
                std::mem::swap(&mut fg, &mut bg);
            }
            if f & flags::DIM != 0 {
                fg = dim(fg);
            }
            if f & flags::HIDDEN != 0 {
                fg = bg;
            }
            if let Some(((l0, c0), (l1, c1))) = sel {
                if (abs > l0 || (abs == l0 && x >= c0)) && (abs < l1 || (abs == l1 && x <= c1)) {
                    (fg, bg) = (v.selection.0, v.selection.1);
                }
            }
            for m in &v.matches[first_match..] {
                if m.line > abs {
                    break;
                }
                if m.line == abs && x >= m.start && x <= m.end {
                    let cur = v.current == Some(*m);
                    (fg, bg) = if cur { (0x000000, 0xff9e3b) } else { (0x000000, 0xe6c84a) };
                }
            }
            let style = (f & flags::BOLD != 0) as u32 * font::STYLE_BOLD
                | (f & flags::ITALIC != 0) as u32 * font::STYLE_ITALIC;
            let glyph = if f & flags::WIDE_SPACER != 0 {
                if prev_glyph != 0 { prev_glyph + glyphs.cell_w } else { 0 }
            } else {
                glyphs.get(cell.c, style)
            };
            prev_glyph = glyph;
            let mut iflags = 0;
            if f & flags::UNDERLINE != 0 || matches!(v.hover, Some((l, s, e)) if l == abs && x >= s && x <= e) {
                iflags |= render::F_UNDERLINE;
            }
            if f & flags::STRIKE != 0 {
                iflags |= render::F_STRIKE;
            }
            if show_cursor && y == cursor_y && x == term.cx {
                if !v.focused {
                    iflags |= render::F_CURSOR_HOLLOW;
                } else {
                    match term.cursor_shape.unwrap_or(v.cursor_default) {
                        CursorShape::Block => {
                            fg = bg;
                            bg = colors.cursor;
                        }
                        CursorShape::Underline => iflags |= render::F_CURSOR_UNDERLINE,
                        CursorShape::Bar => iflags |= render::F_CURSOR_BAR,
                    }
                }
            }
            row_out[x] = CellInstance { glyph, fg: pack(fg), bg: pack(bg), flags: iflags };
        }
    }
}

// ---------------------------------------------------------------------------
// Application state
// ---------------------------------------------------------------------------

struct Click {
    at: Instant,
    cell: (usize, usize),
    count: u8,
}

struct State {
    window: Arc<Window>,
    renderer: Renderer,
    glyphs: font::Glyphs,
    adapter: accesskit_winit::Adapter,
    proxy: EventLoopProxy<UserEvent>,

    cfg: Config,
    colors: Colors,
    ui_colors: UiColors,
    zoom: f32,
    scale: f64,
    pad: f32,

    tabs: Vec<Session>,
    active: usize,
    next_id: u64,
    overlay: Overlay,
    /// Bumped on any UI (non-terminal) change.
    ui_gen: u64,
    quit: bool,

    cells: Vec<CellInstance>,
    last_frame: u64,

    modifiers: ModifiersState,
    focused: bool,
    mouse_px: (f64, f64),
    mouse_buttons: u8,
    selecting: bool,
    sel_anchor: Option<(i64, usize)>,
    last_click: Option<Click>,
    hover: Option<(i64, usize, usize, String)>,
    scroll_accum: f64,
    clipboard: Option<arboard::Clipboard>,

    blink_epoch: Instant,
    flash_until: Option<Instant>,

    announcement: String,
    announce_flip: bool,
    a11y: a11y::A11y,
    a11y_active: bool,
    a11y_sent: (u64, u64, u64),
    a11y_last: Instant,
}

fn font_px(pt: f32, scale: f64) -> f32 {
    (pt * 4.0 / 3.0 * scale as f32).round()
}

fn theme_colors(cfg: &Config) -> (Colors, UiColors) {
    let t = config::theme(&cfg.theme).unwrap_or(&config::THEMES[0]);
    let c = Colors { palette: config::palette(t), fg: t.fg, bg: t.bg, cursor: t.cursor };
    (c, UiColors::new(t.fg, t.bg, t.selection_fg, t.selection_bg))
}

impl State {
    fn session(&self) -> &Session {
        &self.tabs[self.active]
    }

    fn bar_rows(&self) -> usize {
        match self.cfg.tab_bar {
            TabBar::Always => 1,
            TabBar::Never => 0,
            TabBar::Auto => (self.tabs.len() > 1) as usize,
        }
    }

    /// Window grid (cols, rows) including chrome.
    fn window_grid(&self) -> (usize, usize) {
        let size = self.window.inner_size();
        let w = (size.width as f32 - 2.0 * self.pad).max(0.0);
        let h = (size.height as f32 - 2.0 * self.pad).max(0.0);
        let cols = (w / self.glyphs.cell_w as f32) as usize;
        let rows = (h / self.glyphs.cell_h as f32) as usize;
        (cols.max(2), rows.max(1 + self.bar_rows()))
    }

    fn term_size(&self) -> (usize, usize) {
        let (c, r) = self.window_grid();
        (c, r - self.bar_rows())
    }

    fn touch_ui(&mut self) {
        self.ui_gen += 1;
        self.window.request_redraw();
    }

    fn announce(&mut self, msg: impl Into<String>) {
        // Toggle an invisible suffix so repeating the same message still
        // counts as a change and gets spoken again.
        self.announce_flip = !self.announce_flip;
        self.announcement = msg.into();
        if self.announce_flip {
            self.announcement.push('\u{200B}');
        }
        self.touch_ui();
    }

    fn apply_size(&mut self) {
        let (cols, rows) = self.term_size();
        for s in &self.tabs {
            s.resize(cols, rows, self.glyphs.cell_w, self.glyphs.cell_h);
        }
        self.touch_ui();
    }

    fn reload_font(&mut self) {
        let pt = (self.cfg.font_size + self.zoom).clamp(4.0, 96.0);
        self.glyphs = font::Glyphs::new(&self.cfg.font_family, font_px(pt, self.scale));
        self.window.set_resize_increments(Some(winit::dpi::PhysicalSize::new(
            self.glyphs.cell_w,
            self.glyphs.cell_h,
        )));
        self.apply_size();
    }

    fn new_term(&self, cols: usize, rows: usize) -> Term {
        let mut t = Term::new(cols, rows);
        t.set_colors(self.colors);
        t.set_scrollback(self.cfg.scrollback);
        t
    }

    fn spawn_session(&mut self, cwd: Option<String>) -> std::io::Result<Session> {
        let id = self.next_id;
        self.next_id += 1;
        let (cols, rows) = self.term_size();
        let term = self.new_term(cols, rows);
        let (p1, p2) = (self.proxy.clone(), self.proxy.clone());
        Session::spawn(
            id,
            Spawn {
                cols,
                rows,
                cell_w: self.glyphs.cell_w,
                cell_h: self.glyphs.cell_h,
                command: &self.cfg.command,
                cwd: cwd.as_deref(),
                term,
            },
            move |id| {
                let _ = p1.send_event(UserEvent::Wakeup(id));
            },
            move |id| {
                let _ = p2.send_event(UserEvent::Exited(id));
            },
        )
    }

    fn new_tab(&mut self) {
        let cwd = if self.cfg.inherit_cwd && !self.tabs.is_empty() { self.session().cwd() } else { None };
        let before = self.bar_rows();
        match self.spawn_session(cwd) {
            Ok(s) => {
                self.tabs.push(s);
                self.switch_tab(self.tabs.len() - 1);
                if self.bar_rows() != before {
                    self.apply_size();
                }
            }
            Err(e) => self.announce(format!("Could not start shell: {e}")),
        }
    }

    fn close_tab(&mut self, i: usize) {
        if i >= self.tabs.len() {
            return;
        }
        let before = self.bar_rows();
        self.tabs.remove(i); // Drop hangs up the child
        if self.tabs.is_empty() {
            self.quit = true;
            return;
        }
        if self.active > i || self.active >= self.tabs.len() {
            self.active = self.active.saturating_sub(1);
        }
        self.switch_tab(self.active);
        if self.bar_rows() != before {
            self.apply_size();
        }
    }

    fn switch_tab(&mut self, i: usize) {
        self.active = i.min(self.tabs.len() - 1);
        self.tabs[self.active].activity = false;
        self.tabs[self.active].bell = false;
        self.overlay = Overlay::None;
        self.hover = None;
        let title = self.session().title();
        self.window.set_title(&title);
        self.last_frame = 0;
        self.touch_ui();
    }

    fn write(&mut self, bytes: &[u8]) {
        let i = self.active;
        self.tabs[i].write(bytes);
    }

    // -- config ---------------------------------------------------------------

    fn apply_config(&mut self, new: Config, save: bool) {
        let old = std::mem::replace(&mut self.cfg, new);
        if old.theme != self.cfg.theme {
            (self.colors, self.ui_colors) = theme_colors(&self.cfg);
            for s in &self.tabs {
                s.shared.term.lock().unwrap().set_colors(self.colors);
            }
        }
        if old.scrollback != self.cfg.scrollback {
            for s in &self.tabs {
                s.shared.term.lock().unwrap().set_scrollback(self.cfg.scrollback);
            }
        }
        if old.padding != self.cfg.padding {
            self.pad = (self.cfg.padding * self.scale as f32).round();
        }
        if old.font_family != self.cfg.font_family || old.font_size != self.cfg.font_size {
            self.reload_font();
        } else if old.padding != self.cfg.padding || old.tab_bar != self.cfg.tab_bar {
            self.apply_size();
        }
        if save {
            if let Err(e) = self.cfg.save() {
                self.announce(format!("Could not save settings: {e}"));
            }
        }
        self.last_frame = 0;
        self.touch_ui();
    }

    fn reload_config(&mut self, announce: bool) {
        let (cfg, errors) = Config::load();
        if let Some(e) = errors.first() {
            self.announce(format!("Config error, {e}"));
        } else if announce {
            self.announce("Configuration reloaded");
        }
        if cfg != self.cfg {
            self.apply_config(cfg, false);
        }
    }

    // -- actions --------------------------------------------------------------

    fn run(&mut self, a: Action) {
        if !matches!(a, Action::OpenMenu) {
            if let Overlay::Menu(_) = self.overlay {
                self.overlay = Overlay::None;
                self.touch_ui();
            }
        }
        match a {
            Action::NewTab => self.new_tab(),
            Action::CloseTab => self.close_tab(self.active),
            Action::NextTab => {
                if self.tabs.len() > 1 {
                    self.switch_tab((self.active + 1) % self.tabs.len())
                }
            }
            Action::PrevTab => {
                if self.tabs.len() > 1 {
                    self.switch_tab((self.active + self.tabs.len() - 1) % self.tabs.len())
                }
            }
            Action::GotoTab(i) => self.switch_tab(i.min(self.tabs.len() - 1)),
            Action::Copy => {
                if self.copy_selection(false) {
                    self.announce("Copied");
                } else {
                    self.announce("Nothing selected");
                }
            }
            Action::Paste => self.paste(false),
            Action::SelectAll => {
                self.session().shared.term.lock().unwrap().select_all();
                self.announce("All text selected");
            }
            Action::Find => self.open_search(),
            Action::ClearScrollback => {
                let mut t = self.session().shared.term.lock().unwrap();
                t.set_selection(None);
                // Same as `clear && printf '\e[3J'` minus the clear.
                let mut p = vte::Parser::new();
                p.advance(&mut *t, b"\x1b[3J");
                drop(t);
                self.announce("Scrollback cleared");
            }
            Action::ZoomIn | Action::ZoomOut | Action::ZoomReset => {
                self.zoom = match a {
                    Action::ZoomIn => self.zoom + 1.0,
                    Action::ZoomOut => self.zoom - 1.0,
                    _ => 0.0,
                };
                self.reload_font();
                let pt = (self.cfg.font_size + self.zoom).clamp(4.0, 96.0);
                self.announce(format!("Font size {}", ui::fmt_num(pt as f64)));
            }
            Action::Settings => {
                self.overlay = Overlay::Settings(ui::SettingsState::new());
                self.touch_ui();
            }
            Action::ReloadConfig => self.reload_config(true),
            Action::OpenMenu => {
                let at = if let Overlay::Menu(_) = self.overlay {
                    None
                } else {
                    Some(self.mouse_cell_or_cursor())
                };
                self.overlay = match at {
                    Some(at) => Overlay::Menu(ui::MenuState { sel: 0, at }),
                    None => Overlay::None,
                };
                self.touch_ui();
            }
            Action::Quit => self.quit = true,
        }
    }

    /// Where to pop up a keyboard-invoked menu: the mouse if it's inside the
    /// window, else the text cursor.
    fn mouse_cell_or_cursor(&self) -> (usize, usize) {
        let size = self.window.inner_size();
        let (mx, my) = self.mouse_px;
        if mx > 0.0 && my > 0.0 && mx < size.width as f64 && my < size.height as f64 {
            self.grid_cell(self.mouse_px)
        } else {
            let t = self.session().shared.term.lock().unwrap();
            (t.cx, t.cy + self.bar_rows() + 1)
        }
    }

    // -- clipboard --------------------------------------------------------------

    fn clipboard(&mut self) -> Option<&mut arboard::Clipboard> {
        if self.clipboard.is_none() {
            self.clipboard = arboard::Clipboard::new().ok();
        }
        self.clipboard.as_mut()
    }

    fn set_clipboard(&mut self, text: String, primary: bool) {
        let Some(cb) = self.clipboard() else { return };
        if primary {
            use arboard::{LinuxClipboardKind, SetExtLinux};
            let _ = cb.set().clipboard(LinuxClipboardKind::Primary).text(text);
        } else {
            let _ = cb.set_text(text);
        }
    }

    fn copy_selection(&mut self, primary: bool) -> bool {
        let text = self.session().shared.term.lock().unwrap().selection_text();
        match text {
            Some(t) if !t.is_empty() => {
                self.set_clipboard(t, primary);
                true
            }
            _ => false,
        }
    }

    fn get_clipboard(&mut self, primary: bool) -> Option<String> {
        let cb = self.clipboard()?;
        if primary {
            use arboard::{GetExtLinux, LinuxClipboardKind};
            cb.get().clipboard(LinuxClipboardKind::Primary).text().ok()
        } else {
            cb.get_text().ok()
        }
    }

    fn paste(&mut self, primary: bool) {
        let Some(text) = self.get_clipboard(primary) else { return };
        match &mut self.overlay {
            Overlay::Search(s) => {
                s.query.push_str(text.lines().next().unwrap_or(""));
                self.update_search(true);
                return;
            }
            Overlay::Settings(s) if s.editing.is_some() => {
                s.editing.as_mut().unwrap().push_str(text.lines().next().unwrap_or(""));
                self.touch_ui();
                return;
            }
            _ => {}
        }
        let bracketed = {
            let mut t = self.session().shared.term.lock().unwrap();
            t.reset_view();
            t.bracketed_paste
        };
        let text = text.replace("\r\n", "\r").replace('\n', "\r");
        if bracketed {
            let text = text.replace('\x1b', "");
            self.write(b"\x1b[200~");
            self.write(text.as_bytes());
            self.write(b"\x1b[201~");
        } else {
            self.write(text.as_bytes());
        }
    }

    // -- search -------------------------------------------------------------------

    fn open_search(&mut self) {
        let seed = {
            let t = self.session().shared.term.lock().unwrap();
            t.selection_text().filter(|s| !s.contains('\n') && s.len() < 200).unwrap_or_default()
        };
        self.overlay = Overlay::Search(ui::SearchState { query: seed, matches: Vec::new(), current: None });
        self.update_search(true);
    }

    /// Recompute matches; with `jump`, select the newest match.
    fn update_search(&mut self, jump: bool) {
        let Overlay::Search(s) = &mut self.overlay else { return };
        let sh = self.tabs[self.active].shared.clone();
        let mut t = sh.term.lock().unwrap();
        s.matches = t.search(&s.query);
        s.current = if jump { s.matches.len().checked_sub(1) } else { s.current };
        let msg = Self::focus_match(&mut t, s);
        drop(t);
        if let Some(m) = msg {
            self.announce(m);
        } else {
            self.touch_ui();
        }
    }

    fn step_search(&mut self, dir: isize) {
        let Overlay::Search(s) = &mut self.overlay else { return };
        let n = s.matches.len();
        if n == 0 {
            self.announce("No matches");
            return;
        }
        s.current = Some(match s.current {
            Some(i) => (i as isize + dir).rem_euclid(n as isize) as usize,
            None => n - 1,
        });
        let sh = self.tabs[self.active].shared.clone();
        let mut t = sh.term.lock().unwrap();
        let msg = Self::focus_match(&mut t, s);
        drop(t);
        if let Some(m) = msg {
            self.announce(m);
        }
    }

    /// Scroll to and select the current match; returns text to announce.
    fn focus_match(t: &mut Term, s: &ui::SearchState) -> Option<String> {
        if s.query.is_empty() {
            return None;
        }
        let Some(m) = s.current.and_then(|i| s.matches.get(i)) else {
            return Some("No matches".into());
        };
        t.scroll_to_line(m.line);
        t.set_selection(Some(Selection { anchor: (m.line, m.start), head: (m.line, m.end) }));
        let line: String = t
            .abs_row(m.line)
            .map(|r| r.cells.iter().filter(|c| c.flags & flags::WIDE_SPACER == 0).map(|c| c.c).collect())
            .unwrap_or_default();
        Some(format!("{}: {}", s.status(), line.trim()))
    }

    // -- keyboard -----------------------------------------------------------------

    fn on_key(&mut self, ev: &winit::event::KeyEvent) {
        if ev.state != ElementState::Pressed {
            return;
        }
        self.blink_epoch = Instant::now();
        if !matches!(self.overlay, Overlay::None) {
            // Clipboard paste works inside search / text fields.
            if let Some(Action::Paste) = input::shortcut(ev, self.modifiers, self.tabs.len()) {
                self.paste(false);
                return;
            }
            let named = match &ev.logical_key {
                Key::Named(n) => Some(*n),
                _ => None,
            };
            let text = ev.text.as_ref().map(|t| t.to_string());
            self.overlay_key(named, text.as_deref());
            return;
        }
        if let Some(a) = input::shortcut(ev, self.modifiers, self.tabs.len()) {
            self.run(a);
            return;
        }
        // Scrollback navigation.
        let m = self.modifiers;
        if m.shift_key() && !m.control_key() && !m.alt_key() {
            if let Key::Named(k) = &ev.logical_key {
                let mut t = self.session().shared.term.lock().unwrap();
                let page = t.rows() as isize - 1;
                let d = match k {
                    NamedKey::PageUp => Some(page),
                    NamedKey::PageDown => Some(-page),
                    NamedKey::Home if !t.alt_active => Some(isize::MAX / 2),
                    NamedKey::End if !t.alt_active => Some(-(isize::MAX / 2)),
                    _ => None,
                };
                if let Some(d) = d {
                    t.scroll_view(d);
                    drop(t);
                    self.window.request_redraw();
                    return;
                }
            }
        }
        let app_cursor = self.session().shared.term.lock().unwrap().app_cursor;
        let bytes = input::encode(ev, self.modifiers, app_cursor);
        if bytes.is_empty() {
            return;
        }
        {
            let mut t = self.session().shared.term.lock().unwrap();
            t.reset_view();
            if t.selection.is_some() && !self.selecting {
                t.set_selection(None);
            }
        }
        self.write(&bytes);
        self.window.set_cursor_visible(false);
    }

    /// Keyboard handling while a menu / search bar / settings screen is open.
    fn overlay_key(&mut self, named: Option<NamedKey>, text: Option<&str>) {
        let shift = self.modifiers.shift_key();
        let ctrl = self.modifiers.control_key();
        match &mut self.overlay {
            Overlay::None => {}
            Overlay::Menu(m) => match named {
                Some(NamedKey::Escape) => {
                    self.overlay = Overlay::None;
                    self.touch_ui();
                }
                Some(NamedKey::ArrowDown) | Some(NamedKey::Tab) if !shift => {
                    m.sel = ui::menu_step(m.sel, 1);
                    self.touch_ui();
                }
                Some(NamedKey::ArrowUp) | Some(NamedKey::Tab) => {
                    m.sel = ui::menu_step(m.sel, -1);
                    self.touch_ui();
                }
                Some(NamedKey::Home) => {
                    m.sel = ui::menu_step(ui::MENU.len() - 1, 1);
                    self.touch_ui();
                }
                Some(NamedKey::End) => {
                    m.sel = ui::menu_step(0, -1);
                    self.touch_ui();
                }
                Some(NamedKey::Enter) | Some(NamedKey::Space) => {
                    if let Some(a) = ui::MENU[m.sel].action {
                        self.overlay = Overlay::None;
                        self.run(a);
                    }
                }
                _ => {
                    // First-letter navigation.
                    if let Some(c) = text.and_then(|t| t.chars().next()) {
                        let c = c.to_ascii_lowercase();
                        let n = ui::MENU.len();
                        if let Some(i) = (1..=n).map(|d| (m.sel + d) % n).find(|&i| {
                            ui::MENU[i].action.is_some() && ui::MENU[i].label.to_ascii_lowercase().starts_with(c)
                        }) {
                            m.sel = i;
                            self.touch_ui();
                        }
                    }
                }
            },
            Overlay::Search(s) => match named {
                Some(NamedKey::Escape) => {
                    self.overlay = Overlay::None;
                    self.session().shared.term.lock().unwrap().set_selection(None);
                    self.touch_ui();
                }
                Some(NamedKey::Enter) | Some(NamedKey::ArrowUp) if !shift => self.step_search(-1),
                Some(NamedKey::Enter) | Some(NamedKey::ArrowDown) => self.step_search(1),
                Some(NamedKey::Backspace) => {
                    s.query.pop();
                    self.update_search(true);
                }
                _ => {
                    if let Some(t) = text.filter(|t| !ctrl && t.chars().all(|c| !c.is_control())) {
                        s.query.push_str(t);
                        self.update_search(true);
                    }
                }
            },
            Overlay::Settings(s) => {
                if let Some(buf) = &mut s.editing {
                    match named {
                        Some(NamedKey::Escape) => s.editing = None,
                        Some(NamedKey::Enter) => {
                            let v = s.editing.take().unwrap();
                            let key = s.items[s.sel].key;
                            let mut cfg = self.cfg.clone();
                            if cfg.set(key, v.trim()).is_ok() {
                                self.apply_config(cfg, true);
                                self.announce("Saved");
                            }
                        }
                        Some(NamedKey::Backspace) => {
                            buf.pop();
                        }
                        _ => {
                            if let Some(t) = text.filter(|t| !ctrl && t.chars().all(|c| !c.is_control())) {
                                buf.push_str(t);
                            }
                        }
                    }
                    self.touch_ui();
                    return;
                }
                let n = s.items.len();
                match named {
                    Some(NamedKey::Escape) => self.overlay = Overlay::None,
                    Some(NamedKey::ArrowDown) => s.sel = (s.sel + 1) % n,
                    Some(NamedKey::ArrowUp) => s.sel = (s.sel + n - 1) % n,
                    Some(NamedKey::Tab) => s.sel = if shift { (s.sel + n - 1) % n } else { (s.sel + 1) % n },
                    Some(NamedKey::Home) => s.sel = 0,
                    Some(NamedKey::End) => s.sel = n - 1,
                    Some(NamedKey::ArrowLeft) | Some(NamedKey::ArrowRight) | Some(NamedKey::PageUp)
                    | Some(NamedKey::PageDown) => {
                        let dir = match named {
                            Some(NamedKey::ArrowLeft) => -1,
                            Some(NamedKey::ArrowRight) => 1,
                            Some(NamedKey::PageUp) => 10,
                            _ => -10,
                        };
                        let mut cfg = self.cfg.clone();
                        if s.adjust(&mut cfg, dir) {
                            self.apply_config(cfg, true);
                        }
                    }
                    Some(NamedKey::Enter) | Some(NamedKey::Space) => match s.items[s.sel].kind {
                        Kind::Text => s.editing = Some(self.cfg.get(s.items[s.sel].key)),
                        _ => {
                            let mut cfg = self.cfg.clone();
                            if s.adjust(&mut cfg, 1) {
                                self.apply_config(cfg, true);
                            }
                        }
                    },
                    _ => {}
                }
                self.touch_ui();
            }
        }
    }

    // -- mouse ----------------------------------------------------------------------

    /// Window grid cell under a pixel position.
    fn grid_cell(&self, (px, py): (f64, f64)) -> (usize, usize) {
        let (cols, rows) = self.window_grid();
        let x = ((px - self.pad as f64) / self.glyphs.cell_w as f64).max(0.0) as usize;
        let y = ((py - self.pad as f64) / self.glyphs.cell_h as f64).max(0.0) as usize;
        (x.min(cols - 1), y.min(rows - 1))
    }

    /// Terminal cell under the mouse (None over the tab bar).
    fn term_cell(&self) -> Option<(usize, usize)> {
        let (x, y) = self.grid_cell(self.mouse_px);
        let bar = self.bar_rows();
        let rows = self.term_size().1;
        (y >= bar).then(|| (x, (y - bar).min(rows - 1)))
    }

    fn mouse_report(&mut self, button: u8, pressed: bool, motion: bool) {
        let Some((x, y)) = self.term_cell() else { return };
        let mut b = button as u32;
        if self.modifiers.shift_key() {
            b += 4;
        }
        if self.modifiers.alt_key() {
            b += 8;
        }
        if self.modifiers.control_key() {
            b += 16;
        }
        if motion {
            b += 32;
        }
        let sgr = self.session().shared.term.lock().unwrap().sgr_mouse;
        if sgr {
            let s = format!("\x1b[<{};{};{}{}", b, x + 1, y + 1, if pressed { 'M' } else { 'm' });
            self.write(s.as_bytes());
        } else {
            let b = if pressed || button >= 64 { b } else { 3 + (b & !3) };
            let enc = |v: usize| (32 + v + 1).min(255) as u8;
            self.write(&[0x1b, b'[', b'M', (32 + b).min(255) as u8, enc(x), enc(y)]);
        }
    }

    fn app_mouse(&self) -> MouseMode {
        if self.modifiers.shift_key() {
            return MouseMode::Off; // shift bypasses app mouse reporting
        }
        self.session().shared.term.lock().unwrap().mouse_mode
    }

    fn on_mouse_button(&mut self, state: ElementState, button: MouseButton) {
        let pressed = state == ElementState::Pressed;
        let cell = self.grid_cell(self.mouse_px);
        let (cols, rows) = self.window_grid();

        // Overlays take the mouse first.
        match &mut self.overlay {
            Overlay::Menu(m) => {
                if !pressed {
                    return;
                }
                let (x, y, w, h) = ui::menu_rect(m, cols, rows);
                let inside = cell.0 >= x && cell.0 < x + w && cell.1 > y && cell.1 < y + h - 1;
                let action = inside.then(|| ui::MENU.get(cell.1 - y - 1).and_then(|i| i.action)).flatten();
                self.overlay = Overlay::None;
                self.touch_ui();
                if let Some(a) = action {
                    self.run(a);
                }
                return;
            }
            Overlay::Settings(s) => {
                if !pressed {
                    return;
                }
                let (x, y, w, h) = ui::settings_rect(cols, rows);
                if cell.0 < x || cell.0 >= x + w || cell.1 < y || cell.1 >= y + h {
                    self.overlay = Overlay::None;
                } else if cell.1 >= y + 2 && cell.1 < y + 2 + ui::settings_visible(h) {
                    let i = s.scroll + cell.1 - y - 2;
                    if i < s.items.len() {
                        let was = s.sel;
                        s.sel = i;
                        if was == i && !matches!(s.items[i].kind, Kind::Text) {
                            let dir = if button == MouseButton::Right { -1 } else { 1 };
                            let mut cfg = self.cfg.clone();
                            if s.adjust(&mut cfg, dir) {
                                self.apply_config(cfg, true);
                            }
                        }
                    }
                }
                self.touch_ui();
                return;
            }
            _ => {}
        }

        // Tab bar.
        let bar = self.bar_rows();
        if cell.1 < bar {
            if !pressed {
                return;
            }
            let lay = ui::tab_bar_layout(cols, self.tabs.len());
            if cell.0 >= lay.plus_x {
                self.new_tab();
            } else if let Some(i) = lay.tabs.iter().position(|&(x, w)| cell.0 >= x && cell.0 < x + w) {
                match button {
                    MouseButton::Middle => self.close_tab(i),
                    _ => self.switch_tab(i),
                }
            }
            return;
        }

        let (code, bit) = match button {
            MouseButton::Left => (0u8, 1u8),
            MouseButton::Middle => (1, 2),
            MouseButton::Right => (2, 4),
            _ => return,
        };
        if pressed {
            self.mouse_buttons |= bit;
        } else {
            self.mouse_buttons &= !bit;
        }
        if self.app_mouse() != MouseMode::Off {
            self.mouse_report(code, pressed, false);
            return;
        }
        let Some((x, y)) = self.term_cell() else { return };
        match (button, pressed) {
            (MouseButton::Left, true) => {
                // Ctrl+click opens links.
                if self.modifiers.control_key() {
                    if let Some((_, _, _, url)) = &self.hover {
                        open_url(url);
                        self.announce(format!("Opening {url}"));
                        return;
                    }
                }
                let now = Instant::now();
                let count = match &self.last_click {
                    Some(c) if now - c.at < MULTI_CLICK && c.cell == (x, y) => c.count % 3 + 1,
                    _ => 1,
                };
                self.last_click = Some(Click { at: now, cell: (x, y), count });
                let mut t = self.tabs[self.active].shared.term.lock().unwrap();
                let line = t.abs_line(y);
                let cols = t.cols();
                match count {
                    2 => {
                        let (s, e) = t.word_bounds(line, x);
                        t.set_selection(Some(Selection { anchor: (line, s), head: (line, e) }));
                        self.sel_anchor = Some((line, s));
                    }
                    3 => {
                        let (a, b) = t.logical_line(line);
                        t.set_selection(Some(Selection { anchor: (a, 0), head: (b, cols - 1) }));
                        self.sel_anchor = Some((a, 0));
                    }
                    _ => {
                        t.set_selection(None);
                        self.sel_anchor = Some((line, x));
                    }
                }
                drop(t);
                self.selecting = true;
                self.window.request_redraw();
            }
            (MouseButton::Left, false) => {
                self.selecting = false;
                if self.cfg.copy_on_select {
                    self.copy_selection(true);
                }
            }
            (MouseButton::Middle, true) => self.paste(true),
            (MouseButton::Right, true) => {
                self.overlay = Overlay::Menu(ui::MenuState { sel: 0, at: cell });
                self.touch_ui();
            }
            _ => {}
        }
    }

    fn on_mouse_move(&mut self) {
        let cell = self.grid_cell(self.mouse_px);
        let (cols, rows) = self.window_grid();
        if let Overlay::Menu(m) = &mut self.overlay {
            let (x, y, w, h) = ui::menu_rect(m, cols, rows);
            if cell.0 > x && cell.0 < x + w - 1 && cell.1 > y && cell.1 < y + h - 1 {
                let i = cell.1 - y - 1;
                if ui::MENU.get(i).is_some_and(|it| it.action.is_some()) && m.sel != i {
                    m.sel = i;
                    self.touch_ui();
                }
            }
            return;
        }
        let mode = self.app_mouse();
        if (mode == MouseMode::Drag && self.mouse_buttons != 0) || mode == MouseMode::Motion {
            let b = match self.mouse_buttons {
                0 => 3,
                b if b & 1 != 0 => 0,
                b if b & 2 != 0 => 1,
                _ => 2,
            };
            self.mouse_report(b, true, true);
            return;
        }
        self.update_hover();
        if self.selecting {
            if let (Some(anchor), Some((x, y))) = (self.sel_anchor, self.term_cell()) {
                let mut t = self.tabs[self.active].shared.term.lock().unwrap();
                let head = (t.abs_line(y), x);
                if head != anchor || t.selection.is_some() {
                    t.set_selection(Some(Selection { anchor, head }));
                }
                drop(t);
                self.window.request_redraw();
            }
        }
    }

    /// With Ctrl held, underline the link under the mouse.
    fn update_hover(&mut self) {
        let new = if self.modifiers.control_key() {
            self.term_cell().and_then(|(x, y)| {
                let t = self.session().shared.term.lock().unwrap();
                let line = t.abs_line(y);
                t.url_at(line, x).map(|(u, s, e)| (line, s, e, u))
            })
        } else {
            None
        };
        if new != self.hover {
            self.window.set_cursor(if new.is_some() { CursorIcon::Pointer } else { CursorIcon::Text });
            self.hover = new;
            self.touch_ui();
        }
    }

    fn on_scroll(&mut self, lines: isize) {
        if lines == 0 {
            return;
        }
        if let Overlay::Settings(s) = &mut self.overlay {
            let n = s.items.len() as isize;
            s.sel = (s.sel as isize - lines.signum()).rem_euclid(n) as usize;
            self.touch_ui();
            return;
        }
        let (mode, alt, alt_scroll, app_cursor) = {
            let t = self.session().shared.term.lock().unwrap();
            (t.mouse_mode, t.alt_active, t.alternate_scroll, t.app_cursor)
        };
        if mode != MouseMode::Off && !self.modifiers.shift_key() {
            let b = if lines > 0 { 64 } else { 65 };
            for _ in 0..lines.unsigned_abs() {
                self.mouse_report(b, true, false);
            }
        } else if alt && alt_scroll {
            let seq: &[u8] = match (lines > 0, app_cursor) {
                (true, true) => b"\x1bOA",
                (true, false) => b"\x1b[A",
                (false, true) => b"\x1bOB",
                (false, false) => b"\x1b[B",
            };
            for _ in 0..lines.unsigned_abs() {
                self.write(seq);
            }
        } else {
            self.session().shared.term.lock().unwrap().scroll_view(lines);
            self.window.request_redraw();
        }
    }

    // -- output events ------------------------------------------------------------

    fn on_wakeup(&mut self, id: u64) {
        let Some(i) = self.tabs.iter().position(|s| s.id == id) else { return };
        let shared = self.tabs[i].shared.clone();
        shared.wakeup_pending.store(false, std::sync::atomic::Ordering::Release);
        let (title, bell, clip) = {
            let mut t = shared.term.lock().unwrap();
            let title = std::mem::take(&mut t.title_changed).then(|| t.title.clone());
            (title, std::mem::take(&mut t.bell), t.clipboard_request.take())
        };
        if let Some(text) = clip.filter(|_| self.cfg.clipboard_write) {
            self.set_clipboard(text, false);
        }
        if i == self.active {
            if let Some(t) = &title {
                self.window.set_title(t);
            }
            if bell {
                if self.cfg.visual_bell {
                    self.flash_until = Some(Instant::now() + FLASH);
                }
                if !self.focused {
                    self.window.request_user_attention(Some(winit::window::UserAttentionType::Informational));
                }
            }
            if title.is_some() {
                self.ui_gen += 1;
            }
            self.window.request_redraw();
        } else {
            let s = &mut self.tabs[i];
            let changed = !s.activity || title.is_some() || (bell && !s.bell);
            s.activity = true;
            s.bell |= bell;
            if bell {
                let n = i + 1;
                self.announce(format!("Bell in tab {n}"));
            }
            if changed {
                self.touch_ui();
            }
        }
    }

    // -- frame ----------------------------------------------------------------------

    fn cursor_on(&self) -> bool {
        !self.cfg.cursor_blink
            || !self.focused
            || (self.blink_epoch.elapsed().as_millis() / BLINK.as_millis()) % 2 == 0
    }

    fn redraw(&mut self) {
        let (cols, rows) = self.window_grid();
        let bar = self.bar_rows();
        let flash = self.flash_until.is_some_and(|t| Instant::now() < t);
        let cursor_on = self.cursor_on();
        let shared = self.session().shared.clone();
        let term = shared.term.lock().unwrap();

        // Hold the previous frame while the app has synchronized output on.
        if term.sync_since.is_some_and(|t| t.elapsed() < SYNC_TIMEOUT) {
            return;
        }

        let frame_key = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            (term.generation, self.ui_gen, self.glyphs.epoch, self.focused, cursor_on, flash, cols, rows)
                .hash(&mut h);
            self.session().id.hash(&mut h);
            h.finish()
        };
        let dirty = frame_key != self.last_frame;
        if dirty {
            self.cells.clear();
            self.cells.resize(cols * rows, CellInstance { bg: pack(self.colors.bg), ..Default::default() });
            for _attempt in 0..2 {
                let epoch = self.glyphs.epoch;
                let (search_matches, current) = match &self.overlay {
                    Overlay::Search(s) => (&s.matches[..], s.current.and_then(|i| s.matches.get(i)).copied()),
                    _ => (&[][..], None),
                };
                let view = View {
                    focused: self.focused,
                    cursor_on,
                    cursor_default: self.cfg.cursor_style,
                    bold_is_bright: self.cfg.bold_is_bright,
                    selection: (self.ui_colors.accent_fg, self.ui_colors.accent_bg),
                    matches: search_matches,
                    current,
                    hover: self.hover.as_ref().map(|h| (h.0, h.1, h.2)),
                    flash,
                };
                let tcols = term.cols().min(cols);
                let trows = term.rows().min(rows - bar);
                if term.cols() == cols && term.rows() == rows - bar {
                    build_cells(&term, &mut self.glyphs, &view, &mut self.cells[bar * cols..]);
                } else {
                    // Mid-resize: build at the terminal's size, then copy what fits.
                    let mut tmp = vec![CellInstance::default(); term.cols() * term.rows()];
                    build_cells(&term, &mut self.glyphs, &view, &mut tmp);
                    for y in 0..trows {
                        let src = &tmp[y * term.cols()..y * term.cols() + tcols];
                        self.cells[(bar + y) * cols..(bar + y) * cols + tcols].copy_from_slice(src);
                    }
                }
                let mut p = Painter { cells: &mut self.cells, cols, rows, glyphs: &mut self.glyphs };
                if bar > 0 {
                    let titles: Vec<(String, bool, bool)> = self
                        .tabs
                        .iter()
                        .enumerate()
                        .map(|(i, s)| {
                            let title = if i == self.active { term.title.clone() } else { s.title() };
                            (title, i == self.active, s.activity || s.bell)
                        })
                        .collect();
                    p.tab_bar(0, &titles, &self.ui_colors);
                }
                match &mut self.overlay {
                    Overlay::None => {}
                    Overlay::Menu(m) => p.menu(m, &self.ui_colors),
                    Overlay::Settings(s) => p.settings(s, &self.cfg, &self.ui_colors),
                    Overlay::Search(s) => p.search_bar(rows - 1, s, &self.ui_colors),
                }
                if self.glyphs.epoch == epoch {
                    break; // atlas didn't overflow mid-build
                }
            }
            self.last_frame = frame_key;
        }
        drop(term);

        self.renderer.upload_atlas(&mut self.glyphs);
        let bg = self.colors.bg;
        let bgf = [
            (bg >> 16 & 0xff) as f64 / 255.0,
            (bg >> 8 & 0xff) as f64 / 255.0,
            (bg & 0xff) as f64 / 255.0,
        ];
        self.renderer.render(
            dirty.then_some(&self.cells[..]),
            self.cells.len() as u32,
            cols as u32,
            &self.glyphs,
            self.pad,
            bgf,
            pack(self.colors.cursor),
        );
    }

    // -- accessibility ----------------------------------------------------------------

    fn push_a11y(&mut self) {
        if !self.a11y_active {
            return;
        }
        let (cols, rows) = self.window_grid();
        let g = a11y::Geometry {
            cell_w: self.glyphs.cell_w as f64,
            cell_h: self.glyphs.cell_h as f64,
            pad: self.pad as f64,
            bar_rows: self.bar_rows(),
            cols,
            rows,
        };
        let shared = self.session().shared.clone();
        let term = shared.term.lock().unwrap();
        let tabs: Vec<(String, bool)> = self
            .tabs
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let title = if i == self.active { term.title.clone() } else { s.title() };
                (title, i == self.active)
            })
            .collect();
        let view = a11y::UiView {
            title: &term.title,
            tab_id: self.session().id,
            tabs: &tabs,
            overlay: &self.overlay,
            cfg: &self.cfg,
            announcement: &self.announcement,
        };
        let update = self.a11y.update(&term, &g, &view);
        self.a11y_sent = (term.generation, self.ui_gen, self.session().id);
        self.a11y_last = Instant::now();
        drop(term);
        self.adapter.update_if_active(|| update);
    }

    fn on_a11y_action(&mut self, req: accesskit::ActionRequest) {
        let id = req.target_node.0;
        match req.action {
            accesskit::Action::Click if (a11y::TAB_BASE..a11y::TAB_BASE + 100).contains(&id) => {
                self.switch_tab((id - a11y::TAB_BASE) as usize);
            }
            accesskit::Action::Click if (a11y::MENU_ITEM_BASE..a11y::MENU_ITEM_BASE + 100).contains(&id) => {
                if let Some(a) = ui::MENU.get((id - a11y::MENU_ITEM_BASE) as usize).and_then(|i| i.action) {
                    self.overlay = Overlay::None;
                    self.run(a);
                }
            }
            accesskit::Action::ShowContextMenu => self.run(Action::OpenMenu),
            accesskit::Action::Focus if req.target_node == a11y::term_node(self.session().id) => {
                self.overlay = Overlay::None;
                self.touch_ui();
            }
            _ => {}
        }
    }

    /// Earliest time something needs to happen without input.
    fn next_deadline(&self) -> Option<Instant> {
        let mut d: Vec<Instant> = Vec::new();
        if self.cfg.cursor_blink && self.focused {
            let n = self.blink_epoch.elapsed().as_millis() / BLINK.as_millis() + 1;
            d.push(self.blink_epoch + BLINK * n as u32);
        }
        if let Some(t) = self.flash_until {
            d.push(t);
        }
        if let Some(t) = self.session().shared.term.lock().unwrap().sync_since {
            d.push(t + SYNC_TIMEOUT);
        }
        d.into_iter().min()
    }
}

/// Debug builds: `RSTERM_DEBUG_SCRIPT="a:OpenMenu;k:ArrowDown;k:Enter;t:hello;w:300"`
/// drives the UI without a keyboard (a: action, k: named key, t: text, w: wait ms).
#[cfg(debug_assertions)]
fn start_debug_script(proxy: EventLoopProxy<UserEvent>) {
    let Ok(script) = std::env::var("RSTERM_DEBUG_SCRIPT") else { return };
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(800));
        for step in script.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            if let Some(ms) = step.strip_prefix("w:") {
                std::thread::sleep(Duration::from_millis(ms.parse().unwrap_or(100)));
                continue;
            }
            let _ = proxy.send_event(UserEvent::Debug(step.to_string()));
            std::thread::sleep(Duration::from_millis(120));
        }
    });
}

#[cfg(debug_assertions)]
impl State {
    fn debug_step(&mut self, step: &str) {
        let (kind, arg) = step.split_once(':').unwrap_or((step, ""));
        match kind {
            "a" => {
                let a = match arg {
                    "NewTab" => Action::NewTab,
                    "CloseTab" => Action::CloseTab,
                    "NextTab" => Action::NextTab,
                    "PrevTab" => Action::PrevTab,
                    "Copy" => Action::Copy,
                    "SelectAll" => Action::SelectAll,
                    "Find" => Action::Find,
                    "ZoomIn" => Action::ZoomIn,
                    "ZoomOut" => Action::ZoomOut,
                    "Settings" => Action::Settings,
                    "ReloadConfig" => Action::ReloadConfig,
                    "OpenMenu" => Action::OpenMenu,
                    "Quit" => Action::Quit,
                    _ => return eprintln!("debug: unknown action {arg}"),
                };
                self.run(a);
            }
            "k" => {
                let k = match arg {
                    "ArrowDown" => NamedKey::ArrowDown,
                    "ArrowUp" => NamedKey::ArrowUp,
                    "ArrowLeft" => NamedKey::ArrowLeft,
                    "ArrowRight" => NamedKey::ArrowRight,
                    "Enter" => NamedKey::Enter,
                    "Escape" => NamedKey::Escape,
                    "Backspace" => NamedKey::Backspace,
                    "Tab" => NamedKey::Tab,
                    _ => return eprintln!("debug: unknown key {arg}"),
                };
                if matches!(self.overlay, Overlay::None) {
                    eprintln!("debug: key {arg} with no overlay ignored");
                } else {
                    self.overlay_key(Some(k), None);
                }
            }
            "t" => {
                if matches!(self.overlay, Overlay::None) {
                    self.write(arg.as_bytes());
                } else {
                    self.overlay_key(None, Some(arg));
                }
            }
            _ => eprintln!("debug: bad step {step}"),
        }
    }
}

fn open_url(url: &str) {
    let _ = std::process::Command::new("xdg-open")
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

// ---------------------------------------------------------------------------
// winit glue
// ---------------------------------------------------------------------------

struct App {
    proxy: EventLoopProxy<UserEvent>,
    state: Option<State>,
}

impl App {
    fn init(&mut self, event_loop: &ActiveEventLoop) {
        let (cfg, errors) = Config::load();
        let attrs = Window::default_attributes()
            .with_title("rsterm")
            .with_visible(false)
            .with_inner_size(winit::dpi::LogicalSize::new(900.0, 560.0));
        let window = Arc::new(event_loop.create_window(attrs).expect("create window"));
        // The AccessKit adapter must exist before the window is first shown.
        let adapter = accesskit_winit::Adapter::with_event_loop_proxy(event_loop, &window, self.proxy.clone());
        window.set_visible(true);

        let scale = window.scale_factor();
        let glyphs = font::Glyphs::new(&cfg.font_family, font_px(cfg.font_size, scale));
        window.set_resize_increments(Some(winit::dpi::PhysicalSize::new(glyphs.cell_w, glyphs.cell_h)));
        let renderer = Renderer::new(window.clone());
        let (colors, ui_colors) = theme_colors(&cfg);

        let proxy = self.proxy.clone();
        config::watch(move || {
            let _ = proxy.send_event(UserEvent::ConfigChanged);
        });

        #[cfg(debug_assertions)]
        start_debug_script(self.proxy.clone());
        let mut st = State {
            window,
            renderer,
            glyphs,
            adapter,
            proxy: self.proxy.clone(),
            pad: (cfg.padding * scale as f32).round(),
            cfg,
            colors,
            ui_colors,
            zoom: 0.0,
            scale,
            tabs: Vec::new(),
            active: 0,
            next_id: 0,
            overlay: Overlay::None,
            ui_gen: 1,
            quit: false,
            cells: Vec::new(),
            last_frame: 0,
            modifiers: ModifiersState::empty(),
            focused: true,
            mouse_px: (-1.0, -1.0),
            mouse_buttons: 0,
            selecting: false,
            sel_anchor: None,
            last_click: None,
            hover: None,
            scroll_accum: 0.0,
            clipboard: None,
            blink_epoch: Instant::now(),
            flash_until: None,
            announcement: String::new(),
            announce_flip: false,
            a11y: a11y::A11y::default(),
            a11y_active: false,
            a11y_sent: (0, 0, u64::MAX),
            a11y_last: Instant::now(),
        };
        let s = st.spawn_session(None).expect("spawn shell");
        st.tabs.push(s);
        if let Some(e) = errors.first() {
            st.announce(format!("Config error, {e}"));
        }
        self.state = Some(st);
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_none() {
            self.init(event_loop);
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserEvent) {
        let Some(st) = self.state.as_mut() else { return };
        match event {
            UserEvent::Wakeup(id) => st.on_wakeup(id),
            UserEvent::Exited(id) => {
                if let Some(i) = st.tabs.iter().position(|s| s.id == id) {
                    st.close_tab(i);
                }
            }
            UserEvent::ConfigChanged => st.reload_config(false),
            #[cfg(debug_assertions)]
            UserEvent::Debug(step) => st.debug_step(&step),
            UserEvent::AccessKit(ev) => match ev.window_event {
                accesskit_winit::WindowEvent::InitialTreeRequested => {
                    st.a11y_active = true;
                    st.a11y.reset();
                    st.push_a11y();
                }
                accesskit_winit::WindowEvent::AccessibilityDeactivated => st.a11y_active = false,
                accesskit_winit::WindowEvent::ActionRequested(req) => st.on_a11y_action(req),
            },
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(st) = self.state.as_mut() else { return };
        st.adapter.process_event(&st.window, &event);
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                st.renderer.resize(size.width, size.height);
                st.apply_size();
                st.a11y.reset();
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                st.scale = scale_factor;
                st.pad = (st.cfg.padding * scale_factor as f32).round();
                st.reload_font();
                st.a11y.reset();
            }
            WindowEvent::Focused(f) => {
                st.focused = f;
                st.blink_epoch = Instant::now();
                let report = st.session().shared.term.lock().unwrap().focus_events;
                if report {
                    st.write(if f { b"\x1b[I" } else { b"\x1b[O" });
                }
                st.window.request_redraw();
            }
            WindowEvent::ModifiersChanged(m) => {
                st.modifiers = m.state();
                st.update_hover();
            }
            WindowEvent::KeyboardInput { event, is_synthetic: false, .. } => st.on_key(&event),
            WindowEvent::CursorMoved { position, .. } => {
                st.window.set_cursor_visible(true);
                st.mouse_px = (position.x, position.y);
                st.on_mouse_move();
            }
            WindowEvent::MouseInput { state, button, .. } => st.on_mouse_button(state, button),
            WindowEvent::MouseWheel { delta, .. } => {
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, y) => (y * 3.0) as isize,
                    MouseScrollDelta::PixelDelta(p) => {
                        st.scroll_accum += p.y / st.glyphs.cell_h as f64;
                        let l = st.scroll_accum.trunc();
                        st.scroll_accum -= l;
                        l as isize
                    }
                };
                st.on_scroll(lines);
            }
            WindowEvent::RedrawRequested => st.redraw(),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let Some(st) = self.state.as_mut() else { return };
        if st.quit {
            event_loop.exit();
            return;
        }
        let now = Instant::now();
        let mut wait = st.next_deadline();
        if wait.is_some_and(|t| t <= now) {
            if st.flash_until.is_some_and(|t| t <= now) {
                st.flash_until = None;
            }
            st.window.request_redraw();
            wait = st.next_deadline().filter(|&t| t > now);
        }

        if st.a11y_active {
            let generation = st.session().shared.term.lock().unwrap().generation;
            let (g, u, tab) = st.a11y_sent;
            let id = st.session().id;
            if u != st.ui_gen || tab != id {
                // UI changes (menus, focus, tab switches) go out immediately.
                st.push_a11y();
            } else if g != generation {
                // Terminal output is throttled so floods can't swamp the bus.
                let due = st.a11y_last + A11Y_INTERVAL;
                if now >= due {
                    st.push_a11y();
                } else {
                    wait = Some(wait.map_or(due, |w| w.min(due)));
                }
            }
        }
        event_loop.set_control_flow(match wait {
            Some(t) => ControlFlow::WaitUntil(t),
            None => ControlFlow::Wait,
        });
    }
}

fn main() {
    // Headless parser benchmark: `rsterm --bench FILE`
    let args: Vec<String> = std::env::args().collect();
    if args.len() == 3 && args[1] == "--bench" {
        let data = std::fs::read(&args[2]).expect("read bench file");
        let mut term = Term::new(120, 40);
        let mut parser = vte::Parser::new();
        let t = Instant::now();
        for chunk in data.chunks(4096) {
            parser.advance(&mut term, chunk);
        }
        let dt = t.elapsed().as_secs_f64();
        println!(
            "{:.1} MB in {:.0} ms = {:.0} MB/s",
            data.len() as f64 / 1e6,
            dt * 1e3,
            data.len() as f64 / 1e6 / dt
        );
        return;
    }
    let event_loop = EventLoop::<UserEvent>::with_user_event().build().expect("event loop");
    let proxy = event_loop.create_proxy();
    let mut app = App { proxy, state: None };
    event_loop.run_app(&mut app).expect("run");
}
