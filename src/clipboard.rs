//! Putting text on the clipboard.

use std::fs::OpenOptions;
use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};

/// Clipboard tools to try, in order.
const TOOLS: [(&str, &[&str]); 4] = [
    ("pbcopy", &[]),
    ("wl-copy", &[]),
    ("xclip", &["-selection", "clipboard"]),
    ("xsel", &["--clipboard", "--input"]),
];

/// Above this many encoded bytes, some terminals and tmux drop an OSC 52 copy.
pub const OSC52_SAFE_LEN: usize = 100_000;

pub enum Via {
    Tool(&'static str),
    /// Sent to the terminal, which may or may not act on it.
    Osc52 {
        encoded_len: usize,
    },
}

/// Copy `text` with a clipboard tool, or with OSC 52 if none works.
///
/// Over SSH a tool would copy to the remote machine's clipboard, so OSC 52
/// goes first there, as it does with `force_osc52`.
pub fn copy(text: &str, force_osc52: bool) -> Result<Via> {
    if !force_osc52 && !over_ssh() {
        if let Some(tool) = TOOLS
            .iter()
            .find_map(|&(bin, args)| try_tool(bin, args, text))
        {
            return Ok(Via::Tool(tool));
        }
    }
    let encoded = STANDARD.encode(text);
    let mut tty =
        OpenOptions::new()
            .write(true)
            .open("/dev/tty")
            .context(if force_osc52 || over_ssh() {
                "cannot open /dev/tty to send OSC 52"
            } else {
                "no clipboard tool worked, and /dev/tty is unavailable for OSC 52"
            })?;
    write!(tty, "\x1b]52;c;{encoded}\x07")?;
    Ok(Via::Osc52 {
        encoded_len: encoded.len(),
    })
}

fn over_ssh() -> bool {
    ["SSH_TTY", "SSH_CONNECTION"]
        .iter()
        .any(|v| std::env::var_os(v).is_some_and(|s| !s.is_empty()))
}

fn try_tool(bin: &'static str, args: &[&str], text: &str) -> Option<&'static str> {
    let mut child = Command::new(bin)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(text.as_bytes());
    } // stdin is dropped here, which sends EOF
    child.wait().is_ok_and(|s| s.success()).then_some(bin)
}
