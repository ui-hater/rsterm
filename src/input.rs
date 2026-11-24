//! Keyboard: xterm-style and kitty-protocol key encoding. (Shortcuts are
//! in `keybind.rs`.)

use crate::keybind::{SHIFT, mod_bits};
use winit::event::{ElementState, KeyEvent};
use winit::keyboard::{Key, KeyCode, KeyLocation, ModifiersState, NamedKey, PhysicalKey};
use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;

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

// ---------------------------------------------------------------------------
// Kitty keyboard protocol
// https://sw.kovidgoyal.net/kitty/keyboard-protocol/
// ---------------------------------------------------------------------------

/// Protocol flags. Flag 1 (disambiguate escape codes) is implied by any
/// non-zero flags, so the encoder never tests it.
pub mod kitty {
    pub const EVENT_TYPES: u8 = 2;
    pub const ALTERNATE_KEYS: u8 = 4;
    pub const ALL_AS_ESCAPES: u8 = 8;
    pub const ASSOCIATED_TEXT: u8 = 16;
}

/// How the kitty protocol identifies a key.
#[derive(Clone, Copy, Debug, PartialEq)]
enum KittyKey {
    /// A key that produces text: its unshifted codepoint.
    Text(u32),
    /// `CSI code u` keys: Escape, Enter, Tab, Backspace, keypad, F13+,
    /// lock and modifier keys.
    Code(u32),
    /// Keys with legacy CSI forms: `CSI n ~` or `CSI 1 X`.
    Func(u32, u8),
}

const ENTER: u32 = 13;
const TAB: u32 = 9;
const BACKSPACE: u32 = 127;
/// Codes from CAPS_LOCK (57358) up: lock, print, pause, menu, media and
/// modifier keys. Legacy encoding sends nothing for them, so they are only
/// reported with ALL_AS_ESCAPES.
const FIRST_SILENT: u32 = 57358;
const KP_FIRST: u32 = 57399;
const KP_LAST: u32 = 57427;

fn kitty_key(ev: &KeyEvent) -> Option<KittyKey> {
    use KittyKey::*;
    let right = ev.location == KeyLocation::Right;
    if ev.location == KeyLocation::Numpad
        && let PhysicalKey::Code(code) = ev.physical_key
    {
        {
            let kp = match code {
                KeyCode::Numpad0 => Some(57399),
                KeyCode::Numpad1 => Some(57400),
                KeyCode::Numpad2 => Some(57401),
                KeyCode::Numpad3 => Some(57402),
                KeyCode::Numpad4 => Some(57403),
                KeyCode::Numpad5 => Some(57404),
                KeyCode::Numpad6 => Some(57405),
                KeyCode::Numpad7 => Some(57406),
                KeyCode::Numpad8 => Some(57407),
                KeyCode::Numpad9 => Some(57408),
                KeyCode::NumpadDecimal => Some(57409),
                KeyCode::NumpadDivide => Some(57410),
                KeyCode::NumpadMultiply => Some(57411),
                KeyCode::NumpadSubtract => Some(57412),
                KeyCode::NumpadAdd => Some(57413),
                KeyCode::NumpadEnter => Some(57414),
                KeyCode::NumpadEqual => Some(57415),
                _ => None,
            };
            // With Num Lock off the digit keys act as arrows etc.; those
            // have their own keypad codes.
            let kp = match (&ev.logical_key, kp) {
                (Key::Named(n), Some(k)) if k <= 57409 => match n {
                    NamedKey::ArrowLeft => Some(57417),
                    NamedKey::ArrowRight => Some(57418),
                    NamedKey::ArrowUp => Some(57419),
                    NamedKey::ArrowDown => Some(57420),
                    NamedKey::PageUp => Some(57421),
                    NamedKey::PageDown => Some(57422),
                    NamedKey::Home => Some(57423),
                    NamedKey::End => Some(57424),
                    NamedKey::Insert => Some(57425),
                    NamedKey::Delete => Some(57426),
                    NamedKey::Clear => Some(57427),
                    _ => kp,
                },
                _ => kp,
            };
            if let Some(k) = kp {
                return Some(Code(k));
            }
        }
    }
    let named = match &ev.logical_key {
        Key::Named(n) => *n,
        Key::Character(_) => {
            // Unshifted key, so Shift+a and Shift+1 report 'a' and '1'.
            let base = match ev.key_without_modifiers() {
                Key::Character(s) => s.chars().next(),
                _ => None,
            }
            .or_else(|| match &ev.logical_key {
                Key::Character(s) => s.chars().next(),
                _ => None,
            })?;
            let base = base.to_lowercase().next().unwrap_or(base);
            return Some(Text(base as u32));
        }
        _ => return None,
    };
    Some(match named {
        NamedKey::Space => Text(32),
        NamedKey::Escape => Code(27),
        NamedKey::Enter => Code(ENTER),
        NamedKey::Tab => Code(TAB),
        NamedKey::Backspace => Code(BACKSPACE),
        NamedKey::Insert => Func(2, b'~'),
        NamedKey::Delete => Func(3, b'~'),
        NamedKey::PageUp => Func(5, b'~'),
        NamedKey::PageDown => Func(6, b'~'),
        NamedKey::ArrowUp => Func(1, b'A'),
        NamedKey::ArrowDown => Func(1, b'B'),
        NamedKey::ArrowRight => Func(1, b'C'),
        NamedKey::ArrowLeft => Func(1, b'D'),
        NamedKey::Home => Func(1, b'H'),
        NamedKey::End => Func(1, b'F'),
        NamedKey::F1 => Func(1, b'P'),
        NamedKey::F2 => Func(1, b'Q'),
        NamedKey::F3 => Func(13, b'~'),
        NamedKey::F4 => Func(1, b'S'),
        NamedKey::F5 => Func(15, b'~'),
        NamedKey::F6 => Func(17, b'~'),
        NamedKey::F7 => Func(18, b'~'),
        NamedKey::F8 => Func(19, b'~'),
        NamedKey::F9 => Func(20, b'~'),
        NamedKey::F10 => Func(21, b'~'),
        NamedKey::F11 => Func(23, b'~'),
        NamedKey::F12 => Func(24, b'~'),
        NamedKey::F13 => Code(57376),
        NamedKey::F14 => Code(57377),
        NamedKey::F15 => Code(57378),
        NamedKey::F16 => Code(57379),
        NamedKey::F17 => Code(57380),
        NamedKey::F18 => Code(57381),
        NamedKey::F19 => Code(57382),
        NamedKey::F20 => Code(57383),
        NamedKey::F21 => Code(57384),
        NamedKey::F22 => Code(57385),
        NamedKey::F23 => Code(57386),
        NamedKey::F24 => Code(57387),
        NamedKey::CapsLock => Code(57358),
        NamedKey::ScrollLock => Code(57359),
        NamedKey::NumLock => Code(57360),
        NamedKey::PrintScreen => Code(57361),
        NamedKey::Pause => Code(57362),
        NamedKey::ContextMenu => Code(57363),
        NamedKey::MediaPlay => Code(57428),
        NamedKey::MediaPause => Code(57429),
        NamedKey::MediaPlayPause => Code(57430),
        NamedKey::MediaStop => Code(57432),
        NamedKey::MediaFastForward => Code(57433),
        NamedKey::MediaRewind => Code(57434),
        NamedKey::MediaTrackNext => Code(57435),
        NamedKey::MediaTrackPrevious => Code(57436),
        NamedKey::MediaRecord => Code(57437),
        NamedKey::AudioVolumeDown => Code(57438),
        NamedKey::AudioVolumeUp => Code(57439),
        NamedKey::AudioVolumeMute => Code(57440),
        NamedKey::Shift => Code(if right { 57447 } else { 57441 }),
        NamedKey::Control => Code(if right { 57448 } else { 57442 }),
        NamedKey::Alt => Code(if right { 57449 } else { 57443 }),
        NamedKey::Super => Code(if right { 57450 } else { 57444 }),
        NamedKey::Hyper => Code(if right { 57451 } else { 57445 }),
        NamedKey::Meta => Code(if right { 57452 } else { 57446 }),
        NamedKey::AltGraph => Code(57453),
        _ => return None,
    })
}

/// Encode a key event for a program that enabled the kitty keyboard
/// protocol with `flags` (non-zero). Handles presses, repeats and releases;
/// returns nothing for events the flags don't ask for.
pub fn encode_kitty(ev: &KeyEvent, m: ModifiersState, flags: u8, app_cursor: bool) -> Vec<u8> {
    use KittyKey::*;
    let all = flags & kitty::ALL_AS_ESCAPES != 0;
    let released = ev.state == ElementState::Released;
    let event = if released {
        3
    } else if ev.repeat && flags & kitty::EVENT_TYPES != 0 {
        2
    } else {
        1
    };
    if released && flags & kitty::EVENT_TYPES == 0 {
        return Vec::new();
    }
    let mods = mod_bits(m);
    let legacy = || if released { Vec::new() } else { encode(ev, m, app_cursor) };
    let Some(key) = kitty_key(ev) else { return legacy() };
    // Text for ASSOCIATED_TEXT: what the key types, minus control characters.
    let text = || -> Option<String> {
        if flags & kitty::ASSOCIATED_TEXT == 0 || !all || released {
            return None;
        }
        ev.text.as_deref().map(|t| t.chars().filter(|c| !c.is_control()).collect::<String>()).filter(|t| !t.is_empty())
    };
    match key {
        Text(cp) => {
            // Plain (or shifted) typing stays plain text unless every key is
            // to be reported; its release is then not reported either.
            if !all && mods & !SHIFT == 0 {
                return legacy();
            }
            let shifted = (flags & kitty::ALTERNATE_KEYS != 0 && mods & SHIFT != 0)
                .then(|| match &ev.logical_key {
                    Key::Character(s) => s.chars().next().map(|c| c as u32),
                    _ => None,
                })
                .flatten()
                .filter(|&c| c != cp);
            csi_u(cp, shifted, mods, event, text())
        }
        Code(code) => {
            if !all {
                // Enter, Tab and Backspace keep their legacy bytes when
                // unmodified (so typing `reset` after a crash still works),
                // and have no release events.
                if matches!(code, ENTER | TAB | BACKSPACE) && (mods == 0 || released) {
                    return legacy();
                }
                if code >= FIRST_SILENT && !(KP_FIRST..=KP_LAST).contains(&code) {
                    return Vec::new();
                }
            }
            csi_u(code, None, mods, event, None)
        }
        Func(n, fin) => {
            if !all && mods == 0 && event == 1 {
                return legacy();
            }
            let out = if mods != 0 || event != 1 {
                let ev = if event != 1 { format!(":{event}") } else { String::new() };
                format!("\x1b[{n};{}{ev}{}", mods + 1, fin as char)
            } else if fin == b'~' {
                format!("\x1b[{n}~")
            } else {
                format!("\x1b[{}", fin as char)
            };
            out.into_bytes()
        }
    }
}

fn csi_u(code: u32, shifted: Option<u32>, mods: u8, event: u8, text: Option<String>) -> Vec<u8> {
    let mut out = format!("\x1b[{code}");
    if let Some(s) = shifted {
        out.push_str(&format!(":{s}"));
    }
    if mods != 0 || event != 1 || text.is_some() {
        out.push_str(&format!(";{}", mods + 1));
        if event != 1 {
            out.push_str(&format!(":{event}"));
        }
    }
    if let Some(t) = text {
        let cps: Vec<String> = t.chars().map(|c| (c as u32).to_string()).collect();
        out.push_str(&format!(";{}", cps.join(":")));
    }
    out.push('u');
    out.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keybind::{ALT, CTRL, SUPER};

    #[test]
    fn csi_u_forms() {
        assert_eq!(csi_u(27, None, 0, 1, None), b"\x1b[27u");
        assert_eq!(csi_u(97, None, CTRL, 1, None), b"\x1b[97;5u");
        assert_eq!(csi_u(97, Some(65), SHIFT, 3, None), b"\x1b[97:65;2:3u");
        assert_eq!(csi_u(97, None, 0, 1, Some("a".into())), b"\x1b[97;1;97u");
        assert_eq!(csi_u(13, None, ALT | SUPER, 2, None), b"\x1b[13;11:2u");
    }
}
