//! A session is one tab: a pty + child process, its `Term`, and the two
//! background threads that feed it.
//!
//!   pty-reader: blocking reads, hands chunks over immediately (so the child
//!               is never throttled by our parsing);
//!   vt-parser:  drains every queued chunk, parses under one lock, and wakes
//!               the UI at most once per frame via `wakeup_pending`.

use crate::pty::Pty;
use crate::term::Term;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

pub struct Shared {
    pub term: Mutex<Term>,
    pub wakeup_pending: AtomicBool,
}

pub struct Session {
    pub id: u64,
    pub shared: Arc<Shared>,
    pub pty: Pty,
    writer: File,
    /// New output arrived while this tab was in the background.
    pub activity: bool,
    pub bell: bool,
}

pub struct Spawn<'a> {
    pub cols: usize,
    pub rows: usize,
    pub cell_w: u32,
    pub cell_h: u32,
    pub command: &'a str,
    pub cwd: Option<&'a str>,
    pub shell_integration: bool,
    pub term: Term,
}

impl Session {
    /// `wake(id)` is called from the parser thread after new output was parsed
    /// (at most once until the UI clears `wakeup_pending`); `exited(id)` when
    /// the child's pty closes.
    pub fn spawn(
        id: u64,
        sp: Spawn<'_>,
        wake: impl Fn(u64) + Send + 'static,
        exited: impl Fn(u64) + Send + 'static,
    ) -> std::io::Result<Session> {
        let pty = Pty::spawn(
            sp.cols as u16,
            sp.rows as u16,
            sp.cell_w as u16,
            sp.cell_h as u16,
            sp.command,
            sp.cwd,
            sp.shell_integration,
        )?;
        let shared = Arc::new(Shared { term: Mutex::new(sp.term), wakeup_pending: AtomicBool::new(false) });

        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(256);
        let (recycle_tx, recycle_rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let mut reader = pty.master.try_clone()?;
        std::thread::Builder::new().name(format!("pty-reader-{id}")).spawn(move || {
            const CHUNK: usize = 1 << 16;
            loop {
                let mut buf = recycle_rx.try_recv().unwrap_or_else(|_| Vec::with_capacity(CHUNK));
                buf.resize(CHUNK, 0);
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        buf.truncate(n);
                        if tx.send(buf).is_err() {
                            break;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
        })?;

        let sh = shared.clone();
        let mut writer = pty.master.try_clone()?;
        std::thread::Builder::new().name(format!("vt-parser-{id}")).spawn(move || {
            let mut parser = vte::Parser::new();
            while let Ok(first) = rx.recv() {
                let responses = {
                    let mut t = sh.term.lock().unwrap();
                    parser.advance(&mut *t, &first);
                    let _ = recycle_tx.send(first);
                    // Parse whatever else is queued under the same lock, bounded
                    // so the renderer still gets the lock regularly.
                    let mut budget = 1usize << 20;
                    while budget > 0 {
                        let Ok(more) = rx.try_recv() else { break };
                        parser.advance(&mut *t, &more);
                        budget = budget.saturating_sub(more.len());
                        let _ = recycle_tx.send(more);
                    }
                    std::mem::take(&mut t.responses)
                };
                if !responses.is_empty() {
                    let _ = writer.write_all(&responses);
                }
                if !sh.wakeup_pending.swap(true, Ordering::AcqRel) {
                    wake(id);
                }
            }
            exited(id);
        })?;

        let writer = pty.master.try_clone()?;
        Ok(Session { id, shared, pty, writer, activity: false, bell: false })
    }

    pub fn write(&mut self, bytes: &[u8]) {
        let _ = self.writer.write_all(bytes);
    }

    pub fn resize(&self, cols: usize, rows: usize, cell_w: u32, cell_h: u32) {
        self.shared.term.lock().unwrap().resize(cols, rows);
        self.pty.resize(cols as u16, rows as u16, cell_w as u16, cell_h as u16);
    }

    pub fn title(&self) -> String {
        self.shared.term.lock().unwrap().title.clone()
    }

    /// Working directory of whatever runs in the foreground: OSC 7 if the
    /// shell reports it, otherwise /proc of the foreground process group.
    pub fn cwd(&self) -> Option<String> {
        if let Some(c) = self.shared.term.lock().unwrap().cwd.clone() {
            return Some(c);
        }
        // SAFETY: valid fd.
        let pgrp = unsafe { libc::tcgetpgrp(self.pty.master.as_raw_fd()) };
        let pid = if pgrp > 0 { pgrp as u32 } else { self.pty.child.id() };
        std::fs::read_link(format!("/proc/{pid}/cwd")).ok().map(|p| p.to_string_lossy().into_owned())
    }

    pub fn hangup(&self) {
        // SAFETY: plain signal to our own child.
        unsafe {
            libc::kill(self.pty.child.id() as i32, libc::SIGHUP);
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.hangup();
        // Reap the child in the background so closed tabs leave no zombies.
        if let Ok(None) = self.pty.child.try_wait() {
            let pid = self.pty.child.id() as libc::pid_t;
            std::thread::spawn(move || unsafe {
                libc::waitpid(pid, std::ptr::null_mut(), 0);
            });
        }
    }
}
