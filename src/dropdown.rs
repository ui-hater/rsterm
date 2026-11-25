//! Dropdown (quake-style) mode: one long-lived instance that a hotkey shows
//! and hides.
//!
//! The instance grabs its own hotkey on X11 (see `hotkey.rs`); binding
//! `rsterm --dropdown` in the desktop also works (and is the only way on
//! Wayland). The first such
//! process takes an exclusive `flock` and listens on a Unix socket; later
//! invocations find the lock held, send their command over the socket and
//! exit immediately.

use std::io::{Read, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cmd {
    Toggle,
    Show,
    Hide,
}

impl Cmd {
    pub fn parse(s: &str) -> Option<Cmd> {
        match s.trim() {
            "toggle" => Some(Cmd::Toggle),
            "show" => Some(Cmd::Show),
            "hide" => Some(Cmd::Hide),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Cmd::Toggle => "toggle",
            Cmd::Show => "show",
            Cmd::Hide => "hide",
        }
    }
}

/// Per-user, per-display base path (without extension).
fn base_path() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute() && p.is_dir())
        .unwrap_or_else(|| PathBuf::from(format!("/tmp/rsterm-{}", unsafe { libc::getuid() })));
    let _ = std::fs::create_dir_all(&dir);
    let display = std::env::var("WAYLAND_DISPLAY")
        .or_else(|_| std::env::var("DISPLAY"))
        .unwrap_or_default()
        .replace(|c: char| !c.is_ascii_alphanumeric(), "_");
    dir.join(format!("rsterm-dropdown{display}"))
}

/// Held for the lifetime of the dropdown instance.
pub struct Instance {
    _lock: std::fs::File,
    listener: UnixListener,
}

pub enum Role {
    /// We are the dropdown; start the UI and call `Instance::serve`.
    Primary(Instance),
    /// Another instance is running and has been sent the command.
    Forwarded,
}

/// Become the dropdown instance, or forward `cmd` to the existing one.
pub fn claim(cmd: Cmd) -> std::io::Result<Role> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let base = base_path();
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(base.with_extension("lock"))?;
    let sock = base.with_extension("sock");
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        // The lock holder may still be starting up; give it a moment to bind.
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match UnixStream::connect(&sock) {
                Ok(mut s) => {
                    s.write_all(format!("{}\n", cmd.name()).as_bytes())?;
                    return Ok(Role::Forwarded);
                }
                Err(e) if Instant::now() >= deadline => return Err(e),
                Err(_) => std::thread::sleep(Duration::from_millis(50)),
            }
        }
    }
    // We hold the lock, so any existing socket file is stale.
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock)?;
    std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600))?;
    Ok(Role::Primary(Instance { _lock: lock, listener }))
}

impl Instance {
    /// Spawn the listener thread; `on_cmd` runs on it for every command.
    pub fn serve(self, on_cmd: impl Fn(Cmd) + Send + 'static) {
        std::thread::Builder::new()
            .name("dropdown-ipc".into())
            .spawn(move || {
                let _lock = self._lock;
                for conn in self.listener.incoming() {
                    let Ok(mut conn) = conn else { continue };
                    let _ = conn.set_read_timeout(Some(Duration::from_secs(1)));
                    let mut buf = [0u8; 64];
                    let mut n = 0;
                    while n < buf.len() {
                        match conn.read(&mut buf[n..]) {
                            Ok(0) | Err(_) => break,
                            Ok(k) => n += k,
                        }
                        if buf[..n].contains(&b'\n') {
                            break;
                        }
                    }
                    if let Some(c) = std::str::from_utf8(&buf[..n]).ok().and_then(Cmd::parse) {
                        on_cmd(c);
                    }
                }
            })
            .expect("spawn dropdown listener");
    }
}

/// XDG autostart entry that starts the dropdown hidden at login.
fn autostart_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"));
    base.join("autostart").join("rsterm-dropdown.desktop")
}

/// The program for the autostart entry's `Exec`/`TryExec`: plain `rsterm`
/// when that is what `$PATH` finds (so the entry keeps working if the binary
/// is reinstalled elsewhere, and doesn't bake in a home directory path),
/// otherwise the absolute path of this binary, quoted per the Desktop Entry
/// spec (`TryExec` takes the path unquoted).
fn autostart_exe() -> std::io::Result<(String, String)> {
    let exe = std::env::current_exe()?;
    let canon = |p: &std::path::Path| std::fs::canonicalize(p).ok();
    let me = canon(&exe);
    let on_path = std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path)
            .map(|d| d.join("rsterm"))
            .find(|p| p.is_file())
            .is_some_and(|p| me.is_some() && canon(&p) == me)
    });
    if on_path {
        return Ok(("rsterm".into(), "rsterm".into()));
    }
    let s = exe.to_string_lossy();
    let try_exec = s.replace('\\', "\\\\");
    if s.chars().all(|c| c.is_ascii_alphanumeric() || "/._-+".contains(c)) {
        return Ok((try_exec, s.into_owned()));
    }
    let mut q = String::from("\"");
    for c in s.chars() {
        if matches!(c, '"' | '`' | '$' | '\\') {
            q.push('\\');
        }
        q.push(c);
    }
    q.push('"');
    // The value is itself a string, whose own `\` escapes apply first.
    Ok((try_exec, q.replace('\\', "\\\\")))
}

/// Create or remove the autostart entry to match `enabled`.
pub fn set_autostart(enabled: bool) -> std::io::Result<()> {
    let path = autostart_path();
    if !enabled {
        return match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    let (try_exec, exe) = autostart_exe()?;
    let text = format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=rsterm dropdown\n\
         Comment=Start the rsterm dropdown terminal hidden; show it with its hotkey\n\
         TryExec={try_exec}\n\
         Exec={exe} --dropdown hide\n\
         Icon=utilities-terminal\n\
         Terminal=false\n\
         X-GNOME-Autostart-enabled=true\n"
    );
    if std::fs::read_to_string(&path).is_ok_and(|t| t == text) {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, text)
}
