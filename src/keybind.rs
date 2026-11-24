//! Key bindings, configured Ghostty-style:
//!
//! ```text
//! keybind = ctrl+shift+t=new_tab
//! keybind = ctrl+shift+t=unbind        # remove a binding (key goes to the program)
//! keybind = alt+left=text:\x1bb        # send bytes
//! keybind = clear                      # drop all bindings, including defaults
//! ```
//!
//! Entries apply in order on top of `DEFAULTS`. Triggers match physical keys
//! (US layout names), so they work the same on any keyboard layout.

use crate::ui::Action;
use winit::keyboard::{KeyCode, ModifiersState};

pub const SHIFT: u8 = 1;
pub const ALT: u8 = 2;
pub const CTRL: u8 = 4;
pub const SUPER: u8 = 8;

pub fn mod_bits(m: ModifiersState) -> u8 {
    [(m.shift_key(), SHIFT), (m.alt_key(), ALT), (m.control_key(), CTRL), (m.super_key(), SUPER)]
        .into_iter()
        .filter(|&(on, _)| on)
        .fold(0, |acc, (_, bit)| acc | bit)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Trigger {
    pub mods: u8,
    pub key: KeyCode,
}

#[derive(Clone, PartialEq, Debug)]
pub enum Bound {
    Action(Action),
    /// Bytes written to the program.
    Text(Vec<u8>),
    /// Swallow the key.
    Ignore,
}

pub enum Entry {
    Clear,
    Bind(Trigger, Option<Bound>),
}

pub const DEFAULTS: &[&str] = &[
    "ctrl+shift+t=new_tab",
    "ctrl+shift+w=close_tab",
    "ctrl+tab=next_tab",
    "ctrl+page_down=next_tab",
    "ctrl+shift+tab=previous_tab",
    "ctrl+page_up=previous_tab",
    "alt+1=goto_tab:1",
    "alt+2=goto_tab:2",
    "alt+3=goto_tab:3",
    "alt+4=goto_tab:4",
    "alt+5=goto_tab:5",
    "alt+6=goto_tab:6",
    "alt+7=goto_tab:7",
    "alt+8=goto_tab:8",
    "alt+9=last_tab",
    "ctrl+shift+c=copy_to_clipboard",
    "ctrl+shift+v=paste_from_clipboard",
    "ctrl+shift+a=select_all",
    "ctrl+shift+f=start_search",
    "ctrl+shift+k=clear_screen",
    "ctrl+shift+page_up=jump_to_prompt:-1",
    "ctrl+shift+page_down=jump_to_prompt:1",
    "shift+page_up=scroll_page_up",
    "shift+page_down=scroll_page_down",
    "shift+home=scroll_to_top",
    "shift+end=scroll_to_bottom",
    "ctrl+shift+equal=increase_font_size:1",
    "ctrl+shift+minus=decrease_font_size:1",
    "ctrl+shift+0=reset_font_size",
    "ctrl+comma=open_config",
    "ctrl+shift+comma=reload_config",
    "ctrl+shift+m=open_menu",
    "context_menu=open_menu",
    "shift+f10=open_menu",
    "ctrl+shift+q=quit",
];

/// (config name, key, label shown in menus).
const KEYS: &[(&str, KeyCode, &str)] = &[
    ("a", KeyCode::KeyA, "A"),
    ("b", KeyCode::KeyB, "B"),
    ("c", KeyCode::KeyC, "C"),
    ("d", KeyCode::KeyD, "D"),
    ("e", KeyCode::KeyE, "E"),
    ("f", KeyCode::KeyF, "F"),
    ("g", KeyCode::KeyG, "G"),
    ("h", KeyCode::KeyH, "H"),
    ("i", KeyCode::KeyI, "I"),
    ("j", KeyCode::KeyJ, "J"),
    ("k", KeyCode::KeyK, "K"),
    ("l", KeyCode::KeyL, "L"),
    ("m", KeyCode::KeyM, "M"),
    ("n", KeyCode::KeyN, "N"),
    ("o", KeyCode::KeyO, "O"),
    ("p", KeyCode::KeyP, "P"),
    ("q", KeyCode::KeyQ, "Q"),
    ("r", KeyCode::KeyR, "R"),
    ("s", KeyCode::KeyS, "S"),
    ("t", KeyCode::KeyT, "T"),
    ("u", KeyCode::KeyU, "U"),
    ("v", KeyCode::KeyV, "V"),
    ("w", KeyCode::KeyW, "W"),
    ("x", KeyCode::KeyX, "X"),
    ("y", KeyCode::KeyY, "Y"),
    ("z", KeyCode::KeyZ, "Z"),
    ("0", KeyCode::Digit0, "0"),
    ("1", KeyCode::Digit1, "1"),
    ("2", KeyCode::Digit2, "2"),
    ("3", KeyCode::Digit3, "3"),
    ("4", KeyCode::Digit4, "4"),
    ("5", KeyCode::Digit5, "5"),
    ("6", KeyCode::Digit6, "6"),
    ("7", KeyCode::Digit7, "7"),
    ("8", KeyCode::Digit8, "8"),
    ("9", KeyCode::Digit9, "9"),
    ("f1", KeyCode::F1, "F1"),
    ("f2", KeyCode::F2, "F2"),
    ("f3", KeyCode::F3, "F3"),
    ("f4", KeyCode::F4, "F4"),
    ("f5", KeyCode::F5, "F5"),
    ("f6", KeyCode::F6, "F6"),
    ("f7", KeyCode::F7, "F7"),
    ("f8", KeyCode::F8, "F8"),
    ("f9", KeyCode::F9, "F9"),
    ("f10", KeyCode::F10, "F10"),
    ("f11", KeyCode::F11, "F11"),
    ("f12", KeyCode::F12, "F12"),
    ("equal", KeyCode::Equal, "="),
    ("=", KeyCode::Equal, "="),
    ("minus", KeyCode::Minus, "-"),
    ("-", KeyCode::Minus, "-"),
    ("comma", KeyCode::Comma, ","),
    (",", KeyCode::Comma, ","),
    ("period", KeyCode::Period, "."),
    (".", KeyCode::Period, "."),
    ("slash", KeyCode::Slash, "/"),
    ("/", KeyCode::Slash, "/"),
    ("backslash", KeyCode::Backslash, "\\"),
    ("\\", KeyCode::Backslash, "\\"),
    ("semicolon", KeyCode::Semicolon, ";"),
    (";", KeyCode::Semicolon, ";"),
    ("apostrophe", KeyCode::Quote, "'"),
    ("'", KeyCode::Quote, "'"),
    ("grave", KeyCode::Backquote, "`"),
    ("grave_accent", KeyCode::Backquote, "`"),
    ("`", KeyCode::Backquote, "`"),
    ("bracket_left", KeyCode::BracketLeft, "["),
    ("left_bracket", KeyCode::BracketLeft, "["),
    ("[", KeyCode::BracketLeft, "["),
    ("bracket_right", KeyCode::BracketRight, "]"),
    ("right_bracket", KeyCode::BracketRight, "]"),
    ("]", KeyCode::BracketRight, "]"),
    ("space", KeyCode::Space, "Space"),
    ("enter", KeyCode::Enter, "Enter"),
    ("return", KeyCode::Enter, "Enter"),
    ("tab", KeyCode::Tab, "Tab"),
    ("escape", KeyCode::Escape, "Escape"),
    ("backspace", KeyCode::Backspace, "Backspace"),
    ("insert", KeyCode::Insert, "Insert"),
    ("delete", KeyCode::Delete, "Delete"),
    ("home", KeyCode::Home, "Home"),
    ("end", KeyCode::End, "End"),
    ("page_up", KeyCode::PageUp, "PageUp"),
    ("page_down", KeyCode::PageDown, "PageDown"),
    ("up", KeyCode::ArrowUp, "Up"),
    ("arrow_up", KeyCode::ArrowUp, "Up"),
    ("down", KeyCode::ArrowDown, "Down"),
    ("arrow_down", KeyCode::ArrowDown, "Down"),
    ("left", KeyCode::ArrowLeft, "Left"),
    ("arrow_left", KeyCode::ArrowLeft, "Left"),
    ("right", KeyCode::ArrowRight, "Right"),
    ("arrow_right", KeyCode::ArrowRight, "Right"),
    ("context_menu", KeyCode::ContextMenu, "Menu"),
    ("menu", KeyCode::ContextMenu, "Menu"),
];

fn parse_trigger(s: &str) -> Result<Trigger, String> {
    let s = s.trim().to_ascii_lowercase();
    let mut mods = 0;
    let parts: Vec<&str> = s.split('+').collect();
    let (key, mod_names) = parts.split_last().ok_or("empty trigger")?;
    for m in mod_names {
        mods |= match *m {
            "shift" => SHIFT,
            "alt" | "opt" | "option" => ALT,
            "ctrl" | "control" => CTRL,
            "super" | "cmd" | "command" => SUPER,
            _ => return Err(format!("unknown modifier {m:?}")),
        };
    }
    let key = KEYS
        .iter()
        .find(|(n, _, _)| n == key)
        .map(|&(_, k, _)| k)
        .ok_or_else(|| format!("unknown key {key:?}"))?;
    Ok(Trigger { mods, key })
}

/// Ghostty-style escapes in `text:` values: \n \r \t \e \\ \xHH \u{HEX}.
fn unescape(s: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c != '\\' {
            let mut b = [0; 4];
            out.extend_from_slice(c.encode_utf8(&mut b).as_bytes());
            continue;
        }
        match it.next() {
            Some('n') => out.push(b'\n'),
            Some('r') => out.push(b'\r'),
            Some('t') => out.push(b'\t'),
            Some('e') => out.push(0x1b),
            Some('\\') => out.push(b'\\'),
            Some('x') => {
                let h: String = it.by_ref().take(2).collect();
                out.push(u8::from_str_radix(&h, 16).map_err(|_| format!("bad escape \\x{h}"))?);
            }
            Some('u') => {
                if it.next() != Some('{') {
                    return Err("expected \\u{...}".into());
                }
                let h: String = it.by_ref().take_while(|&c| c != '}').collect();
                let c = u32::from_str_radix(&h, 16).ok().and_then(char::from_u32).ok_or("bad \\u escape")?;
                let mut b = [0; 4];
                out.extend_from_slice(c.encode_utf8(&mut b).as_bytes());
            }
            other => return Err(format!("unknown escape \\{}", other.map(String::from).unwrap_or_default())),
        }
    }
    Ok(out)
}

/// Parse an action name (with optional `:param`). Ok(None) = unbind.
fn parse_action(s: &str) -> Result<Option<Bound>, String> {
    let (name, param) = match s.split_once(':') {
        Some((n, p)) => (n.trim(), Some(p)),
        None => (s.trim(), None),
    };
    let num = || -> Result<i64, String> {
        param.ok_or(format!("{name} needs a value, e.g. {name}:1"))?.trim().parse().map_err(|_| format!("bad value for {name}"))
    };
    let a = match name {
        "unbind" => return Ok(None),
        "ignore" => return Ok(Some(Bound::Ignore)),
        "text" => return Ok(Some(Bound::Text(unescape(param.unwrap_or(""))?))),
        "csi" => return Ok(Some(Bound::Text([b"\x1b[".as_slice(), &unescape(param.unwrap_or(""))?].concat()))),
        "esc" => return Ok(Some(Bound::Text([b"\x1b".as_slice(), &unescape(param.unwrap_or(""))?].concat()))),
        "new_tab" => Action::NewTab,
        "close_tab" | "close_surface" => Action::CloseTab,
        "next_tab" => Action::NextTab,
        "previous_tab" => Action::PrevTab,
        "last_tab" => Action::GotoTab(usize::MAX),
        "goto_tab" => Action::GotoTab((num()?.max(1) - 1) as usize),
        "copy_to_clipboard" => Action::Copy,
        "paste_from_clipboard" => Action::Paste,
        "select_all" => Action::SelectAll,
        "start_search" | "search" => Action::Find,
        "clear_screen" => Action::ClearScrollback,
        "jump_to_prompt" => {
            if num()? < 0 {
                Action::PrevPrompt
            } else {
                Action::NextPrompt
            }
        }
        "scroll_page_up" => Action::ScrollPageUp,
        "scroll_page_down" => Action::ScrollPageDown,
        "scroll_to_top" => Action::ScrollTop,
        "scroll_to_bottom" => Action::ScrollBottom,
        "increase_font_size" => Action::ZoomIn,
        "decrease_font_size" => Action::ZoomOut,
        "reset_font_size" => Action::ZoomReset,
        "open_config" => Action::Settings,
        "reload_config" => Action::ReloadConfig,
        "open_menu" => Action::OpenMenu,
        "quit" => Action::Quit,
        _ => return Err(format!("unknown action {name:?}")),
    };
    Ok(Some(Bound::Action(a)))
}

/// Parse one `keybind` value.
pub fn parse_entry(s: &str) -> Result<Entry, String> {
    let s = s.trim();
    if s == "clear" {
        return Ok(Entry::Clear);
    }
    // trigger=action; the action (e.g. text:a=b) may itself contain '='.
    let (t, a) = s.split_once('=').ok_or("expected trigger=action")?;
    let (t, a) = if t.is_empty() || t.ends_with('+') {
        // The key is "=" itself, as in "ctrl+==...".
        let rest = a.strip_prefix('=').ok_or("expected trigger=action")?;
        (format!("{t}="), rest)
    } else {
        (t.to_string(), a)
    };
    Ok(Entry::Bind(parse_trigger(&t)?, parse_action(a)?))
}

pub struct Bindings {
    list: Vec<(Trigger, Bound)>,
}

impl Bindings {
    /// Defaults, then each user entry in order (invalid ones are skipped;
    /// `Config::set` already reported them).
    pub fn new(user: &[String]) -> Bindings {
        let mut b = Bindings { list: Vec::new() };
        for e in DEFAULTS.iter().copied().chain(user.iter().map(String::as_str)) {
            match parse_entry(e) {
                Ok(Entry::Clear) => b.list.clear(),
                Ok(Entry::Bind(t, bound)) => {
                    b.list.retain(|(x, _)| *x != t);
                    if let Some(bound) = bound {
                        b.list.push((t, bound));
                    }
                }
                Err(_) => {}
            }
        }
        b
    }

    pub fn lookup(&self, mods: u8, key: KeyCode) -> Option<&Bound> {
        self.list.iter().find(|(t, _)| t.mods == mods && t.key == key).map(|(_, b)| b)
    }

    /// Human-readable shortcut for `a` (first binding), e.g. "Ctrl+Shift+T".
    pub fn label(&self, a: Action) -> String {
        let Some((t, _)) = self.list.iter().find(|(_, b)| *b == Bound::Action(a)) else { return String::new() };
        let mut s = String::new();
        for (bit, name) in [(CTRL, "Ctrl+"), (ALT, "Alt+"), (SHIFT, "Shift+"), (SUPER, "Super+")] {
            if t.mods & bit != 0 {
                s.push_str(name);
            }
        }
        s.push_str(KEYS.iter().find(|(_, k, _)| *k == t.key).map_or("?", |(_, _, l)| l));
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_parse() {
        for d in DEFAULTS {
            assert!(parse_entry(d).is_ok(), "{d}");
        }
        let b = Bindings::new(&[]);
        assert_eq!(b.list.len(), DEFAULTS.len());
        assert_eq!(b.label(Action::NewTab), "Ctrl+Shift+T");
        assert_eq!(b.label(Action::ZoomIn), "Ctrl+Shift+=");
        assert_eq!(b.label(Action::NextTab), "Ctrl+Tab");
    }

    #[test]
    fn user_entries() {
        let b = Bindings::new(&[
            "ctrl+shift+t=unbind".into(),
            "ctrl+t=new_tab".into(),
            "alt+left=text:\\x1bb".into(),
            "ctrl+==increase_font_size:1".into(),
        ]);
        assert_eq!(b.lookup(CTRL | SHIFT, KeyCode::KeyT), None);
        assert_eq!(b.lookup(CTRL, KeyCode::KeyT), Some(&Bound::Action(Action::NewTab)));
        assert_eq!(b.lookup(ALT, KeyCode::ArrowLeft), Some(&Bound::Text(b"\x1bb".to_vec())));
        assert_eq!(b.lookup(CTRL, KeyCode::Equal), Some(&Bound::Action(Action::ZoomIn)));
        assert_eq!(b.label(Action::NewTab), "Ctrl+T");
        let b = Bindings::new(&["clear".into(), "f1=quit".into()]);
        assert_eq!(b.list.len(), 1);
        assert!(parse_entry("ctrl+nope=quit").is_err());
        assert!(parse_entry("ctrl+t=fly").is_err());
    }
}
