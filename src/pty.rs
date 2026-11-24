//! Pseudo-terminal: spawn the shell on a fresh pty and expose the master side.

use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};

pub struct Pty {
    pub master: File,
    pub child: Child,
}

fn winsize(cols: u16, rows: u16, cw: u16, ch: u16) -> libc::winsize {
    libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: cols * cw, ws_ypixel: rows * ch }
}

impl Pty {
    /// Spawn `command` (run via the shell's `-c`) or, if empty, the login shell.
    pub fn spawn(cols: u16, rows: u16, cw: u16, ch: u16, command: &str, cwd: Option<&str>) -> io::Result<Pty> {
        let mut master: libc::c_int = -1;
        let mut slave: libc::c_int = -1;
        let ws = winsize(cols, rows, cw, ch);
        // SAFETY: openpty writes two fds into the provided out pointers.
        let rc = unsafe {
            libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null(), &ws)
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fds were just returned by openpty and are owned by us.
        let master = unsafe { OwnedFd::from_raw_fd(master) };
        let slave = unsafe { OwnedFd::from_raw_fd(slave) };
        unsafe {
            libc::fcntl(master.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
            libc::fcntl(slave.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
        }

        let shell = std::env::var("SHELL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| {
            // Fall back to the passwd entry.
            unsafe {
                let pw = libc::getpwuid(libc::getuid());
                if !pw.is_null() && !(*pw).pw_shell.is_null() {
                    return std::ffi::CStr::from_ptr((*pw).pw_shell).to_string_lossy().into_owned();
                }
            }
            "/bin/sh".into()
        });

        let mut cmd = Command::new(&shell);
        if !command.trim().is_empty() {
            cmd.arg("-c").arg(command);
        }
        if let Some(dir) = cwd.filter(|d| std::path::Path::new(d).is_dir()) {
            cmd.current_dir(dir);
        }
        cmd.stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(Stdio::from(slave))
            .env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor")
            .env("TERM_PROGRAM", "rsterm")
            .env_remove("DESKTOP_STARTUP_ID")
            .env_remove("XDG_ACTIVATION_TOKEN");
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                for sig in [
                    libc::SIGCHLD,
                    libc::SIGHUP,
                    libc::SIGINT,
                    libc::SIGQUIT,
                    libc::SIGTERM,
                    libc::SIGALRM,
                    libc::SIGPIPE,
                ] {
                    libc::signal(sig, libc::SIG_DFL);
                }
                Ok(())
            });
        }
        let child = cmd.spawn()?;
        Ok(Pty { master: File::from(master), child })
    }

    pub fn resize(&self, cols: u16, rows: u16, cw: u16, ch: u16) {
        let ws = winsize(cols, rows, cw, ch);
        // SAFETY: valid fd and winsize struct.
        unsafe {
            libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &ws);
        }
    }
}
