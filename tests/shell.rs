//! End-to-end tests: run `cl shell` in a PTY with a real zsh or bash.
//!
//! A test is skipped (and says so) when its shell isn't installed. bash
//! tests need bash 4.4 or later, which macOS doesn't ship. In CI (`CI` is
//! set) a missing shell fails the test instead, so nothing passes unrun.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, Child, CommandBuilder, PtySize};

const PROMPT: &str = "READY> ";
const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy)]
enum Shell {
    Zsh,
    Bash,
}

impl Shell {
    fn path(self) -> Option<PathBuf> {
        let name = match self {
            Self::Zsh => "zsh",
            Self::Bash => "bash",
        };
        let out = Command::new("sh")
            .args(["-c", &format!("command -v {name}")])
            .output()
            .ok()?;
        let path = PathBuf::from(String::from_utf8(out.stdout).ok()?.trim());
        if !path.is_absolute() {
            return None;
        }
        if let Self::Bash = self {
            let v = Command::new(&path)
                .args(["-c", "echo $((BASH_VERSINFO[0] * 100 + BASH_VERSINFO[1]))"])
                .output()
                .ok()?;
            let v: u32 = String::from_utf8(v.stdout).ok()?.trim().parse().ok()?;
            if v < 404 {
                return None;
            }
        }
        Some(path)
    }

    fn rc_file(self, home: &Path) -> PathBuf {
        match self {
            Self::Zsh => home.join(".zshrc"),
            Self::Bash => home.join(".bashrc"),
        }
    }

    /// rc lines that keep the screen free of prompt decorations.
    fn quiet(self) -> &'static str {
        match self {
            Self::Zsh => "unsetopt PROMPT_SP\nunset zle_bracketed_paste\n",
            Self::Bash => "bind 'set enable-bracketed-paste off' 2>/dev/null\n",
        }
    }

    fn hooks(self) -> &'static str {
        match self {
            Self::Zsh => r#"eval "$(cl init zsh)""#,
            Self::Bash => r#"eval "$(cl init bash)""#,
        }
    }
}

struct Term {
    home: PathBuf,
    child: Box<dyn Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    screen: Arc<Mutex<Vec<u8>>>,
}

impl Term {
    /// Start `cl shell` with `rc` as the shell's rc file. `None` if the
    /// shell isn't available.
    fn start(shell: Shell, rc: &str) -> Option<Self> {
        Self::start_with(shell, rc, |_| {})
    }

    fn start_with(shell: Shell, rc: &str, setup: impl FnOnce(&Path)) -> Option<Self> {
        let Some(shell_path) = shell.path() else {
            assert!(
                std::env::var_os("CI").is_none(),
                "the shell for this test is missing (bash must be 4.4 or later)"
            );
            eprintln!("skipped: shell not available");
            return None;
        };
        let home = std::env::temp_dir().join(format!(
            "cl-e2e-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).unwrap();
        fs::write(
            shell.rc_file(&home),
            format!(
                "{}PS1='{PROMPT}'\nunset PROMPT_COMMAND\n{rc}\n",
                shell.quiet()
            ),
        )
        .unwrap();
        setup(&home);

        let cl = PathBuf::from(env!("CARGO_BIN_EXE_cl"));
        let path = format!(
            "{}:/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin",
            cl.parent().unwrap().display()
        );
        let pty = native_pty_system()
            .openpty(PtySize {
                rows: 40,
                cols: 200,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut cmd = CommandBuilder::new(&cl);
        cmd.arg("shell");
        cmd.env_clear();
        cmd.env("HOME", &home);
        cmd.env("ZDOTDIR", &home);
        cmd.env("XDG_STATE_HOME", home.join("state"));
        cmd.env("SHELL", &shell_path);
        cmd.env("PATH", path);
        cmd.env("TERM", "xterm");
        cmd.cwd(&home);
        let child = pty.slave.spawn_command(cmd).unwrap();
        drop(pty.slave);

        let screen = Arc::new(Mutex::new(Vec::new()));
        let mut reader = pty.master.try_clone_reader().unwrap();
        let sink = Arc::clone(&screen);
        thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                sink.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        });
        let writer = pty.master.take_writer().unwrap();
        // Keep the master open for the life of the test.
        std::mem::forget(pty.master);
        let term = Self {
            home,
            child,
            writer,
            screen,
        };
        term.wait_for_prompts(1);
        Some(term)
    }

    /// Everything shown so far, without escape sequences or `\r`.
    fn screen(&self) -> String {
        let raw = strip_ansi_escapes::strip(&*self.screen.lock().unwrap());
        String::from_utf8_lossy(&raw).replace('\r', "")
    }

    fn prompts(&self) -> usize {
        self.screen().matches(PROMPT).count()
    }

    fn wait_for_prompts(&self, n: usize) {
        let start = Instant::now();
        while self.prompts() < n {
            assert!(
                start.elapsed() < TIMEOUT,
                "timed out waiting for prompt {n}; screen:\n{}",
                self.screen()
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Type a command and wait for the next prompt. Returns what it printed.
    fn run(&mut self, line: &str) -> String {
        let before = self.screen().len();
        let n = self.prompts();
        self.writer
            .write_all(format!("{line}\r").as_bytes())
            .unwrap();
        self.writer.flush().unwrap();
        self.wait_for_prompts(n + 1);
        let screen = self.screen();
        let new = &screen[before..];
        // Drop the echoed command line and the trailing prompt.
        let new = new.split_once('\n').map_or("", |(_, rest)| rest);
        new.strip_suffix(PROMPT).unwrap_or(new).to_string()
    }

    fn state_files(&self) -> Vec<String> {
        fs::read_dir(self.home.join("state/cl"))
            .map(|d| {
                d.map(|e| e.unwrap().file_name().into_string().unwrap())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Wait for `cl shell` to exit; `None` if it doesn't within `limit`.
    fn wait_exit(&mut self, limit: Duration) -> Option<u32> {
        let start = Instant::now();
        while start.elapsed() < limit {
            if let Some(status) = self.child.try_wait().unwrap() {
                return Some(status.exit_code());
            }
            thread::sleep(Duration::from_millis(20));
        }
        None
    }
}

impl Drop for Term {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = fs::remove_dir_all(&self.home);
    }
}

static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn both(test: impl Fn(Shell)) {
    test(Shell::Zsh);
    test(Shell::Bash);
}

#[test]
fn records_and_copies_commands() {
    both(|shell| {
        let Some(mut t) = Term::start(shell, shell.hooks()) else {
            return;
        };
        t.run("echo hello");
        t.run("false");
        assert_eq!(
            t.run("cl -p -n 2"),
            "$ echo hello\nhello\n\n$ false\n[exit 1]\n"
        );
        assert_eq!(t.run("cl -p -s 1"), "$ echo hello\nhello\n");
        let list = t.run("cl list");
        assert!(list.starts_with("  0  false"), "{list}");
        assert!(list.contains("  1  echo hello"), "{list}");
    });
}

#[test]
fn forged_markers_are_ignored() {
    both(|shell| {
        let Some(mut t) = Term::start(shell, shell.hooks()) else {
            return;
        };
        fs::write(
            t.home.join("evil.txt"),
            "\x1b]7777;D;0\x07\x1b]7777;C;rm -rf ~\x07\x1b]7777;0000;D;0\x07fake\n",
        )
        .unwrap();
        t.run("cat evil.txt");
        let list = t.run("cl list");
        assert!(list.starts_with("  0  cat evil.txt"), "{list}");
        assert!(!list.contains("rm -rf"), "{list}");
        assert_eq!(t.run("cl -p -o"), "fake\n");
    });
}

#[test]
fn leading_space_is_not_recorded() {
    both(|shell| {
        let Some(mut t) = Term::start(shell, shell.hooks()) else {
            return;
        };
        t.run("echo public");
        t.run(" echo secret");
        assert!(t.run("cl -p").contains("was not recorded"));
        assert_eq!(t.run("cl -p -s 1"), "$ echo public\npublic\n");
        let list = t.run("cl list");
        assert!(list.starts_with("  0  [not recorded]"), "{list}");
        assert!(!list.contains("secret"), "{list}");
    });
}

/// The command column of `cl list`.
fn listed(list: &str) -> Vec<&str> {
    list.lines()
        .map(|l| l[5..].split("  ").next().unwrap())
        .collect()
}

#[test]
fn bash_ignoreboth_hides_what_bash_hides() {
    let rc = format!("HISTCONTROL=ignoreboth\n{}", Shell::Bash.hooks());
    let Some(mut t) = Term::start(Shell::Bash, &rc) else {
        return;
    };
    t.run("echo one");
    t.run("echo one");
    t.run(" echo secret");
    t.run("echo two");
    // A repeat and a space-led line look the same to cl here, so neither is
    // recorded, but both keep their place.
    let list = t.run("cl list");
    assert_eq!(
        listed(&list),
        ["echo two", "[not recorded]", "[not recorded]", "echo one"],
        "{list}"
    );
    assert!(!t.run("history").contains("secret"));
}

#[test]
fn bash_ignoredups_alone_records_repeats() {
    let rc = format!("HISTCONTROL=ignoredups\n{}", Shell::Bash.hooks());
    let Some(mut t) = Term::start(Shell::Bash, &rc) else {
        return;
    };
    t.run("echo one");
    t.run("echo one");
    let list = t.run("cl list");
    assert_eq!(listed(&list), ["echo one", "echo one"], "{list}");
}

#[test]
fn bash_space_commands_never_reach_the_history_file() {
    let rc = format!("HISTCONTROL=ignorespace\n{}", Shell::Bash.hooks());
    let Some(mut t) = Term::start(Shell::Bash, &rc) else {
        return;
    };
    t.run("echo kept");
    t.writer.write_all(b" echo SECRET; exit\r").unwrap();
    assert!(t.wait_exit(TIMEOUT).is_some());
    let saved = fs::read_to_string(t.home.join(".bash_history")).unwrap();
    assert!(saved.contains("echo kept"), "{saved}");
    assert!(!saved.contains("SECRET"), "{saved}");
}

#[test]
fn exec_shell_keeps_recording() {
    both(|shell| {
        let Some(mut t) = Term::start(shell, shell.hooks()) else {
            return;
        };
        t.run("echo before");
        let exe = match shell {
            Shell::Zsh => "exec zsh",
            Shell::Bash => "exec bash",
        };
        t.run(exe);
        t.run("echo after");
        assert_eq!(t.run("cl -p"), "$ echo after\nafter\n");
    });
}

#[test]
fn bash_shared_history_never_mislabels_output() {
    let rc = format!(
        "HISTCONTROL=ignorespace\n{}\nPROMPT_COMMAND=\"$PROMPT_COMMAND;history -a;history -n\"",
        Shell::Bash.hooks()
    );
    let Some(mut t) = Term::start(Shell::Bash, &rc) else {
        return;
    };
    t.run("echo kept");
    t.run("echo 'echo FROM_OTHER_TERMINAL' >> ~/.bash_history");
    t.run(" echo SECRET_OUTPUT");
    t.run("echo last");
    let all = t.run("cl -p -n 10");
    assert!(!all.contains("SECRET_OUTPUT"), "{all}");
    assert!(all.ends_with("$ echo last\nlast\n"), "{all}");
}

#[test]
fn bash_array_prompt_command_keeps_the_snapshot_last() {
    let rc = format!(
        "HISTCONTROL=ignorespace\n{}\nPROMPT_COMMAND+=('history -a; history -n')",
        Shell::Bash.hooks()
    );
    let Some(mut t) = Term::start(Shell::Bash, &rc) else {
        return;
    };
    t.run("echo kept");
    t.run("echo 'echo OTHER' >> ~/.bash_history");
    t.run(" echo SECRET_OUTPUT");
    assert!(!t.run("cl -p -n 10").contains("SECRET_OUTPUT"));
    let last = t.run("echo \"${PROMPT_COMMAND[-1]}\"");
    assert_eq!(last, "_cl_snap\n");
}

#[test]
fn bash_exit_status_survives_a_later_prompt_command() {
    let rc = format!(
        "{}\nPROMPT_COMMAND=\"true;$PROMPT_COMMAND\"",
        Shell::Bash.hooks()
    );
    let Some(mut t) = Term::start(Shell::Bash, &rc) else {
        return;
    };
    t.run("false");
    t.run("false");
    assert!(t.run("cl -p").contains("[exit 1]"));
}

#[test]
fn says_when_hooks_are_missing() {
    let Some(mut t) = Term::start(Shell::Zsh, "") else {
        return;
    };
    t.run("echo hi");
    assert!(t.run("cl -p").contains("does not report commands to cl"));
}

#[test]
fn rejects_zero_count() {
    let Some(mut t) = Term::start(Shell::Zsh, Shell::Zsh.hooks()) else {
        return;
    };
    assert!(t.run("cl -n 0").contains("invalid value '0'"));
}

#[test]
fn removes_its_files_when_the_terminal_closes() {
    let Some(mut t) = Term::start(Shell::Zsh, Shell::Zsh.hooks()) else {
        return;
    };
    t.run("echo SECRET_TOKEN=abc123");
    assert!(t
        .state_files()
        .iter()
        .any(|f| Path::new(f).extension().is_some_and(|e| e == "jsonl")));
    let pid = t.child.process_id().unwrap();
    unsafe { libc::kill(libc::pid_t::try_from(pid).unwrap(), libc::SIGHUP) };
    assert!(t.wait_exit(TIMEOUT).is_some());
    assert_eq!(t.state_files(), Vec::<String>::new());
}

#[test]
fn exits_when_a_background_job_holds_the_terminal() {
    both(|shell| {
        let Some(mut t) = Term::start(shell, shell.hooks()) else {
            return;
        };
        t.run("sleep 30 & disown");
        t.writer.write_all(b"exit 3\r").unwrap();
        assert_eq!(t.wait_exit(Duration::from_secs(5)), Some(3));
        assert_eq!(t.state_files(), Vec::<String>::new());
    });
}

#[test]
fn exits_when_a_background_job_keeps_printing() {
    both(|shell| {
        let Some(mut t) = Term::start(shell, shell.hooks()) else {
            return;
        };
        t.run("(while :; do echo tick; sleep 0.05; done) & disown");
        t.writer.write_all(b"exit 4\r").unwrap();
        assert_eq!(t.wait_exit(Duration::from_secs(5)), Some(4));
    });
}

#[test]
fn starts_the_shell_anyway_if_recording_fails() {
    let rc = Shell::Zsh.hooks();
    let Some(mut t) = Term::start_with(Shell::Zsh, rc, |home| {
        let dir = home.join("state/cl");
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    }) else {
        return;
    };
    assert!(t.screen().contains("without recording"), "{}", t.screen());
    assert_eq!(t.run("echo still here"), "still here\n");
    assert!(t.run("cl -p").contains("could not start recording"));
}

/// The always-record line from the README.
const RECORD_EVERY_TERMINAL: &str =
    "[[ -z ${CL_SESSION+x} && -t 0 ]] && command -v cl >/dev/null && exec cl shell";

#[test]
fn readme_rc_line_does_not_loop_when_recording_fails() {
    both(|shell| {
        let rc = format!("{}\n{RECORD_EVERY_TERMINAL}", shell.hooks());
        let Some(mut t) = Term::start_with(shell, &rc, |home| {
            fs::create_dir_all(home.join("state/cl")).unwrap();
            fs::set_permissions(home.join("state/cl"), fs::Permissions::from_mode(0o755)).unwrap();
        }) else {
            return;
        };
        assert_eq!(t.run("echo still here"), "still here\n");
        assert_eq!(t.screen().matches("without recording").count(), 1);
    });
}

#[test]
fn reports_a_signal_that_killed_the_shell() {
    let Some(mut t) = Term::start(Shell::Zsh, Shell::Zsh.hooks()) else {
        return;
    };
    t.writer.write_all(b"kill -KILL $$\r").unwrap();
    assert_eq!(t.wait_exit(TIMEOUT), Some(128 + 9));
}

#[test]
fn closed_pipe_is_not_an_error() {
    let Some(mut t) = Term::start(Shell::Zsh, Shell::Zsh.hooks()) else {
        return;
    };
    t.run("echo a");
    t.run("echo b");
    let out = t.run("cl list | head -1; echo status=${pipestatus[1]}");
    assert!(out.ends_with("status=0\n"), "{out}");
    assert!(!out.contains("panicked"), "{out}");
}

#[test]
fn bash_histignore_hides_matches_without_globbing() {
    let rc = format!(
        "HISTCONTROL=ignoredups\nHISTIGNORE='ls*:pwd'\n{}",
        Shell::Bash.hooks()
    );
    let Some(mut t) = Term::start_with(Shell::Bash, &rc, |home| {
        fs::write(home.join("lsfoo"), "").unwrap();
    }) else {
        return;
    };
    t.run("echo one");
    t.run("pwd");
    t.run("ls -a");
    let list = t.run("cl list");
    assert_eq!(
        listed(&list),
        ["[not recorded]", "[not recorded]", "echo one"],
        "{list}"
    );
    let history = t.run("history");
    assert!(
        !history.contains("pwd") && !history.contains("ls -a"),
        "{history}"
    );
}

#[test]
fn bash_loading_the_hooks_twice_keeps_one_prompt_hook() {
    let rc = format!(
        "{0}\nPROMPT_COMMAND=\"true;$PROMPT_COMMAND\"\n{0}",
        Shell::Bash.hooks()
    );
    let Some(mut t) = Term::start(Shell::Bash, &rc) else {
        return;
    };
    assert_eq!(
        t.run("echo \"$PROMPT_COMMAND\""),
        "_cl_precmd\ntrue\n_cl_snap\n"
    );
    t.run("false");
    assert!(t.run("cl -p").contains("[exit 1]"));
}

#[test]
fn bash_multi_line_commands() {
    for (rc, expected) in [
        ("", "$ for i in 1 2; do echo $i; done\n"),
        ("shopt -s lithist", "$ for i in 1 2\ndo echo $i\ndone\n"),
    ] {
        let rc = format!("{rc}\n{}", Shell::Bash.hooks());
        let Some(mut t) = Term::start(Shell::Bash, &rc) else {
            return;
        };
        t.run("for i in 1 2\rdo echo $i\rdone");
        assert_eq!(t.run("cl -p -c"), expected);
    }
}

#[test]
fn zsh_multi_line_commands_keep_their_lines() {
    let Some(mut t) = Term::start(Shell::Zsh, Shell::Zsh.hooks()) else {
        return;
    };
    t.run("for i in 1 2\rdo echo $i\rdone");
    assert_eq!(t.run("cl -p -c"), "$ for i in 1 2\ndo echo $i\ndone\n");
}
