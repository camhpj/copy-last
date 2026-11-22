//! cl — copy your last N terminal commands and their output.
//!
//! A terminal doesn't keep a record of "which bytes were output of which
//! command", so `cl shell` runs your shell inside a thin PTY wrapper.
//! Tiny shell hooks emit invisible markers before/after each command; the
//! wrapper strips them from the stream and stores each command + output.
//! `cl -n N -o OFFSET` then copies from that per-session history.

use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::{self, BufWriter, IsTerminal, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;

use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use clap::{Args, Parser, Subcommand, ValueEnum};
use crossterm::terminal;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use serde::{Deserialize, Serialize};
use signal_hook::{consts::SIGWINCH, iterator::Signals};

/// Private OSC sequence: ESC ] 7777 ; <C|D> ; <payload> BEL
const MARKER: &[u8] = b"\x1b]7777;";
/// Commands kept per session.
const HISTORY_LIMIT: usize = 100;
/// Max output kept per command (the tail is kept).
const MAX_OUTPUT: usize = 1 << 20;

// ───────────────────────────── CLI ─────────────────────────────

#[derive(Parser)]
#[command(
    name = "cl",
    version,
    about = "Copy your last N terminal commands and their output",
    args_conflicts_with_subcommands = true
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    #[command(flatten)]
    copy: CopyArgs,
}

#[derive(Args)]
struct CopyArgs {
    /// Number of commands to copy
    #[arg(short = 'n', long, default_value_t = 1)]
    count: usize,
    /// Skip this many of the most recent commands first (0 = last command)
    #[arg(short, long, default_value_t = 0)]
    offset: usize,
    /// Copy only the command lines
    #[arg(short = 'c', long, conflicts_with = "output_only")]
    commands_only: bool,
    /// Copy only the output
    #[arg(short = 'O', long)]
    output_only: bool,
    /// Print to stdout instead of copying
    #[arg(short, long)]
    print: bool,
    /// Always copy via the OSC 52 terminal escape (handy over SSH)
    #[arg(long)]
    osc52: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start a recording shell session
    Shell,
    /// Print shell integration (add `eval "$(cl init zsh)"` to your rc file)
    Init { shell: ShellKind },
    /// List recorded commands with the offset to use for each
    List {
        #[arg(short = 'n', long, default_value_t = 20)]
        count: usize,
    },
}

#[derive(Clone, ValueEnum)]
enum ShellKind {
    Zsh,
    Bash,
}

fn main() {
    let cli = Cli::parse();
    let result = match cli.cmd {
        Some(Cmd::Shell) => match run_shell() {
            Ok(code) => std::process::exit(code),
            Err(e) => Err(e),
        },
        Some(Cmd::Init { shell }) => {
            print!(
                "{}",
                match shell {
                    ShellKind::Zsh => ZSH_INIT,
                    ShellKind::Bash => BASH_INIT,
                }
            );
            Ok(())
        }
        Some(Cmd::List { count }) => list(count),
        None => copy_cmd(&cli.copy),
    };
    if let Err(e) = result {
        eprintln!("cl: {e:#}");
        std::process::exit(1);
    }
}

// ─────────────────────────── storage ───────────────────────────

#[derive(Serialize, Deserialize)]
struct Entry {
    command: String,
    output: String,
    exit: i32,
    #[serde(default)]
    truncated: bool,
}

fn state_dir() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .context("HOME is not set")?;
    let dir = base.join("cl");
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)?;
    Ok(dir)
}

fn load() -> Result<Vec<Entry>> {
    let path = std::env::var_os("CL_SESSION")
        .context("not inside a cl session; start one with `cl shell`")?;
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    Ok(text
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect())
}

// ─────────────────────── copy / list ───────────────────────

fn copy_cmd(a: &CopyArgs) -> Result<()> {
    let entries = load()?;
    let len = entries.len();
    if len == 0 {
        bail!("no commands recorded yet in this session");
    }
    if a.offset >= len {
        bail!(
            "offset {} is out of range: only {len} command(s) recorded",
            a.offset
        );
    }
    let end = len - a.offset;
    let picked = &entries[end.saturating_sub(a.count.max(1))..end];
    let text = render(picked, a);

    if a.print {
        println!("{text}");
        return Ok(());
    }
    let via = copy_to_clipboard(&text, a.osc52)?;
    eprintln!(
        "cl: copied {} command(s), {} line(s) via {via}",
        picked.len(),
        text.lines().count()
    );
    Ok(())
}

fn render(entries: &[Entry], a: &CopyArgs) -> String {
    let blocks: Vec<String> = entries
        .iter()
        .map(|e| {
            let mut b = String::new();
            if !a.output_only {
                b += &format!("$ {}\n", e.command);
            }
            if !a.commands_only {
                if e.truncated {
                    b += "[… earlier output truncated …]\n";
                }
                if !e.output.is_empty() {
                    b += &e.output;
                    b.push('\n');
                }
                if !a.output_only && e.exit != 0 {
                    b += &format!("[exit {}]\n", e.exit);
                }
            }
            b.trim_end().to_string()
        })
        .collect();
    blocks.join(if a.commands_only { "\n" } else { "\n\n" })
}

fn list(count: usize) -> Result<()> {
    let entries = load()?;
    if entries.is_empty() {
        eprintln!("cl: no commands recorded yet in this session");
    }
    for (offset, e) in entries.iter().rev().take(count).enumerate() {
        let first = e.command.lines().next().unwrap_or("");
        let short: String = if first.chars().count() > 50 {
            first.chars().take(49).chain(['…']).collect()
        } else {
            first.to_string()
        };
        println!(
            "{offset:>3}  {short:<50}  exit {:<3}  {} lines",
            e.exit,
            e.output.lines().count()
        );
    }
    Ok(())
}

fn copy_to_clipboard(text: &str, force_osc52: bool) -> Result<&'static str> {
    if !force_osc52 {
        let tools: [(&'static str, &[&str]); 4] = [
            ("pbcopy", &[]),
            ("wl-copy", &[]),
            ("xclip", &["-selection", "clipboard"]),
            ("xsel", &["--clipboard", "--input"]),
        ];
        for (bin, args) in tools {
            let Ok(mut child) = Command::new(bin)
                .args(args)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            else {
                continue;
            };
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            } // stdin dropped here → EOF
            if child.wait().map(|s| s.success()).unwrap_or(false) {
                return Ok(bin);
            }
        }
    }
    // Fallback: ask the terminal emulator to set the clipboard.
    let mut tty = OpenOptions::new()
        .write(true)
        .open("/dev/tty")
        .context("no clipboard tool found and /dev/tty unavailable")?;
    write!(tty, "\x1b]52;c;{}\x07", STANDARD.encode(text))?;
    Ok("OSC 52")
}

// ─────────────────────── recording shell ───────────────────────

struct RawMode;
impl RawMode {
    fn enable() -> Result<Self> {
        terminal::enable_raw_mode()?;
        Ok(Self)
    }
}
impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}

fn run_shell() -> Result<i32> {
    if std::env::var_os("CL_SESSION").is_some() {
        bail!("already inside a cl session");
    }
    if !io::stdin().is_terminal() {
        bail!("`cl shell` needs an interactive terminal");
    }

    let path = state_dir()?.join(format!("{}.jsonl", std::process::id()));
    let (cols, rows) = terminal::size().unwrap_or((80, 24));
    let size = |cols, rows| PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    };

    let pty = native_pty_system().openpty(size(cols, rows))?;
    // Run the user's $SHELL (env is inherited, so no login shell needed).
    let mut cmd = match std::env::var_os("SHELL") {
        Some(sh) => CommandBuilder::new(sh),
        None => CommandBuilder::new_default_prog(),
    };
    cmd.cwd(std::env::current_dir()?);
    cmd.env("CL_SESSION", &path);
    let mut child = pty.slave.spawn_command(cmd)?;
    drop(pty.slave);

    let mut reader = pty.master.try_clone_reader()?;
    let mut writer = pty.master.take_writer()?;
    let master = pty.master;

    // Keep the inner PTY's size in sync with the real terminal.
    let mut signals = Signals::new([SIGWINCH])?;
    thread::spawn(move || {
        for _ in signals.forever() {
            if let Ok((cols, rows)) = terminal::size() {
                let _ = master.resize(size(cols, rows));
            }
        }
    });

    // Keyboard → shell.
    thread::spawn(move || {
        let mut stdin = io::stdin();
        let mut buf = [0u8; 4096];
        while let Ok(n) = stdin.read(&mut buf) {
            if n == 0 || writer.write_all(&buf[..n]).is_err() {
                break;
            }
            let _ = writer.flush();
        }
    });

    // Shell → screen (and recorder).
    let raw = RawMode::enable()?;
    let mut rec = Recorder::new(path.clone());
    let mut stdout = io::stdout().lock();
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let out = rec.feed(&buf[..n]);
                stdout.write_all(&out)?;
                stdout.flush()?;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break, // EIO once the shell exits
        }
    }
    drop(raw);
    let _ = fs::remove_file(&path);
    let _ = fs::remove_file(path.with_extension("tmp"));
    Ok(child.wait()?.exit_code() as i32)
}

struct Running {
    command: String,
    raw: Vec<u8>,
    truncated: bool,
}

struct Recorder {
    path: PathBuf,
    carry: Vec<u8>, // possible partial marker held from the previous chunk
    running: Option<Running>,
    history: VecDeque<Entry>,
}

impl Recorder {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            carry: Vec::new(),
            running: None,
            history: VecDeque::new(),
        }
    }

    /// Consume PTY output; return the bytes to show (markers removed).
    fn feed(&mut self, chunk: &[u8]) -> Vec<u8> {
        let mut buf = std::mem::take(&mut self.carry);
        buf.extend_from_slice(chunk);
        let mut out = Vec::with_capacity(buf.len());
        let mut i = 0;
        while i < buf.len() {
            let Some(rel) = find(&buf[i..], MARKER) else {
                // Hold back a trailing partial marker, pass the rest.
                let rest = &buf[i..];
                let keep = partial_marker_len(rest);
                self.pass(&rest[..rest.len() - keep], &mut out);
                self.carry = rest[rest.len() - keep..].to_vec();
                return out;
            };
            let start = i + rel;
            self.pass(&buf[i..start], &mut out);
            let body = start + MARKER.len();
            match buf[body..].iter().position(|&b| b == 0x07) {
                Some(len) => {
                    self.marker(&buf[body..body + len]);
                    i = body + len + 1;
                }
                None if buf.len() - start < 64 * 1024 => {
                    self.carry = buf[start..].to_vec();
                    return out;
                }
                None => {
                    // Not really our marker; give up and pass it through.
                    self.pass(&buf[start..], &mut out);
                    return out;
                }
            }
        }
        out
    }

    fn pass(&mut self, bytes: &[u8], out: &mut Vec<u8>) {
        out.extend_from_slice(bytes);
        if let Some(run) = &mut self.running {
            run.raw.extend_from_slice(bytes);
            if run.raw.len() > 2 * MAX_OUTPUT {
                let cut = run.raw.len() - MAX_OUTPUT;
                run.raw.drain(..cut);
                run.truncated = true;
            }
        }
    }

    fn marker(&mut self, payload: &[u8]) {
        let (kind, rest) = match payload.iter().position(|&b| b == b';') {
            Some(i) => (&payload[..i], &payload[i + 1..]),
            None => (payload, &b""[..]),
        };
        match kind {
            b"C" => {
                self.running = Some(Running {
                    command: String::from_utf8_lossy(rest).trim().to_string(),
                    raw: Vec::new(),
                    truncated: false,
                })
            }
            b"D" => {
                if let Some(run) = self.running.take() {
                    let exit = std::str::from_utf8(rest)
                        .ok()
                        .and_then(|s| s.trim().parse().ok())
                        .unwrap_or(0);
                    self.finish(run, exit);
                }
            }
            _ => {}
        }
    }

    fn finish(&mut self, run: Running, exit: i32) {
        let command = run.command;
        // Don't record empty lines or cl itself.
        if command.is_empty()
            || command
                .split_whitespace()
                .next()
                .and_then(|w| std::path::Path::new(w).file_name())
                .is_some_and(|n| n == "cl")
        {
            return;
        }
        self.history.push_back(Entry {
            output: clean(&run.raw),
            command,
            exit,
            truncated: run.truncated,
        });
        while self.history.len() > HISTORY_LIMIT {
            self.history.pop_front();
        }
        let _ = self.save(); // never let a disk error kill the user's shell
    }

    fn save(&self) -> io::Result<()> {
        let tmp = self.path.with_extension("tmp");
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        let mut f = BufWriter::new(file);
        for e in &self.history {
            serde_json::to_writer(&mut f, e)?;
            f.write_all(b"\n")?;
        }
        f.flush()?;
        drop(f);
        fs::rename(&tmp, &self.path)
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn partial_marker_len(buf: &[u8]) -> usize {
    (1..MARKER.len())
        .rev()
        .find(|&k| buf.ends_with(&MARKER[..k]))
        .unwrap_or(0)
}

/// Turn raw terminal bytes into plain text.
fn clean(raw: &[u8]) -> String {
    if find(raw, b"\x1b[?1049h").is_some() || find(raw, b"\x1b[?47h").is_some() {
        return "[full-screen program: output not captured]".into();
    }
    // Last redraw wins on each line (progress bars, zsh's PROMPT_SP), then strip colors etc.
    let lines: Vec<String> = raw
        .split(|&b| b == b'\n')
        .map(|line| {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            let last = line.rsplit(|&b| b == b'\r').next().unwrap_or(&[]);
            let text = String::from_utf8_lossy(&strip_ansi_escapes::strip(last)).into_owned();
            text.trim_end().to_string()
        })
        .collect();
    lines
        .join("\n")
        .trim_start_matches('\n')
        .trim_end()
        .to_string()
}

// ───────────────────────── shell hooks ─────────────────────────

const ZSH_INIT: &str = r#"# cl shell integration (zsh)
_cl_preexec() {
  [[ -n $CL_SESSION ]] || return
  local c=${1//$'\a'/}
  printf '\e]7777;C;%s\a' "${c//$'\e'/}"
}
_cl_precmd() {
  local s=$?
  [[ -n $CL_SESSION ]] && printf '\e]7777;D;%s\a' "$s"
  return $s
}
autoload -Uz add-zsh-hook
add-zsh-hook preexec _cl_preexec
precmd_functions=(_cl_precmd ${precmd_functions:#_cl_precmd})
"#;

const BASH_INIT: &str = r#"# cl shell integration (bash >= 4.4)
_cl_preexec() {
  [[ -n $CL_SESSION ]] || return
  local c; c=$(HISTTIMEFORMAT= history 1); c=${c#*[0-9] }
  c=${c//$'\a'/}
  printf '\e]7777;C;%s\a' "${c//$'\e'/}"
}
_cl_precmd() {
  local s=$?
  [[ -n $CL_SESSION ]] && printf '\e]7777;D;%s\a' "$s"
  return $s
}
[[ $PS0 == *_cl_preexec* ]] || PS0='$(_cl_preexec)'"$PS0"
[[ $PROMPT_COMMAND == *_cl_precmd* ]] || PROMPT_COMMAND="_cl_precmd${PROMPT_COMMAND:+; $PROMPT_COMMAND}"
"#;
