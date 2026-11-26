//! Dropdown hotkey on GNOME Wayland sessions.
//!
//! There the X11 root-window grab (`hotkey.rs`) only sees keys while an
//! XWayland window has focus, so instead the hotkey is registered as a GNOME
//! custom keybinding that runs `rsterm --dropdown`, which forwards a toggle
//! to the running instance (or starts one). The binding stays after rsterm
//! exits, so the key also starts the dropdown, as the first call does.

use std::process::Command;

const LIST_SCHEMA: &str = "org.gnome.settings-daemon.plugins.media-keys";
const LIST_KEY: &str = "custom-keybindings";
const PATH: &str = "/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/rsterm-dropdown/";

/// A GNOME session on Wayland, where the X11 grab doesn't work globally.
/// Must be checked from the environment we were started in.
pub fn wayland_session() -> bool {
    let gnome = std::env::var("XDG_CURRENT_DESKTOP")
        .is_ok_and(|d| d.split(':').any(|p| p.eq_ignore_ascii_case("gnome")));
    gnome && std::env::var_os("WAYLAND_DISPLAY").is_some()
}

/// Make the GNOME custom keybinding match `spec` (empty = remove it).
pub fn sync_hotkey(spec: &str) -> Result<(), String> {
    let mut list = parse_list(&gsettings(&["get", LIST_SCHEMA, LIST_KEY])?);
    let listed = list.iter().any(|p| p == PATH);
    let entry = format!("{LIST_SCHEMA}.custom-keybinding:{PATH}");
    if spec.trim().is_empty() {
        if listed {
            list.retain(|p| p != PATH);
            gsettings(&["set", LIST_SCHEMA, LIST_KEY, &format_list(&list)])?;
        }
        for key in ["name", "command", "binding"] {
            gsettings(&["reset", &entry, key])?;
        }
        return Ok(());
    }
    let hk = crate::hotkey::parse(spec).ok_or_else(|| format!("invalid dropdown hotkey {spec:?}"))?;
    let exe = match crate::dropdown::launch_exe().map_err(|e| e.to_string())? {
        None => "rsterm".to_string(),
        Some(p) => shell_quote(&p.to_string_lossy()),
    };
    // GNOME splits the command shell-style, without running a shell.
    for (key, value) in [
        ("name", "rsterm dropdown".to_string()),
        ("command", format!("{exe} --dropdown")),
        ("binding", hk.gtk_accel()),
    ] {
        gsettings(&["set", &entry, key, &gvariant_str(&value)])?;
    }
    if !listed {
        list.push(PATH.to_string());
        gsettings(&["set", LIST_SCHEMA, LIST_KEY, &format_list(&list)])?;
    }
    Ok(())
}

fn gsettings(args: &[&str]) -> Result<String, String> {
    let out = Command::new("gsettings")
        .args(args)
        .output()
        .map_err(|e| format!("could not run gsettings: {e}"))?;
    if !out.status.success() {
        return Err(format!("gsettings failed: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Parse a GVariant `as` as gsettings prints it: `@as []` or `['/a/', '/b/']`.
fn parse_list(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur: Option<String> = None;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match (&mut cur, c) {
            (None, '\'') => cur = Some(String::new()),
            (Some(_), '\'') => out.extend(cur.take()),
            (Some(v), '\\') => v.extend(chars.next()),
            (Some(v), c) => v.push(c),
            (None, _) => {}
        }
    }
    out
}

fn format_list(list: &[String]) -> String {
    if list.is_empty() {
        return "@as []".into();
    }
    let items: Vec<String> = list.iter().map(|s| gvariant_str(s)).collect();
    format!("[{}]", items.join(", "))
}

fn gvariant_str(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
}

fn shell_quote(s: &str) -> String {
    if s.chars().all(|c| c.is_ascii_alphanumeric() || "/._-+".contains(c)) {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gvariant_lists_roundtrip() {
        assert!(parse_list("@as []").is_empty());
        let l = parse_list("['/a/custom0/', '/b/it\\'s/']");
        assert_eq!(l, ["/a/custom0/", "/b/it's/"]);
        assert_eq!(parse_list(&format_list(&l)), l);
        assert_eq!(format_list(&[]), "@as []");
        assert_eq!(shell_quote("/home/x y/rsterm"), "'/home/x y/rsterm'");
    }
}
