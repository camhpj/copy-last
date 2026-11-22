//! `cl` and `cl list`: read the session's history and show or copy it.

use std::fmt::Write as _;
use std::io::{self, Write as _};

use anyhow::{bail, Result};

use crate::clipboard::{self, Via, OSC52_SAFE_LEN};
use crate::store::{self, Entry};

/// What to copy, from the command line.
#[allow(clippy::struct_excessive_bools)] // they're independent CLI flags
pub struct Selection {
    pub count: usize,
    pub skip: usize,
    pub commands_only: bool,
    pub output_only: bool,
    pub print: bool,
    pub osc52: bool,
}

pub fn copy(sel: &Selection) -> Result<()> {
    let entries = store::recent(sel.skip.saturating_add(sel.count))?;
    if entries.is_empty() {
        bail!("no commands recorded yet in this session");
    }
    if sel.skip >= entries.len() {
        bail!(
            "cannot skip {}: only {} recorded. Run `cl list` to see them.",
            sel.skip,
            plural(entries.len(), "command")
        );
    }
    let picked = &entries[..entries.len() - sel.skip];
    let picked = &picked[picked.len().saturating_sub(sel.count)..];
    if picked.iter().all(|e| !e.recorded) {
        bail!(
            "that command was not recorded: it started with a space, or bash kept it out of \
             its history. Run `cl list` to see what was recorded."
        );
    }
    let text = render(picked, sel);

    if sel.print {
        return print(&format!("{text}\n"));
    }
    let what = format!(
        "{} ({})",
        plural(picked.len(), "command"),
        plural(text.lines().count(), "line")
    );
    match clipboard::copy(&text, sel.osc52)? {
        Via::Tool(tool) => eprintln!("cl: copied {what} with {tool}"),
        Via::Osc52 { encoded_len } => {
            eprintln!("cl: sent {what} with OSC 52");
            if encoded_len > OSC52_SAFE_LEN {
                eprintln!(
                    "cl: the copy is {} KB. Some terminals and tmux ignore OSC 52 copies \
                     this large. If nothing was copied, run `cl -p`.",
                    encoded_len / 1000
                );
            }
        }
    }
    Ok(())
}

fn render(entries: &[Entry], sel: &Selection) -> String {
    let blocks: Vec<String> = entries
        .iter()
        .map(|e| {
            if !e.recorded {
                return "[a command that was not recorded]".to_string();
            }
            let mut b = String::new();
            if !sel.output_only {
                b.push_str("$ ");
                b.push_str(&e.command);
                b.push('\n');
            }
            if !sel.commands_only {
                if e.truncated {
                    b.push_str("[… earlier output truncated …]\n");
                }
                if !e.output.is_empty() {
                    b.push_str(&e.output);
                    b.push('\n');
                }
                if !sel.output_only && e.exit != 0 {
                    let _ = writeln!(b, "[exit {}]", e.exit);
                }
            }
            b.trim_end().to_string()
        })
        .collect();
    blocks.join(if sel.commands_only { "\n" } else { "\n\n" })
}

pub fn list(count: usize) -> Result<()> {
    let entries = store::recent(count)?;
    if entries.is_empty() {
        eprintln!("cl: no commands recorded yet in this session");
    }
    let mut text = String::new();
    for (skip, e) in entries.iter().rev().enumerate() {
        let first = if e.recorded {
            e.command.lines().next().unwrap_or("")
        } else {
            "[not recorded]"
        };
        let short: String = if first.chars().count() > 50 {
            first.chars().take(49).chain(['…']).collect()
        } else {
            first.to_string()
        };
        let _ = writeln!(
            text,
            "{skip:>3}  {short:<50}  exit {:<3}  {}",
            e.exit,
            plural(e.output.lines().count(), "line")
        );
    }
    print(&text)
}

/// Write to stdout. A closed pipe (`cl -p | head -1`) is not an error.
fn print(text: &str) -> Result<()> {
    match io::stdout().lock().write_all(text.as_bytes()) {
        Err(e) if e.kind() != io::ErrorKind::BrokenPipe => Err(e.into()),
        _ => Ok(()),
    }
}

fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{n} {word}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sel() -> Selection {
        Selection {
            count: 1,
            skip: 0,
            commands_only: false,
            output_only: false,
            print: true,
            osc52: false,
        }
    }

    fn entries() -> Vec<Entry> {
        vec![
            Entry {
                command: "make".into(),
                output: "error: nope".into(),
                exit: 2,
                truncated: true,
                recorded: true,
            },
            Entry {
                command: "ls".into(),
                output: "a\nb".into(),
                exit: 0,
                truncated: false,
                recorded: true,
            },
            Entry::not_recorded(0),
        ]
    }

    #[test]
    fn renders_command_output_and_exit() {
        assert_eq!(
            render(&entries(), &sel()),
            "$ make\n[… earlier output truncated …]\nerror: nope\n[exit 2]\n\n$ ls\na\nb\n\n\
             [a command that was not recorded]"
        );
    }

    #[test]
    fn renders_commands_only() {
        let s = Selection {
            commands_only: true,
            ..sel()
        };
        assert_eq!(
            render(&entries(), &s),
            "$ make\n$ ls\n[a command that was not recorded]"
        );
    }

    #[test]
    fn renders_output_only() {
        let s = Selection {
            output_only: true,
            ..sel()
        };
        assert_eq!(
            render(&entries(), &s),
            "[… earlier output truncated …]\nerror: nope\n\na\nb\n\n[a command that was not recorded]"
        );
    }

    #[test]
    fn plurals() {
        assert_eq!(plural(1, "line"), "1 line");
        assert_eq!(plural(0, "line"), "0 lines");
        assert_eq!(plural(3, "command"), "3 commands");
    }
}
