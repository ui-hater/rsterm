//! Configuration: a Ghostty-style `key = value` file at
//! `$XDG_CONFIG_HOME/rsterm/config` (default `~/.config/rsterm/config`),
//! built-in color themes, and a polling watcher for hot reload.

use crate::term::CursorShape;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TabBar {
    Auto,
    Always,
    Never,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Edge {
    Top,
    Bottom,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub font_family: String,
    pub font_size: f32,
    pub theme: String,
    pub scrollback: usize,
    pub cursor_style: CursorShape,
    pub cursor_blink: bool,
    pub padding: f32,
    pub tab_bar: TabBar,
    pub copy_on_select: bool,
    pub visual_bell: bool,
    pub bold_is_bright: bool,
    pub inherit_cwd: bool,
    pub clipboard_write: bool,
    pub command: String,
    /// Initial window size in cells.
    pub window_cols: usize,
    pub window_rows: usize,
    pub decorations: bool,
    pub mouse_hide_while_typing: bool,
    /// Lines scrolled per mouse wheel notch.
    pub scroll_multiplier: f32,
    /// Overrides for the theme's colors (None = use the theme).
    pub foreground: Option<u32>,
    pub background: Option<u32>,
    pub cursor_color: Option<u32>,
    pub selection_fg: Option<u32>,
    pub selection_bg: Option<u32>,
    /// Dropdown window size as a percentage of the monitor.
    pub dropdown_width: f32,
    pub dropdown_height: f32,
    pub dropdown_position: Edge,
    pub dropdown_hide_on_focus_loss: bool,
    pub dropdown_always_on_top: bool,
    /// Global hotkey that toggles the dropdown (empty = none).
    pub dropdown_hotkey: String,
    /// Start the dropdown hidden at login (XDG autostart entry).
    pub dropdown_autostart: bool,
    /// Seconds the dropdown takes to slide in or out (0 = no animation).
    pub dropdown_animation: f32,
    /// Show OSC 9 / OSC 777 notifications from programs.
    pub desktop_notifications: bool,
    /// Inject prompt marks (OSC 133) into zsh and bash.
    pub shell_integration: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            font_family: "monospace".into(),
            font_size: 11.0,
            theme: "Tomorrow Night".into(),
            scrollback: 10_000,
            cursor_style: CursorShape::Block,
            cursor_blink: false,
            padding: 4.0,
            tab_bar: TabBar::Auto,
            copy_on_select: true,
            visual_bell: false,
            bold_is_bright: false,
            inherit_cwd: true,
            clipboard_write: true,
            command: String::new(),
            window_cols: 100,
            window_rows: 30,
            decorations: true,
            mouse_hide_while_typing: true,
            scroll_multiplier: 3.0,
            foreground: None,
            background: None,
            cursor_color: None,
            selection_fg: None,
            selection_bg: None,
            dropdown_width: 100.0,
            dropdown_height: 40.0,
            dropdown_position: Edge::Top,
            // Off by default: a window vanishing whenever focus moves (e.g. to
            // a screen reader popup or a notification) is disorienting.
            dropdown_hide_on_focus_loss: false,
            dropdown_always_on_top: true,
            dropdown_hotkey: "F12".into(),
            dropdown_autostart: false,
            dropdown_animation: 0.2,
            desktop_notifications: true,
            shell_integration: true,
        }
    }
}

pub fn config_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"));
    base.join("rsterm").join("config")
}

fn parse_bool(v: &str) -> Option<bool> {
    match v.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

pub fn cursor_name(c: CursorShape) -> &'static str {
    match c {
        CursorShape::Block => "block",
        CursorShape::Underline => "underline",
        CursorShape::Bar => "bar",
    }
}

pub fn edge_name(e: Edge) -> &'static str {
    match e {
        Edge::Top => "top",
        Edge::Bottom => "bottom",
    }
}

/// Parse `#rrggbb` / `rrggbb`; empty means "unset".
fn parse_color(v: &str) -> Result<Option<u32>, ()> {
    if v.is_empty() {
        return Ok(None);
    }
    let h = v.strip_prefix('#').unwrap_or(v);
    if h.len() != 6 {
        return Err(());
    }
    u32::from_str_radix(h, 16).map(Some).map_err(|_| ())
}

fn color_name(c: Option<u32>) -> String {
    c.map(|c| format!("#{c:06x}")).unwrap_or_default()
}

/// Every config key, in the order `save` writes them. `set` and `get` must
/// handle each of these.
pub const KEYS: &[&str] = &[
    "font-family",
    "font-size",
    "theme",
    "foreground",
    "background",
    "cursor-color",
    "selection-foreground",
    "selection-background",
    "scrollback-limit",
    "cursor-style",
    "cursor-style-blink",
    "window-padding",
    "window-width",
    "window-height",
    "window-decorations",
    "tab-bar",
    "copy-on-select",
    "mouse-hide-while-typing",
    "mouse-scroll-multiplier",
    "visual-bell",
    "bold-is-bright",
    "window-inherit-working-directory",
    "clipboard-write",
    "command",
    "dropdown-width",
    "dropdown-height",
    "dropdown-position",
    "dropdown-hide-on-focus-loss",
    "dropdown-always-on-top",
    "dropdown-hotkey",
    "dropdown-autostart",
    "dropdown-animation-duration",
    "desktop-notifications",
    "shell-integration",
];

pub fn tab_bar_name(t: TabBar) -> &'static str {
    match t {
        TabBar::Auto => "auto",
        TabBar::Always => "always",
        TabBar::Never => "never",
    }
}

impl Config {
    /// Load the config file (missing file = defaults). Returns the config and
    /// a list of human-readable problems found while parsing.
    pub fn load() -> (Config, Vec<String>) {
        let mut cfg = Config::default();
        let mut errors = Vec::new();
        let Ok(text) = std::fs::read_to_string(config_path()) else { return (cfg, errors) };
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else {
                errors.push(format!("line {}: expected key = value", n + 1));
                continue;
            };
            let (k, v) = (k.trim(), v.trim().trim_matches('"'));
            if let Err(e) = cfg.set(k, v) {
                errors.push(format!("line {}: {e}", n + 1));
            }
        }
        env_override("RSTERM_FONT", &mut cfg.font_family);
        if let Some(sz) = std::env::var("RSTERM_FONT_SIZE").ok().and_then(|s| s.parse().ok()) {
            cfg.font_size = sz;
        }
        (cfg, errors)
    }

    /// On first start, write a config file listing every key with its
    /// default value, so there is something to find and edit.
    pub fn create_if_missing() {
        let path = config_path();
        if path.exists() {
            return;
        }
        if let Err(e) = Config::default().save() {
            eprintln!("rsterm: could not create {}: {e}", path.display());
        }
    }

    pub fn set(&mut self, key: &str, v: &str) -> Result<(), String> {
        let bad = || format!("invalid value {v:?} for {key}");
        match key {
            "font-family" => self.font_family = v.to_string(),
            "font-size" => self.font_size = v.parse::<f32>().map_err(|_| bad())?.clamp(4.0, 96.0),
            "theme" => {
                if theme(v).is_none() {
                    return Err(format!("unknown theme {v:?}"));
                }
                self.theme = theme(v).unwrap().name.to_string();
            }
            "scrollback-limit" => self.scrollback = v.parse::<usize>().map_err(|_| bad())?.min(1_000_000),
            "cursor-style" => {
                self.cursor_style = match v {
                    "block" => CursorShape::Block,
                    "underline" => CursorShape::Underline,
                    "bar" => CursorShape::Bar,
                    _ => return Err(bad()),
                }
            }
            "cursor-style-blink" => self.cursor_blink = parse_bool(v).ok_or_else(bad)?,
            "window-padding" => self.padding = v.parse::<f32>().map_err(|_| bad())?.clamp(0.0, 100.0),
            "tab-bar" => {
                self.tab_bar = match v {
                    "auto" => TabBar::Auto,
                    "always" => TabBar::Always,
                    "never" => TabBar::Never,
                    _ => return Err(bad()),
                }
            }
            "copy-on-select" => self.copy_on_select = parse_bool(v).ok_or_else(bad)?,
            "visual-bell" => self.visual_bell = parse_bool(v).ok_or_else(bad)?,
            "bold-is-bright" => self.bold_is_bright = parse_bool(v).ok_or_else(bad)?,
            "window-inherit-working-directory" => self.inherit_cwd = parse_bool(v).ok_or_else(bad)?,
            "clipboard-write" => self.clipboard_write = parse_bool(v).ok_or_else(bad)?,
            "command" => self.command = v.to_string(),
            "window-width" => self.window_cols = v.parse::<usize>().map_err(|_| bad())?.clamp(10, 1000),
            "window-height" => self.window_rows = v.parse::<usize>().map_err(|_| bad())?.clamp(2, 500),
            "window-decorations" => self.decorations = parse_bool(v).ok_or_else(bad)?,
            "mouse-hide-while-typing" => self.mouse_hide_while_typing = parse_bool(v).ok_or_else(bad)?,
            "mouse-scroll-multiplier" => {
                self.scroll_multiplier = v.parse::<f32>().map_err(|_| bad())?.clamp(0.1, 50.0)
            }
            "foreground" => self.foreground = parse_color(v).map_err(|_| bad())?,
            "background" => self.background = parse_color(v).map_err(|_| bad())?,
            "cursor-color" => self.cursor_color = parse_color(v).map_err(|_| bad())?,
            "selection-foreground" => self.selection_fg = parse_color(v).map_err(|_| bad())?,
            "selection-background" => self.selection_bg = parse_color(v).map_err(|_| bad())?,
            "dropdown-width" => self.dropdown_width = v.parse::<f32>().map_err(|_| bad())?.clamp(10.0, 100.0),
            "dropdown-height" => self.dropdown_height = v.parse::<f32>().map_err(|_| bad())?.clamp(10.0, 100.0),
            "dropdown-position" => {
                self.dropdown_position = match v {
                    "top" => Edge::Top,
                    "bottom" => Edge::Bottom,
                    _ => return Err(bad()),
                }
            }
            "dropdown-hide-on-focus-loss" => self.dropdown_hide_on_focus_loss = parse_bool(v).ok_or_else(bad)?,
            "dropdown-always-on-top" => self.dropdown_always_on_top = parse_bool(v).ok_or_else(bad)?,
            "dropdown-hotkey" => {
                if !v.is_empty() && crate::hotkey::parse(v).is_none() {
                    return Err(bad());
                }
                self.dropdown_hotkey = v.to_string();
            }
            "dropdown-autostart" => self.dropdown_autostart = parse_bool(v).ok_or_else(bad)?,
            "dropdown-animation-duration" => {
                self.dropdown_animation = v.parse::<f32>().map_err(|_| bad())?.clamp(0.0, 2.0)
            }
            "desktop-notifications" => self.desktop_notifications = parse_bool(v).ok_or_else(bad)?,
            "shell-integration" => {
                self.shell_integration = match v {
                    "detect" => true,
                    "none" => false,
                    _ => return Err(bad()),
                }
            }
            _ => return Err(format!("unknown key {key:?}")),
        }
        Ok(())
    }

    /// Current value of `key`, formatted as it appears in the config file.
    pub fn get(&self, key: &str) -> String {
        let b = |v: bool| (if v { "true" } else { "false" }).to_string();
        match key {
            "font-family" => self.font_family.clone(),
            "font-size" => crate::ui::fmt_num(self.font_size as f64),
            "theme" => self.theme.clone(),
            "scrollback-limit" => self.scrollback.to_string(),
            "cursor-style" => cursor_name(self.cursor_style).into(),
            "cursor-style-blink" => b(self.cursor_blink),
            "window-padding" => crate::ui::fmt_num(self.padding as f64),
            "tab-bar" => tab_bar_name(self.tab_bar).into(),
            "copy-on-select" => b(self.copy_on_select),
            "visual-bell" => b(self.visual_bell),
            "bold-is-bright" => b(self.bold_is_bright),
            "window-inherit-working-directory" => b(self.inherit_cwd),
            "clipboard-write" => b(self.clipboard_write),
            "command" => self.command.clone(),
            "window-width" => self.window_cols.to_string(),
            "window-height" => self.window_rows.to_string(),
            "window-decorations" => b(self.decorations),
            "mouse-hide-while-typing" => b(self.mouse_hide_while_typing),
            "mouse-scroll-multiplier" => crate::ui::fmt_num(self.scroll_multiplier as f64),
            "foreground" => color_name(self.foreground),
            "background" => color_name(self.background),
            "cursor-color" => color_name(self.cursor_color),
            "selection-foreground" => color_name(self.selection_fg),
            "selection-background" => color_name(self.selection_bg),
            "dropdown-width" => crate::ui::fmt_num(self.dropdown_width as f64),
            "dropdown-height" => crate::ui::fmt_num(self.dropdown_height as f64),
            "dropdown-position" => edge_name(self.dropdown_position).into(),
            "dropdown-hide-on-focus-loss" => b(self.dropdown_hide_on_focus_loss),
            "dropdown-always-on-top" => b(self.dropdown_always_on_top),
            "dropdown-hotkey" => self.dropdown_hotkey.clone(),
            "dropdown-autostart" => b(self.dropdown_autostart),
            "dropdown-animation-duration" => crate::ui::fmt_num(self.dropdown_animation as f64),
            "desktop-notifications" => b(self.desktop_notifications),
            "shell-integration" => (if self.shell_integration { "detect" } else { "none" }).into(),
            _ => String::new(),
        }
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = config_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut text = String::from(
            "# rsterm configuration. Written by the settings screen (Ctrl+,);\n\
             # hand edits are fine and are picked up automatically.\n",
        );
        for k in KEYS {
            text.push_str(&format!("{k} = {}\n", self.get(k)));
        }
        // Write atomically so the watcher never sees a half-written file.
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(tmp, path)
    }
}

fn env_override(var: &str, out: &mut String) -> bool {
    match std::env::var(var) {
        Ok(v) if !v.is_empty() => {
            *out = v;
            true
        }
        _ => false,
    }
}

/// Poll the config file's mtime and call `on_change` when it changes.
pub fn watch(on_change: impl Fn() + Send + 'static) {
    std::thread::Builder::new()
        .name("config-watch".into())
        .spawn(move || {
            let path = config_path();
            let mtime = |p: &PathBuf| std::fs::metadata(p).and_then(|m| m.modified()).ok();
            let mut last: Option<SystemTime> = mtime(&path);
            loop {
                std::thread::sleep(Duration::from_millis(500));
                let now = mtime(&path);
                if now != last {
                    last = now;
                    on_change();
                }
            }
        })
        .expect("spawn config watcher");
}

/// Monospace font families known to fontconfig (for the settings screen).
pub fn monospace_families() -> Vec<String> {
    let out = std::process::Command::new("fc-list").args([":spacing=mono", "family"]).output();
    let mut v: Vec<String> = out
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split(',').next())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    v.sort_unstable_by_key(|s| s.to_lowercase());
    v.dedup();
    v.insert(0, "monospace".into());
    v
}

// ---------------------------------------------------------------------------
// Themes
// ---------------------------------------------------------------------------

pub struct Theme {
    pub name: &'static str,
    pub fg: u32,
    pub bg: u32,
    pub cursor: u32,
    pub selection_bg: u32,
    pub selection_fg: u32,
    pub ansi: [u32; 16],
}

pub const THEMES: &[Theme] = &[
    Theme {
        name: "Tomorrow Night",
        fg: 0xd8d8d8,
        bg: 0x181818,
        cursor: 0xe0e0e0,
        selection_bg: 0x3a5a8a,
        selection_fg: 0xffffff,
        ansi: [
            0x1d1f21, 0xcc6666, 0xb5bd68, 0xf0c674, 0x81a2be, 0xb294bb, 0x8abeb7, 0xc5c8c6, //
            0x666666, 0xd54e53, 0xb9ca4a, 0xe7c547, 0x7aa6da, 0xc397d8, 0x70c0b1, 0xeaeaea,
        ],
    },
    Theme {
        name: "High Contrast Dark",
        fg: 0xffffff,
        bg: 0x000000,
        cursor: 0xffff00,
        selection_bg: 0x0000c0,
        selection_fg: 0xffffff,
        ansi: [
            0x000000, 0xff5555, 0x55ff55, 0xffff55, 0x6c9cff, 0xff55ff, 0x55ffff, 0xe0e0e0, //
            0x909090, 0xff7777, 0x77ff77, 0xffff77, 0x8cb4ff, 0xff77ff, 0x77ffff, 0xffffff,
        ],
    },
    Theme {
        name: "High Contrast Light",
        fg: 0x000000,
        bg: 0xffffff,
        cursor: 0x0000ff,
        selection_bg: 0xffff00,
        selection_fg: 0x000000,
        ansi: [
            0x000000, 0xa00000, 0x005f00, 0x5f4b00, 0x0000b0, 0x8f008f, 0x005f5f, 0x404040, //
            0x4a4a4a, 0xc00000, 0x007000, 0x6f5700, 0x0000ff, 0xa800a8, 0x007070, 0x000000,
        ],
    },
    Theme {
        name: "Solarized Dark",
        fg: 0x839496,
        bg: 0x002b36,
        cursor: 0x93a1a1,
        selection_bg: 0x274642,
        selection_fg: 0xeee8d5,
        ansi: [
            0x073642, 0xdc322f, 0x859900, 0xb58900, 0x268bd2, 0xd33682, 0x2aa198, 0xeee8d5, //
            0x002b36, 0xcb4b16, 0x586e75, 0x657b83, 0x839496, 0x6c71c4, 0x93a1a1, 0xfdf6e3,
        ],
    },
    Theme {
        name: "Solarized Light",
        fg: 0x657b83,
        bg: 0xfdf6e3,
        cursor: 0x586e75,
        selection_bg: 0xeee8d5,
        selection_fg: 0x073642,
        ansi: [
            0x073642, 0xdc322f, 0x859900, 0xb58900, 0x268bd2, 0xd33682, 0x2aa198, 0xeee8d5, //
            0x002b36, 0xcb4b16, 0x586e75, 0x657b83, 0x839496, 0x6c71c4, 0x93a1a1, 0xfdf6e3,
        ],
    },
    Theme {
        name: "Gruvbox Dark",
        fg: 0xebdbb2,
        bg: 0x282828,
        cursor: 0xebdbb2,
        selection_bg: 0x504945,
        selection_fg: 0xfbf1c7,
        ansi: [
            0x282828, 0xcc241d, 0x98971a, 0xd79921, 0x458588, 0xb16286, 0x689d6a, 0xa89984, //
            0x928374, 0xfb4934, 0xb8bb26, 0xfabd2f, 0x83a598, 0xd3869b, 0x8ec07c, 0xebdbb2,
        ],
    },
    Theme {
        name: "Dracula",
        fg: 0xf8f8f2,
        bg: 0x282a36,
        cursor: 0xf8f8f2,
        selection_bg: 0x44475a,
        selection_fg: 0xffffff,
        ansi: [
            0x21222c, 0xff5555, 0x50fa7b, 0xf1fa8c, 0xbd93f9, 0xff79c6, 0x8be9fd, 0xf8f8f2, //
            0x6272a4, 0xff6e6e, 0x69ff94, 0xffffa5, 0xd6acff, 0xff92df, 0xa4ffff, 0xffffff,
        ],
    },
    Theme {
        name: "Nord",
        fg: 0xd8dee9,
        bg: 0x2e3440,
        cursor: 0xd8dee9,
        selection_bg: 0x434c5e,
        selection_fg: 0xeceff4,
        ansi: [
            0x3b4252, 0xbf616a, 0xa3be8c, 0xebcb8b, 0x81a1c1, 0xb48ead, 0x88c0d0, 0xe5e9f0, //
            0x4c566a, 0xbf616a, 0xa3be8c, 0xebcb8b, 0x81a1c1, 0xb48ead, 0x8fbcbb, 0xeceff4,
        ],
    },
];

pub fn theme(name: &str) -> Option<&'static Theme> {
    THEMES.iter().find(|t| t.name.eq_ignore_ascii_case(name))
}

/// Full 256-color palette for a theme.
pub fn palette(t: &Theme) -> [u32; 256] {
    let mut p = [0u32; 256];
    p[..16].copy_from_slice(&t.ansi);
    let steps = [0, 95, 135, 175, 215, 255];
    for i in 0..216 {
        let (r, g, b) = (steps[i / 36], steps[(i / 6) % 6], steps[i % 6]);
        p[16 + i] = r << 16 | g << 8 | b;
    }
    for i in 0..24 {
        let v = 8 + i as u32 * 10;
        p[232 + i] = v << 16 | v << 8 | v;
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_roundtrip_keys() {
        let mut c = Config::default();
        c.set("theme", "nord").unwrap();
        assert_eq!(c.theme, "Nord");
        c.set("cursor-style", "bar").unwrap();
        c.set("tab-bar", "always").unwrap();
        c.set("copy-on-select", "off").unwrap();
        assert!(c.set("bogus", "1").is_err());
        assert!(c.set("font-size", "huge").is_err());
        assert_eq!(c.cursor_style, CursorShape::Bar);
        assert_eq!(c.tab_bar, TabBar::Always);
        assert!(!c.copy_on_select);
        c.set("background", "#102030").unwrap();
        assert_eq!(c.background, Some(0x102030));
        c.set("background", "").unwrap();
        assert_eq!(c.background, None);
        assert!(c.set("background", "#12").is_err());
        c.set("dropdown-position", "bottom").unwrap();
        assert_eq!(c.dropdown_position, Edge::Bottom);
    }

    /// `save` writes `get` for every key in KEYS; each must parse back.
    #[test]
    fn every_key_roundtrips() {
        let mut c = Config::default();
        c.foreground = Some(0xabcdef);
        c.dropdown_position = Edge::Bottom;
        c.command = "fish".into();
        c.dropdown_hotkey = "ctrl+grave".into();
        c.dropdown_autostart = true;
        c.dropdown_animation = 0.35;
        c.desktop_notifications = false;
        c.shell_integration = false;
        let mut d = Config::default();
        for k in KEYS {
            let v = c.get(k);
            d.set(k, &v).unwrap_or_else(|e| panic!("{k}: {e}"));
        }
        assert_eq!(c, d);
    }
}
