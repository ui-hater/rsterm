//! rsterm — a GPU-rendered terminal emulator that exposes its text over AT-SPI.

mod a11y;
mod config;
mod dropdown;
mod hotkey;
mod font;
mod input;
mod keybind;
mod pty;
mod render;
mod session;
mod shell_integration;
mod term;
mod ui;

use config::{Config, Edge, TabBar};
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
/// How long to keep asking the window manager to focus a just-shown dropdown.
const FOCUS_RETRY: Duration = Duration::from_millis(1000);
/// Grace period before hide-on-focus-loss acts, so brief focus blips
/// (a notification, a screen reader popup) don't make the window vanish.
const HIDE_GRACE: Duration = Duration::from_millis(250);

#[derive(Debug)]
enum UserEvent {
    AccessKit(accesskit_winit::Event),
    Wakeup(u64),
    Exited(u64),
    ConfigChanged,
    Dropdown(dropdown::Cmd),
    /// A problem worth telling the user about (e.g. the hotkey grab failed).
    Notice(String),
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
    /// Cursor blink phase in the last drawn frame. `next_deadline` only knows
    /// when the *next* phase starts, so `about_to_wait` compares against this
    /// to tell that a phase change still has to be drawn.
    cursor_drawn: bool,
    flash_until: Option<Instant>,

    announcement: String,
    announce_flip: bool,
    a11y: a11y::A11y,
    a11y_active: bool,
    a11y_sent: (u64, u64, u64),
    a11y_last: Instant,

    /// Dropdown mode: the window is shown/hidden by `rsterm --dropdown`.
    dropdown: bool,
    shown: bool,
    /// Keep requesting focus until this time (or until we get it).
    focus_until: Option<Instant>,
    /// Hide-on-focus-loss deadline.
    hide_at: Option<Instant>,
    /// Sends a changed `dropdown-hotkey` to the hotkey thread.
    hotkey_tx: Option<std::sync::mpsc::Sender<String>>,
    /// How far the dropdown is slid in: 0 = off screen, 1 = in place.
    slide_pos: f32,
    /// Key bindings built from the defaults and `keybind` entries.
    binds: keybind::Bindings,
    /// Keys whose press ran a binding; their releases aren't sent to the
    /// program (kitty event reporting would otherwise see a lone release).
    consumed_keys: Vec<winit::keyboard::KeyCode>,
    /// The slide animation in progress, if any.
    slide: Option<Slide>,
    /// Monitor the dropdown belongs to. Remembered because while slid out
    /// the window is off screen (or over a neighbouring monitor).
    monitor: Option<winit::monitor::MonitorHandle>,
}

#[derive(Clone, Copy)]
struct Slide {
    from: f32,
    to: f32,
    start: Instant,
    dur: Duration,
}

impl Slide {
    /// Current position and whether the slide has finished.
    fn at(&self, now: Instant) -> (f32, bool) {
        let t = if self.dur.is_zero() {
            1.0
        } else {
            (now.saturating_duration_since(self.start).as_secs_f32() / self.dur.as_secs_f32()).min(1.0)
        };
        // Ease out (cubic): fast start, gentle stop.
        let e = 1.0 - (1.0 - t).powi(3);
        (self.from + (self.to - self.from) * e, t >= 1.0)
    }
}

fn font_px(pt: f32, scale: f64) -> f32 {
    (pt * 4.0 / 3.0 * scale as f32).round()
}

fn theme_colors(cfg: &Config) -> (Colors, UiColors) {
    let t = config::theme(&cfg.theme).unwrap_or(&config::THEMES[0]);
    let fg = cfg.foreground.unwrap_or(t.fg);
    let bg = cfg.background.unwrap_or(t.bg);
    let cursor = cfg.cursor_color.unwrap_or(t.cursor);
    let c = Colors { palette: config::palette(t), fg, bg, cursor };
    let (sel_fg, sel_bg) = (cfg.selection_fg.unwrap_or(t.selection_fg), cfg.selection_bg.unwrap_or(t.selection_bg));
    (c, UiColors::new(fg, bg, sel_fg, sel_bg))
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
        if !self.dropdown {
            self.window.set_resize_increments(Some(winit::dpi::PhysicalSize::new(
                self.glyphs.cell_w,
                self.glyphs.cell_h,
            )));
        }
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
                shell_integration: self.cfg.shell_integration,
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
        // The dropdown lives on (it owns the hotkey): when its last shell
        // exits, start a fresh one and hide. Quit (Ctrl+Shift+Q) still exits.
        let mut respawned = false;
        if self.tabs.is_empty()
            && self.dropdown
            && let Ok(s) = self.spawn_session(None)
        {
            self.tabs.push(s);
            respawned = true;
        }
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
        if respawned {
            self.hide_dropdown();
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
        let (colors, ui_colors) = theme_colors(&self.cfg);
        if colors != self.colors || ui_colors != self.ui_colors {
            (self.colors, self.ui_colors) = (colors, ui_colors);
            for s in &self.tabs {
                s.shared.term.lock().unwrap().set_colors(self.colors);
            }
        }
        if self.dropdown {
            let c = &self.cfg;
            let moved = (c.dropdown_width, c.dropdown_height, c.dropdown_position, c.dropdown_always_on_top)
                != (old.dropdown_width, old.dropdown_height, old.dropdown_position, old.dropdown_always_on_top);
            if moved {
                self.place_dropdown();
            }
            let c = &self.cfg;
            if !c.dropdown_hide_on_focus_loss {
                self.hide_at = None;
            }
            if c.dropdown_hotkey != old.dropdown_hotkey {
                if let Some(tx) = &self.hotkey_tx {
                    let _ = tx.send(c.dropdown_hotkey.clone());
                }
            }
        } else if old.decorations != self.cfg.decorations {
            self.window.set_decorations(self.cfg.decorations);
        }
        if old.dropdown_autostart != self.cfg.dropdown_autostart {
            if let Err(e) = dropdown::set_autostart(self.cfg.dropdown_autostart) {
                self.announce(format!("Could not change autostart: {e}"));
            }
        }
        if old.keybinds != self.cfg.keybinds {
            self.binds = keybind::Bindings::new(&self.cfg.keybinds);
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
            Action::ScrollPageUp | Action::ScrollPageDown | Action::ScrollTop | Action::ScrollBottom => {
                let mut t = self.session().shared.term.lock().unwrap();
                let page = t.rows() as isize - 1;
                t.scroll_view(match a {
                    Action::ScrollPageUp => page,
                    Action::ScrollPageDown => -page,
                    Action::ScrollTop => isize::MAX / 2,
                    _ => -(isize::MAX / 2),
                });
                drop(t);
                self.window.request_redraw();
            }
            Action::PrevPrompt | Action::NextPrompt => {
                let dir = if a == Action::PrevPrompt { -1 } else { 1 };
                let r = self.session().shared.term.lock().unwrap().jump_to_prompt(dir);
                self.window.request_redraw();
                self.announce(match r {
                    Some(text) if text.is_empty() => "Bottom".to_string(),
                    Some(text) => text,
                    None if dir < 0 => "No previous prompt".to_string(),
                    None => "No next prompt".to_string(),
                });
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
                    Some(at) => Overlay::Menu(ui::MenuState { sel: 0, at, shortcuts: self.menu_shortcuts(), scroll: 0 }),
                    None => Overlay::None,
                };
                self.touch_ui();
            }
            Action::Quit => self.quit = true,
        }
    }

    /// Run a bound action if it applies right now. Returns false when it
    /// doesn't (e.g. Alt+3 with fewer than 3 tabs, Shift+PageUp on the
    /// alternate screen), so the key goes to the program instead.
    fn perform(&mut self, a: Action) -> bool {
        let applies = match a {
            Action::GotoTab(i) => self.tabs.len() > 1 && (i == usize::MAX || i < self.tabs.len()),
            Action::ScrollPageUp | Action::ScrollPageDown | Action::ScrollTop | Action::ScrollBottom => {
                !self.session().shared.term.lock().unwrap().alt_active
            }
            _ => true,
        };
        if applies {
            self.run(a);
        }
        applies
    }

    fn menu_shortcuts(&self) -> Vec<String> {
        ui::MENU.iter().map(|it| it.action.map(|a| self.binds.label(a)).unwrap_or_default()).collect()
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
        let code = match ev.physical_key {
            winit::keyboard::PhysicalKey::Code(c) => Some(c),
            _ => None,
        };
        let bound = code.and_then(|c| self.binds.lookup(keybind::mod_bits(self.modifiers), c)).cloned();
        if ev.state != ElementState::Pressed {
            if let Some(i) = code.and_then(|c| self.consumed_keys.iter().position(|&k| k == c)) {
                self.consumed_keys.swap_remove(i);
                return;
            }
            // Releases only matter to programs using kitty event reporting.
            if matches!(self.overlay, Overlay::None) {
                let (flags, app_cursor) = {
                    let t = self.session().shared.term.lock().unwrap();
                    (t.kitty_flags(), t.app_cursor)
                };
                if flags & input::kitty::EVENT_TYPES != 0 {
                    let bytes = input::encode_kitty(ev, self.modifiers, flags, app_cursor);
                    if !bytes.is_empty() {
                        self.write(&bytes);
                    }
                }
            }
            return;
        }
        self.blink_epoch = Instant::now();
        if !matches!(self.overlay, Overlay::None) {
            // Clipboard paste works inside search / text fields.
            if bound == Some(keybind::Bound::Action(Action::Paste)) {
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
        let consumed = match bound {
            Some(keybind::Bound::Action(a)) => self.perform(a),
            Some(keybind::Bound::Text(bytes)) => {
                self.write(&bytes);
                true
            }
            Some(keybind::Bound::Ignore) => true,
            None => false,
        };
        if consumed {
            if let Some(c) = code.filter(|c| !self.consumed_keys.contains(c)) {
                self.consumed_keys.push(c);
            }
            return;
        }
        let (flags, app_cursor) = {
            let t = self.session().shared.term.lock().unwrap();
            (t.kitty_flags(), t.app_cursor)
        };
        let bytes = if flags != 0 {
            input::encode_kitty(ev, self.modifiers, flags, app_cursor)
        } else {
            input::encode(ev, self.modifiers, app_cursor)
        };
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
        if self.cfg.mouse_hide_while_typing {
            self.window.set_cursor_visible(false);
        }
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
                            match cfg.set(key, v.trim()) {
                                Ok(()) => {
                                    self.apply_config(cfg, true);
                                    self.announce("Saved");
                                }
                                Err(e) => self.announce(format!("Not saved: {e}")),
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
                            let spoken = s.choice_value(&cfg);
                            self.apply_config(cfg, true);
                            if let Some(v) = spoken {
                                self.announce(v);
                            }
                        }
                    }
                    Some(NamedKey::Enter) | Some(NamedKey::Space) => match s.items[s.sel].kind {
                        Kind::Text => s.editing = Some(self.cfg.get(s.items[s.sel].key)),
                        _ => {
                            let mut cfg = self.cfg.clone();
                            if s.adjust(&mut cfg, 1) {
                                let spoken = s.choice_value(&cfg);
                                self.apply_config(cfg, true);
                                if let Some(v) = spoken {
                                    self.announce(v);
                                }
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
                let action = inside.then(|| ui::MENU.get(m.scroll + cell.1 - y - 1).and_then(|i| i.action)).flatten();
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
                } else if let Some(row) = cell.1.checked_sub(y + ui::settings_layout(h).top)
                    .filter(|&r| r < ui::settings_layout(h).vis)
                {
                    let i = s.scroll + row;
                    if i < s.items.len() {
                        let was = s.sel;
                        s.sel = i;
                        if was == i && !matches!(s.items[i].kind, Kind::Text) {
                            let dir = if button == MouseButton::Right { -1 } else { 1 };
                            let mut cfg = self.cfg.clone();
                            if s.adjust(&mut cfg, dir) {
                                let spoken = s.choice_value(&cfg);
                                self.apply_config(cfg, true);
                                if let Some(v) = spoken {
                                    self.announce(v);
                                }
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
                self.overlay = Overlay::Menu(ui::MenuState { sel: 0, at: cell, shortcuts: self.menu_shortcuts(), scroll: 0 });
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
                let i = m.scroll + cell.1 - y - 1;
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
        if let Overlay::Menu(m) = &mut self.overlay {
            let dir = if lines > 0 { -1 } else { 1 };
            for _ in 0..lines.unsigned_abs() {
                m.sel = ui::menu_step(m.sel, dir);
            }
            self.touch_ui();
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
        let (title, bell, clip, notes) = {
            let mut t = shared.term.lock().unwrap();
            let title = std::mem::take(&mut t.title_changed).then(|| t.title.clone());
            (title, std::mem::take(&mut t.bell), t.clipboard_request.take(), std::mem::take(&mut t.notifications))
        };
        if self.cfg.desktop_notifications {
            for (t, body) in notes {
                self.notify(i, t, body);
            }
        }
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

    /// Show an OSC 9 / 777 notification from tab `i`. While the user is
    /// looking at that tab it is announced (spoken, not a popup); otherwise it
    /// becomes a desktop notification, which screen readers also read.
    fn notify(&mut self, i: usize, title: String, body: String) {
        let looking = i == self.active && self.focused && (self.shown || !self.dropdown);
        if looking {
            let msg = match (title.is_empty(), body.is_empty()) {
                (true, _) => body,
                (false, true) => title,
                (false, false) => format!("{title}: {body}"),
            };
            self.announce(msg);
            return;
        }
        let summary = if title.is_empty() { self.tabs[i].title() } else { title };
        let summary = if summary.is_empty() { "rsterm".to_string() } else { summary };
        let _ = std::process::Command::new("notify-send")
            .args(["--app-name=rsterm", "--icon=utilities-terminal", "--"])
            .arg(summary)
            .arg(body)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map(|mut c| std::thread::spawn(move || c.wait()));
    }

    // -- frame ----------------------------------------------------------------------

    /// Whether the cursor blinks: the application's request (DECSCUSR /
    /// mode 12) wins over `cursor-style-blink`.
    fn cursor_blinks(&self, term: &Term) -> bool {
        self.focused && term.cursor_blink.unwrap_or(self.cfg.cursor_blink)
    }

    fn cursor_on(&self, term: &Term) -> bool {
        !self.cursor_blinks(term)
            || (self.blink_epoch.elapsed().as_millis() / BLINK.as_millis()) % 2 == 0
    }

    fn redraw(&mut self) {
        let (cols, rows) = self.window_grid();
        let bar = self.bar_rows();
        let flash = self.flash_until.is_some_and(|t| Instant::now() < t);
        let shared = self.session().shared.clone();
        let term = shared.term.lock().unwrap();
        let cursor_on = self.cursor_on(&term);
        self.cursor_drawn = cursor_on;

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
        // While the dropdown is hidden its tree is frozen: Orca would otherwise
        // hear text changes from a window the user can't see. The incremental
        // diff catches up in one update when it's shown again.
        if !self.a11y_active || (self.dropdown && !self.shown) {
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
            dropdown: self.dropdown,
        };
        let scroll = self.a11y.scroll(&term, &g, &view);
        let update = self.a11y.update(&term, &g, &view);
        self.a11y_sent = (term.generation, self.ui_gen, self.session().id);
        self.a11y_last = Instant::now();
        drop(term);
        if let Some(scroll) = scroll {
            self.adapter.update_if_active(|| scroll);
        }
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

    // -- dropdown -------------------------------------------------------------------

    /// Geometry on the dropdown's monitor: (x, shown y, hidden y, w, h).
    fn dropdown_geometry(&mut self) -> Option<(i32, i32, i32, u32, u32)> {
        let valid = |m: &winit::monitor::MonitorHandle| m.size().width > 0;
        if !self.monitor.as_ref().is_some_and(valid) {
            self.monitor = self.window.current_monitor().filter(valid).or_else(|| self.window.primary_monitor());
        }
        let mon = self.monitor.as_ref()?;
        let (mp, ms) = (mon.position(), mon.size());
        let w = (ms.width as f32 * self.cfg.dropdown_width / 100.0).round() as u32;
        let h = (ms.height as f32 * self.cfg.dropdown_height / 100.0).round() as u32;
        let x = mp.x + (ms.width.saturating_sub(w) / 2) as i32;
        Some(match self.cfg.dropdown_position {
            Edge::Top => (x, mp.y, mp.y - h as i32, w, h),
            Edge::Bottom => (x, mp.y + ms.height.saturating_sub(h) as i32, mp.y + ms.height as i32, w, h),
        })
    }

    /// Size and position the dropdown on its monitor.
    fn place_dropdown(&mut self) {
        let Some((_, _, _, w, h)) = self.dropdown_geometry() else { return };
        let _ = self.window.request_inner_size(winit::dpi::PhysicalSize::new(w, h));
        self.window.set_window_level(if self.cfg.dropdown_always_on_top {
            winit::window::WindowLevel::AlwaysOnTop
        } else {
            winit::window::WindowLevel::Normal
        });
        self.move_dropdown();
    }

    /// Move the dropdown to where `slide_pos` puts it.
    fn move_dropdown(&mut self) {
        let Some((x, shown, hidden, _, _)) = self.dropdown_geometry() else { return };
        let y = hidden + ((shown - hidden) as f32 * self.slide_pos).round() as i32;
        self.window.set_outer_position(winit::dpi::PhysicalPosition::new(x, y));
    }

    /// Shown, and not on its way out.
    fn dropdown_visible(&self) -> bool {
        self.shown && !self.slide.is_some_and(|s| s.to == 0.0)
    }

    fn dropdown_cmd(&mut self, cmd: dropdown::Cmd) {
        let show = match cmd {
            dropdown::Cmd::Show => true,
            dropdown::Cmd::Hide => false,
            // A shown-but-unfocused dropdown (behind nothing, but focus went
            // elsewhere) is brought to focus rather than hidden.
            dropdown::Cmd::Toggle => !(self.dropdown_visible() && self.focused),
        };
        if show { self.show_dropdown() } else { self.hide_dropdown() }
    }

    /// Animate `slide_pos` towards `to` (0 or 1), or jump there if
    /// animations are off.
    fn start_slide(&mut self, to: f32, now: Instant) {
        let secs = self.cfg.dropdown_animation * (to - self.slide_pos).abs();
        if secs <= 0.0 {
            self.slide = None;
            self.slide_pos = to;
            self.move_dropdown();
            if to == 0.0 {
                self.finish_hide();
            }
            return;
        }
        self.slide = Some(Slide { from: self.slide_pos, to, start: now, dur: Duration::from_secs_f32(secs) });
    }

    fn show_dropdown(&mut self) {
        self.hide_at = None;
        if !self.shown {
            self.shown = true;
            // Pick the monitor afresh each time it appears.
            self.monitor = None;
            self.slide_pos = if self.cfg.dropdown_animation > 0.0 { 0.0 } else { 1.0 };
            self.place_dropdown();
            self.window.set_visible(true);
            // Re-place after mapping: some WMs ignore geometry of unmapped windows.
            self.place_dropdown();
            self.last_frame = 0;
            self.touch_ui();
        }
        self.start_slide(1.0, Instant::now());
        // focus_window is a no-op until the WM has mapped the window, so keep
        // asking (from about_to_wait) until Focused(true) arrives. Focus is
        // what makes Orca announce the window and start tracking the terminal.
        self.window.focus_window();
        self.focus_until = Some(Instant::now() + FOCUS_RETRY);
    }

    /// Start hiding: slide out, then unmap (`finish_hide`).
    fn hide_dropdown(&mut self) {
        self.hide_at = None;
        self.focus_until = None;
        if !self.dropdown_visible() {
            return;
        }
        self.overlay = Overlay::None;
        self.selecting = false;
        self.mouse_buttons = 0;
        self.start_slide(0.0, Instant::now());
    }

    fn finish_hide(&mut self) {
        if !self.shown {
            return;
        }
        self.shown = false;
        self.window.set_visible(false);
        // Unmapping doesn't always deliver FocusOut; tell AccessKit directly so
        // the AT-SPI window is deactivated and Orca stops treating it as active.
        if self.focused {
            self.focused = false;
            self.adapter.process_event(&self.window, &WindowEvent::Focused(false));
        }
    }

    /// Dropdown timers: slide animation, focus retries and hide-on-focus-loss.
    fn dropdown_tick(&mut self, now: Instant) {
        if let Some(sl) = self.slide {
            let (pos, done) = sl.at(now);
            self.slide_pos = pos;
            self.move_dropdown();
            if done {
                self.slide = None;
                if sl.to == 0.0 {
                    self.finish_hide();
                }
            }
        }
        if let Some(until) = self.focus_until {
            if self.focused || !self.shown {
                self.focus_until = None;
            } else if now >= until {
                self.focus_until = None;
                // The WM refused focus; at least flag the window.
                self.window.request_user_attention(Some(winit::window::UserAttentionType::Critical));
            } else {
                self.window.focus_window();
            }
        }
        if self.hide_at.is_some_and(|t| now >= t) {
            self.hide_at = None;
            if !self.focused {
                self.hide_dropdown();
            }
        }
    }

    /// Earliest time something needs to happen without input.
    fn next_deadline(&self) -> Option<Instant> {
        let mut d: Vec<Instant> = Vec::new();
        let term = self.session().shared.term.lock().unwrap();
        if self.cursor_blinks(&term) {
            let n = self.blink_epoch.elapsed().as_millis() / BLINK.as_millis() + 1;
            d.push(self.blink_epoch + BLINK * n as u32);
        }
        if let Some(t) = term.sync_since {
            d.push(t + SYNC_TIMEOUT);
        }
        drop(term);
        if let Some(t) = self.flash_until {
            d.push(t);
        }
        if let Some(t) = self.hide_at {
            d.push(t);
        }
        if self.focus_until.is_some() {
            d.push(Instant::now() + Duration::from_millis(50));
        }
        if self.slide.is_some() {
            // ~120 Hz; the WM/compositor shows it at its own rate.
            d.push(Instant::now() + Duration::from_millis(8));
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
                    "PrevPrompt" => Action::PrevPrompt,
                    "NextPrompt" => Action::NextPrompt,
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
            "d" => match dropdown::Cmd::parse(arg) {
                Some(c) => self.dropdown_cmd(c),
                None => eprintln!("debug: bad dropdown command {arg}"),
            },
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
    dropdown: bool,
    /// The command the dropdown was started with (`hide` starts hidden).
    initial: Option<dropdown::Cmd>,
}

impl App {
    fn init(&mut self, event_loop: &ActiveEventLoop) {
        use winit::platform::x11::{WindowAttributesExtX11, WindowType};
        Config::create_if_missing();
        let (cfg, errors) = Config::load();
        let dropdown = self.dropdown;
        let mut attrs = Window::default_attributes()
            .with_title(if dropdown { "rsterm dropdown" } else { "rsterm" })
            .with_visible(false)
            .with_decorations(cfg.decorations && !dropdown)
            // WM_CLASS / app_id, so window rules can target the dropdown.
            .with_name("rsterm", if dropdown { "rsterm-dropdown" } else { "rsterm" });
        if dropdown {
            // Utility windows stay out of the taskbar and Alt+Tab but still
            // take keyboard focus (unlike Dock windows).
            attrs = attrs.with_x11_window_type(vec![WindowType::Utility]);
        }
        let window = Arc::new(event_loop.create_window(attrs).expect("create window"));
        // The AccessKit adapter must exist before the window is first shown.
        let adapter = accesskit_winit::Adapter::with_event_loop_proxy(event_loop, &window, self.proxy.clone());

        let scale = window.scale_factor();
        let glyphs = font::Glyphs::new(&cfg.font_family, font_px(cfg.font_size, scale));
        let pad = (cfg.padding * scale as f32).round();
        if !dropdown {
            window.set_resize_increments(Some(winit::dpi::PhysicalSize::new(glyphs.cell_w, glyphs.cell_h)));
            let w = cfg.window_cols as f32 * glyphs.cell_w as f32 + 2.0 * pad;
            let h = cfg.window_rows as f32 * glyphs.cell_h as f32 + 2.0 * pad;
            let _ = window.request_inner_size(winit::dpi::PhysicalSize::new(w as u32, h as u32));
            window.set_visible(true);
        }
        let renderer = Renderer::new(window.clone());
        let (colors, ui_colors) = theme_colors(&cfg);

        let proxy = self.proxy.clone();
        config::watch(move || {
            let _ = proxy.send_event(UserEvent::ConfigChanged);
        });

        #[cfg(debug_assertions)]
        start_debug_script(self.proxy.clone());
        let binds = keybind::Bindings::new(&cfg.keybinds);
        let mut st = State {
            window,
            renderer,
            glyphs,
            adapter,
            proxy: self.proxy.clone(),
            pad,
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
            cursor_drawn: true,
            flash_until: None,
            announcement: String::new(),
            announce_flip: false,
            a11y: a11y::A11y::default(),
            a11y_active: false,
            a11y_sent: (0, 0, u64::MAX),
            a11y_last: Instant::now(),
            dropdown,
            shown: false,
            focus_until: None,
            hide_at: None,
            hotkey_tx: None,
            binds,
            consumed_keys: Vec::new(),
            slide_pos: 1.0,
            slide: None,
            monitor: None,
        };
        let s = st.spawn_session(None).expect("spawn shell");
        st.tabs.push(s);
        if let Err(e) = dropdown::set_autostart(st.cfg.dropdown_autostart) {
            eprintln!("rsterm: could not update autostart entry: {e}");
        }
        if dropdown {
            if std::env::var_os("DISPLAY").is_some() {
                let (tx, rx) = std::sync::mpsc::channel();
                let p = self.proxy.clone();
                hotkey::spawn(st.cfg.dropdown_hotkey.clone(), rx, move |m| {
                    let _ = p.send_event(match m {
                        hotkey::Msg::Pressed => UserEvent::Dropdown(dropdown::Cmd::Toggle),
                        hotkey::Msg::Error(e) => UserEvent::Notice(e),
                    });
                });
                st.hotkey_tx = Some(tx);
            }
            // The first `rsterm --dropdown` is itself a hotkey press: appear,
            // unless started with `hide` (autostart at login).
            if self.initial != Some(dropdown::Cmd::Hide) {
                st.show_dropdown();
            }
        }
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
            UserEvent::Dropdown(cmd) => st.dropdown_cmd(cmd),
            UserEvent::Notice(msg) => {
                eprintln!("rsterm: {msg}");
                st.announce(msg);
            }
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
                if st.dropdown {
                    if f {
                        st.focus_until = None;
                        st.hide_at = None;
                    } else if st.shown && st.cfg.dropdown_hide_on_focus_loss {
                        st.hide_at = Some(Instant::now() + HIDE_GRACE);
                    }
                }
                st.focused = f;
                st.consumed_keys.clear();
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
                    MouseScrollDelta::LineDelta(_, y) => (y * st.cfg.scroll_multiplier).round() as isize,
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
        if st.dropdown {
            st.dropdown_tick(now);
        }
        let mut wait = st.next_deadline();
        let blink_due = {
            let term = st.session().shared.term.lock().unwrap();
            st.cursor_on(&term) != st.cursor_drawn
        };
        if blink_due || wait.is_some_and(|t| t <= now) {
            if st.flash_until.is_some_and(|t| t <= now) {
                st.flash_until = None;
            }
            st.window.request_redraw();
            wait = st.next_deadline().filter(|&t| t > now);
        }

        if st.a11y_active && (st.shown || !st.dropdown) {
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
    if args.get(1).is_some_and(|a| a == "--help" || a == "-h") {
        println!(
            "usage: rsterm [--dropdown [toggle|show|hide]] [--bench FILE]\n\n  \
             --dropdown   quake-style terminal: the first call starts it, later calls\n               \
             toggle/show/hide it. The dropdown grabs its own global hotkey\n               \
             (config `dropdown-hotkey`, default F12); `--dropdown hide` starts it\n               \
             hidden (used by `dropdown-autostart`)."
        );
        return;
    }
    let mut instance = None;
    let mut initial = None;
    let dropdown = args.get(1).is_some_and(|a| a == "--dropdown");
    if dropdown {
        let cmd = match args.get(2) {
            None => dropdown::Cmd::Toggle,
            Some(a) => dropdown::Cmd::parse(a).unwrap_or_else(|| {
                eprintln!("rsterm: unknown dropdown command {a:?} (expected toggle, show or hide)");
                std::process::exit(2)
            }),
        };
        initial = Some(cmd);
        match dropdown::claim(cmd) {
            Ok(dropdown::Role::Forwarded) => return,
            Ok(dropdown::Role::Primary(i)) => instance = Some(i),
            Err(e) => {
                eprintln!("rsterm: could not reach or start the dropdown instance: {e}");
                std::process::exit(1)
            }
        }
    }
    let mut builder = EventLoop::<UserEvent>::with_user_event();
    if dropdown && std::env::var_os("DISPLAY").is_some() {
        // Wayland clients can neither hide (unmap) nor position their own
        // windows, so the dropdown runs through X11/XWayland where both work.
        use winit::platform::x11::EventLoopBuilderExtX11;
        builder.with_x11();
    }
    let event_loop = builder.build().expect("event loop");
    let proxy = event_loop.create_proxy();
    if let Some(i) = instance {
        let p = proxy.clone();
        i.serve(move |cmd| {
            let _ = p.send_event(UserEvent::Dropdown(cmd));
        });
    }
    let mut app = App { proxy, state: None, dropdown, initial };
    event_loop.run_app(&mut app).expect("run");
}
