//! Keyboard: terminal-level shortcuts and xterm-style key encoding.

use crate::ui::Action;
use winit::event::KeyEvent;
use winit::keyboard::{Key, KeyCode, ModifiersState, NamedKey, PhysicalKey};

/// Map a key press to a terminal action (tabs, clipboard, menus...).
/// Uses physical keys for Ctrl+Shift chords so they work on any layout.
pub fn shortcut(ev: &KeyEvent, m: ModifiersState, tabs: usize) -> Option<Action> {
    let (ctrl, shift, alt) = (m.control_key(), m.shift_key(), m.alt_key());
    let PhysicalKey::Code(code) = ev.physical_key else { return None };
    if ctrl && shift && !alt {
        let a = match code {
            KeyCode::KeyT => Action::NewTab,
            KeyCode::KeyW => Action::CloseTab,
            KeyCode::KeyC => Action::Copy,
            KeyCode::KeyV => Action::Paste,
            KeyCode::KeyA => Action::SelectAll,
            KeyCode::KeyF => Action::Find,
            KeyCode::KeyK => Action::ClearScrollback,
            KeyCode::KeyM => Action::OpenMenu,
            KeyCode::KeyQ => Action::Quit,
            KeyCode::Equal => Action::ZoomIn,
            KeyCode::Minus => Action::ZoomOut,
            KeyCode::Digit0 => Action::ZoomReset,
            KeyCode::Comma => Action::ReloadConfig,
            KeyCode::Tab => Action::PrevTab,
            KeyCode::PageUp => Action::PrevPrompt,
            KeyCode::PageDown => Action::NextPrompt,
            _ => return None,
        };
        return Some(a);
    }
    if ctrl && !shift && !alt {
        return match code {
            KeyCode::Comma => Some(Action::Settings),
            KeyCode::Tab | KeyCode::PageDown => Some(Action::NextTab),
            KeyCode::PageUp => Some(Action::PrevTab),
            _ => None,
        };
    }
    if alt && !ctrl && !shift && tabs > 1 {
        let n = match code {
            KeyCode::Digit1 => 1,
            KeyCode::Digit2 => 2,
            KeyCode::Digit3 => 3,
            KeyCode::Digit4 => 4,
            KeyCode::Digit5 => 5,
            KeyCode::Digit6 => 6,
            KeyCode::Digit7 => 7,
            KeyCode::Digit8 => 8,
            KeyCode::Digit9 => return Some(Action::GotoTab(usize::MAX)), // last tab
            _ => return None,
        };
        return (n <= tabs).then_some(Action::GotoTab(n - 1));
    }
    match (&ev.logical_key, shift, ctrl, alt) {
        (Key::Named(NamedKey::ContextMenu), _, false, false) => Some(Action::OpenMenu),
        (Key::Named(NamedKey::F10), true, false, false) => Some(Action::OpenMenu),
        _ => None,
    }
}

/// Encode a key press as the bytes an xterm would send.
pub fn encode(ev: &KeyEvent, m: ModifiersState, app_cursor: bool) -> Vec<u8> {
    let (ctrl, shift, alt) = (m.control_key(), m.shift_key(), m.alt_key());
    let modnum = 1 + shift as u8 + 2 * alt as u8 + 4 * ctrl as u8;
    let mut buf: Vec<u8> = Vec::with_capacity(16);

    let csi_letter = |buf: &mut Vec<u8>, ch: u8, ss3: bool| {
        if modnum > 1 {
            buf.extend_from_slice(format!("\x1b[1;{}{}", modnum, ch as char).as_bytes());
        } else if ss3 {
            buf.extend_from_slice(&[0x1b, b'O', ch]);
        } else {
            buf.extend_from_slice(&[0x1b, b'[', ch]);
        }
    };
    let csi_tilde = |buf: &mut Vec<u8>, n: u8| {
        if modnum > 1 {
            buf.extend_from_slice(format!("\x1b[{};{}~", n, modnum).as_bytes());
        } else {
            buf.extend_from_slice(format!("\x1b[{}~", n).as_bytes());
        }
    };
    let esc_if_alt = |buf: &mut Vec<u8>| {
        if alt {
            buf.push(0x1b)
        }
    };

    match &ev.logical_key {
        Key::Named(named) => match named {
            NamedKey::Enter => {
                esc_if_alt(&mut buf);
                buf.push(b'\r')
            }
            NamedKey::Backspace => {
                esc_if_alt(&mut buf);
                buf.push(if ctrl { 0x08 } else { 0x7f })
            }
            NamedKey::Tab => {
                if shift {
                    buf.extend_from_slice(b"\x1b[Z")
                } else {
                    esc_if_alt(&mut buf);
                    buf.push(b'\t')
                }
            }
            NamedKey::Escape => {
                esc_if_alt(&mut buf);
                buf.push(0x1b)
            }
            NamedKey::Space => {
                esc_if_alt(&mut buf);
                buf.push(if ctrl { 0 } else { b' ' })
            }
            NamedKey::ArrowUp => csi_letter(&mut buf, b'A', app_cursor),
            NamedKey::ArrowDown => csi_letter(&mut buf, b'B', app_cursor),
            NamedKey::ArrowRight => csi_letter(&mut buf, b'C', app_cursor),
            NamedKey::ArrowLeft => csi_letter(&mut buf, b'D', app_cursor),
            NamedKey::Home => csi_letter(&mut buf, b'H', app_cursor),
            NamedKey::End => csi_letter(&mut buf, b'F', app_cursor),
            NamedKey::Insert => csi_tilde(&mut buf, 2),
            NamedKey::Delete => csi_tilde(&mut buf, 3),
            NamedKey::PageUp => csi_tilde(&mut buf, 5),
            NamedKey::PageDown => csi_tilde(&mut buf, 6),
            NamedKey::F1 => csi_letter(&mut buf, b'P', true),
            NamedKey::F2 => csi_letter(&mut buf, b'Q', true),
            NamedKey::F3 => csi_letter(&mut buf, b'R', true),
            NamedKey::F4 => csi_letter(&mut buf, b'S', true),
            NamedKey::F5 => csi_tilde(&mut buf, 15),
            NamedKey::F6 => csi_tilde(&mut buf, 17),
            NamedKey::F7 => csi_tilde(&mut buf, 18),
            NamedKey::F8 => csi_tilde(&mut buf, 19),
            NamedKey::F9 => csi_tilde(&mut buf, 20),
            NamedKey::F10 => csi_tilde(&mut buf, 21),
            NamedKey::F11 => csi_tilde(&mut buf, 23),
            NamedKey::F12 => csi_tilde(&mut buf, 24),
            _ => {
                if let Some(t) = &ev.text {
                    buf.extend_from_slice(t.as_bytes())
                }
            }
        },
        Key::Character(s) => {
            let mut chars = s.chars();
            let c = chars.next();
            if ctrl && chars.next().is_none() {
                let ctl = match c.map(|c| c.to_ascii_lowercase()) {
                    Some(c @ 'a'..='z') => Some(c as u8 - b'a' + 1),
                    Some('@') | Some('2') | Some(' ') => Some(0),
                    Some('[') | Some('3') => Some(0x1b),
                    Some('\\') | Some('4') => Some(0x1c),
                    Some(']') | Some('5') => Some(0x1d),
                    Some('^') | Some('6') => Some(0x1e),
                    Some('_') | Some('7') | Some('-') => Some(0x1f),
                    Some('?') | Some('8') => Some(0x7f),
                    _ => None,
                };
                if let Some(b) = ctl {
                    esc_if_alt(&mut buf);
                    buf.push(b);
                }
            }
            if buf.is_empty() {
                let text = ev.text.as_deref().unwrap_or(s.as_str());
                esc_if_alt(&mut buf);
                buf.extend_from_slice(text.as_bytes());
            }
        }
        _ => {
            if let Some(t) = &ev.text {
                buf.extend_from_slice(t.as_bytes())
            }
        }
    }
    buf
}
