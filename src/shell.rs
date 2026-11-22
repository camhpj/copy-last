//! `cl shell`: run the user's shell in a PTY and record what it prints.

use std::convert::Infallible;
use std::ffi::{OsStr, OsString};
use std::io::{self, IsTerminal, Read, Write};
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use signal_hook::consts::{SIGHUP, SIGINT, SIGQUIT, SIGTERM, SIGWINCH};
use signal_hook::iterator::Signals;

use crate::recorder::{Event, Recorder};
use crate::store::{self, Session, ENV_SESSION, ENV_TOKEN};
use crate::term::{self, RawMode};

/// After the shell exits, keep showing output until none arrives for this
/// long. The PTY only closes once every process holding it exits, so a
/// background job can keep it open long after the shell is gone.
const DRAIN_GRACE: Duration = Duration::from_millis(200);
/// Exit at most this long after the shell, even if a background job is
/// still printing.
const DRAIN_MAX: Duration = Duration::from_secs(2);

/// Run the recording shell. Exits the process when the shell exits.
pub fn run() -> Result<Infallible> {
    if std::env::var_os(ENV_SESSION).is_some_and(|v| !v.is_empty()) {
        bail!("already inside a cl session");
    }
    if !io::stdin().is_terminal() {
        bail!("`cl shell` needs an interactive terminal");
    }
    let shell = user_shell();
    match start(&shell) {
        Ok(started) => started.relay(),
        Err(e) => {
            // Start the shell anyway, so `exec cl shell` in an rc file
            // can't leave you without a terminal. An empty CL_SESSION
            // tells the rc file not to try again.
            eprintln!("cl: {e:#}");
            eprintln!("cl: running {} without recording", shell.to_string_lossy());
            let err = Command::new(&shell).env(ENV_SESSION, "").exec();
            Err(err).with_context(|| format!("cannot start {}", shell.to_string_lossy()))
        }
    }
}

fn user_shell() -> OsString {
    std::env::var_os("SHELL")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/bin/sh".into())
}

struct Started {
    session: Session,
    token: String,
    signals: Signals,
    raw: RawMode,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    reader: Box<dyn Read + Send>,
    writer: Box<dyn Write + Send>,
}

/// Everything that can fail happens here, before the shell starts, so a
/// failure can fall back to a plain shell.
fn start(shell: &OsStr) -> Result<Started> {
    let session = Session::create(&store::state_dir()?)?;
    let token = store::random_hex(16)?;
    // Registered before raw mode, so no signal can kill us with the
    // terminal still raw. Signals that arrive now are handled in `relay`.
    let signals = Signals::new([SIGWINCH, SIGHUP, SIGTERM, SIGINT, SIGQUIT])?;
    let (cols, rows) = term::size().unwrap_or((80, 24));
    let pty = native_pty_system().openpty(pty_size(cols, rows))?;
    let reader = pty.master.try_clone_reader()?;
    let writer = pty.master.take_writer()?;

    // The environment is inherited, so a login shell isn't needed.
    let mut cmd = CommandBuilder::new(shell);
    cmd.cwd(std::env::current_dir()?);
    cmd.env(ENV_SESSION, session.history_path());
    cmd.env(ENV_TOKEN, &token);

    let raw = RawMode::enable().context("cannot set the terminal to raw mode")?;
    let child = pty
        .slave
        .spawn_command(cmd)
        .with_context(|| format!("cannot start {}", shell.to_string_lossy()))?;
    drop(pty.slave);
    Ok(Started {
        session,
        token,
        signals,
        raw,
        master: pty.master,
        child,
        reader,
        writer,
    })
}

impl Started {
    /// Copy keys to the shell and output to the screen until the shell exits.
    fn relay(self) -> Result<Infallible> {
        let Self {
            session,
            token,
            mut signals,
            raw: _raw,
            master,
            child,
            mut reader,
            mut writer,
        } = self;
        let session = Arc::new(Mutex::new(session));
        // Bytes read from the shell so far, to tell when output has stopped.
        let progress = Arc::new(AtomicU64::new(0));

        // Resize the inner PTY with the real terminal, and clean up if
        // we're killed (closing the window sends SIGHUP).
        let s = Arc::clone(&session);
        thread::spawn(move || {
            for sig in signals.forever() {
                if sig != SIGWINCH {
                    shutdown(&s, 128 + sig);
                }
                if let Some((cols, rows)) = term::size() {
                    let _ = master.resize(pty_size(cols, rows));
                }
            }
        });

        // Keyboard to shell. If the terminal goes away, so do we.
        let s = Arc::clone(&session);
        thread::spawn(move || {
            let mut stdin = io::stdin();
            let mut buf = [0u8; 4096];
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) => shutdown(&s, 1),
                    Ok(n) => {
                        if writer.write_all(&buf[..n]).is_err() {
                            break; // the shell is gone; the exit thread takes over
                        }
                        let _ = writer.flush();
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => shutdown(&s, 1),
                }
            }
        });

        // Exit when the shell does, even if a background job keeps the
        // PTY open. Wait until output stops first, so none is lost.
        let (exited, exit_code) = mpsc::channel();
        let s = Arc::clone(&session);
        let p = Arc::clone(&progress);
        thread::spawn(move || {
            let code = wait_for_exit(child);
            let _ = exited.send(code);
            let deadline = std::time::Instant::now() + DRAIN_MAX;
            loop {
                let before = p.load(Ordering::Relaxed);
                thread::sleep(DRAIN_GRACE);
                if p.load(Ordering::Relaxed) == before || std::time::Instant::now() >= deadline {
                    shutdown(&s, code);
                }
            }
        });

        // Shell to screen, recording as it goes.
        let mut rec = Recorder::new(&token);
        let mut stdout = io::stdout().lock();
        let mut buf = vec![0u8; 64 * 1024];
        let mut out = Vec::with_capacity(buf.len());
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    out.clear();
                    rec.feed(&buf[..n], &mut out);
                    if stdout
                        .write_all(&out)
                        .and_then(|()| stdout.flush())
                        .is_err()
                    {
                        shutdown(&session, 1);
                    }
                    store_events(&mut rec, &session);
                    // Counted once shown, so a slow terminal isn't cut off.
                    progress.fetch_add(n as u64, Ordering::Relaxed);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                // Linux reports EIO once the last process holding the PTY exits.
                Err(e) if e.raw_os_error() == Some(libc::EIO) => break,
                Err(_) => shutdown(&session, 1),
            }
        }
        out.clear();
        rec.flush(&mut out);
        let _ = stdout.write_all(&out).and_then(|()| stdout.flush());
        shutdown(&session, exit_code.recv().unwrap_or(1))
    }
}

/// Wait for the shell and return its exit code, or 128 + the signal that
/// killed it. portable-pty reports signal deaths as 1, so reap it directly.
fn wait_for_exit(mut child: Box<dyn Child + Send + Sync>) -> i32 {
    let Some(pid) = child
        .process_id()
        .and_then(|p| libc::pid_t::try_from(p).ok())
    else {
        return child
            .wait()
            .map_or(1, |s| i32::try_from(s.exit_code()).unwrap_or(1));
    };
    let mut status = 0;
    loop {
        // SAFETY: `status` is a valid out-pointer, and `pid` is our child.
        let ret = unsafe { libc::waitpid(pid, &raw mut status, 0) };
        if ret == pid {
            break;
        }
        if ret == -1 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return 1;
        }
    }
    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else if libc::WIFSIGNALED(status) {
        128 + libc::WTERMSIG(status)
    } else {
        1
    }
}

fn store_events(rec: &mut Recorder, session: &Mutex<Session>) {
    let mut events = rec.events().peekable();
    if events.peek().is_none() {
        return;
    }
    let mut session = session.lock().unwrap_or_else(PoisonError::into_inner);
    for event in events {
        // A full disk must never take the user's shell down with it.
        let _ = match event {
            Event::Ready => session.mark_ready(),
            Event::Entry(entry) => session.append(&entry),
        };
    }
}

/// Delete the session's files, restore the terminal and exit.
fn shutdown(session: &Mutex<Session>, code: i32) -> ! {
    // Keep the lock until exit, so no other thread writes the files again.
    let mut session = session.lock().unwrap_or_else(PoisonError::into_inner);
    session.remove();
    term::restore();
    std::process::exit(code)
}

const fn pty_size(cols: u16, rows: u16) -> PtySize {
    PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}
