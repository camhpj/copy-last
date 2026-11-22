//! Raw mode and window size for the real terminal on stdin.

use std::io;
use std::sync::OnceLock;

/// Terminal settings from before raw mode, so any thread can restore them.
static SAVED: OnceLock<libc::termios> = OnceLock::new();

/// Raw mode on stdin, restored when dropped.
pub struct RawMode;

impl RawMode {
    pub fn enable() -> io::Result<Self> {
        // SAFETY: termios is plain data, and tcgetattr fills it in before use.
        let mut t: libc::termios = unsafe { std::mem::zeroed() };
        check(unsafe { libc::tcgetattr(libc::STDIN_FILENO, &raw mut t) })?;
        let saved = *SAVED.get_or_init(|| t);
        let mut settings = saved;
        // SAFETY: `settings` is a valid termios.
        unsafe { libc::cfmakeraw(&raw mut settings) };
        check(unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw const settings) })?;
        Ok(Self)
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        restore();
    }
}

/// Put the terminal back the way it was before raw mode.
pub fn restore() {
    if let Some(t) = SAVED.get() {
        // SAFETY: `t` came from tcgetattr on the same descriptor.
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, t) };
    }
}

/// The terminal's size as (columns, rows).
pub fn size() -> Option<(u16, u16)> {
    // SAFETY: TIOCGWINSZ fills in a winsize struct and touches nothing else.
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::ioctl(libc::STDIN_FILENO, libc::TIOCGWINSZ, &mut ws) } == 0;
    (ok && ws.ws_col > 0 && ws.ws_row > 0).then_some((ws.ws_col, ws.ws_row))
}

fn check(ret: libc::c_int) -> io::Result<()> {
    if ret == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
