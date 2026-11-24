//! Global hotkey for the dropdown, grabbed on the X11 root window.
//!
//! Runs on its own thread with its own X connection, so it works no matter
//! which window has focus. Only X11 (or apps under XWayland) can grab keys
//! this way; on a Wayland session the key only fires while an XWayland
//! window is focused, so binding `rsterm --dropdown` in the desktop remains
//! the fallback there.

use std::os::unix::io::AsRawFd;
use std::sync::mpsc::{Receiver, TryRecvError};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{ConnectionExt, GrabMode, ModMask, Window};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hotkey {
    mods: u16,
    keysym: u32,
}

/// Parse e.g. `F12`, `ctrl+grave`, `super+Return`. Case-insensitive.
pub fn parse(spec: &str) -> Option<Hotkey> {
    let mut parts: Vec<&str> = spec.split('+').map(str::trim).collect();
    let key = parts.pop()?.to_ascii_lowercase();
    let mut mods = 0u16;
    for m in parts {
        mods |= match m.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => u16::from(ModMask::CONTROL),
            "shift" => u16::from(ModMask::SHIFT),
            "alt" | "mod1" => u16::from(ModMask::M1),
            "super" | "win" | "logo" | "mod4" => u16::from(ModMask::M4),
            _ => return None,
        };
    }
    let keysym = match key.as_str() {
        k if k.len() == 1 && k.as_bytes()[0].is_ascii_alphanumeric() => k.as_bytes()[0] as u32,
        k if k.len() >= 2 && k.starts_with('f') && k[1..].bytes().all(|b| b.is_ascii_digit()) => {
            let n: u32 = k[1..].parse().ok()?;
            if !(1..=35).contains(&n) {
                return None;
            }
            0xffbe + n - 1
        }
        "grave" | "`" => 0x60,
        "escape" | "esc" => 0xff1b,
        "tab" => 0xff09,
        "return" | "enter" => 0xff0d,
        "space" => 0x20,
        "pause" => 0xff13,
        "scroll_lock" | "scrolllock" => 0xff14,
        "print" => 0xff61,
        "insert" => 0xff63,
        "home" => 0xff50,
        "end" => 0xff57,
        "page_up" | "pageup" | "prior" => 0xff55,
        "page_down" | "pagedown" | "next" => 0xff56,
        "menu" => 0xff67,
        "minus" | "-" => 0x2d,
        "equal" | "=" => 0x3d,
        "comma" | "," => 0x2c,
        "period" | "." => 0x2e,
        "slash" | "/" => 0x2f,
        "backslash" | "\\" => 0x5c,
        "semicolon" | ";" => 0x3b,
        "apostrophe" | "'" => 0x27,
        "bracketleft" | "[" => 0x5b,
        "bracketright" | "]" => 0x5d,
        _ => return None,
    };
    Some(Hotkey { mods, keysym })
}

pub enum Msg {
    Pressed,
    Error(String),
}

/// Grab `spec` (empty = none) and call `on` for presses and problems. Send a
/// new spec over `updates` to re-grab after a config change.
pub fn spawn(spec: String, updates: Receiver<String>, on: impl Fn(Msg) + Send + 'static) {
    std::thread::Builder::new()
        .name("hotkey".into())
        .spawn(move || {
            if let Err(e) = run(spec, updates, &on) {
                on(Msg::Error(format!("global hotkey unavailable: {e}")));
            }
        })
        .expect("spawn hotkey thread");
}

/// Keycodes + modifier masks currently grabbed.
type Grabbed = Vec<(u8, u16)>;

fn run(mut spec: String, updates: Receiver<String>, on: &impl Fn(Msg)) -> Result<(), Box<dyn std::error::Error>> {
    let (conn, screen) = x11rb::connect(None)?;
    let root = conn.setup().roots[screen].root;
    let mut grabbed = grab(&conn, root, &spec, on)?;
    let fd = conn.stream().as_raw_fd();
    let mut last_press: Option<u32> = None;
    loop {
        while let Some(ev) = conn.poll_for_event()? {
            if let Event::KeyPress(k) = ev {
                // Holding the key auto-repeats presses ~30ms apart: only the
                // first press of a hold toggles.
                let repeat = last_press.is_some_and(|t| k.time.wrapping_sub(t) < 400);
                last_press = Some(k.time);
                if !repeat && grabbed.iter().any(|&(code, _)| code == k.detail) {
                    on(Msg::Pressed);
                }
            }
        }
        let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        unsafe { libc::poll(&mut pfd, 1, 250) };
        match updates.try_recv() {
            Ok(new) if new != spec => {
                for &(code, mods) in &grabbed {
                    conn.ungrab_key(code, root, ModMask::from(mods))?;
                }
                spec = new;
                grabbed = grab(&conn, root, &spec, on)?;
            }
            Ok(_) | Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => return Ok(()),
        }
    }
}

fn grab(conn: &impl Connection, root: Window, spec: &str, on: &impl Fn(Msg)) -> Result<Grabbed, Box<dyn std::error::Error>> {
    let mut out = Vec::new();
    if spec.trim().is_empty() {
        return Ok(out);
    }
    let Some(hk) = parse(spec) else {
        on(Msg::Error(format!("invalid dropdown hotkey {spec:?}")));
        return Ok(out);
    };
    let setup = conn.setup();
    let (min, max) = (setup.min_keycode, setup.max_keycode);
    let map = conn.get_keyboard_mapping(min, max - min + 1)?.reply()?;
    let per = map.keysyms_per_keycode as usize;
    let codes: Vec<u8> = map
        .keysyms
        .chunks(per.max(1))
        .enumerate()
        .filter(|(_, syms)| syms.contains(&hk.keysym))
        .map(|(i, _)| min + i as u8)
        .collect();
    if codes.is_empty() {
        on(Msg::Error(format!("dropdown hotkey {spec} is not on this keyboard")));
        return Ok(out);
    }
    // Grab with every Caps Lock / Num Lock combination so the key works
    // regardless of lock state.
    let (lock, num) = (u16::from(ModMask::LOCK), u16::from(ModMask::M2));
    let mut failed = false;
    for &code in &codes {
        for extra in [0, lock, num, lock | num] {
            let mods = hk.mods | extra;
            let r = conn.grab_key(false, root, ModMask::from(mods), code, GrabMode::ASYNC, GrabMode::ASYNC)?.check();
            if r.is_ok() {
                out.push((code, mods));
            } else {
                failed = true;
            }
        }
    }
    conn.flush()?;
    if failed && out.is_empty() {
        on(Msg::Error(format!("dropdown hotkey {spec} is already used by another program")));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hotkeys() {
        assert_eq!(parse("F12"), Some(Hotkey { mods: 0, keysym: 0xffc9 }));
        assert_eq!(parse("ctrl+grave").map(|h| h.keysym), Some(0x60));
        assert!(parse("Super+Return").is_some_and(|h| h.mods == u16::from(ModMask::M4)));
        assert_eq!(parse("ctrl+a").map(|h| h.keysym), Some(0x61));
        assert!(parse("hyper+x").is_none());
        assert!(parse("F99").is_none());
        assert!(parse("").is_none());
    }
}
