//! Splits the shell's output into commands and their output.
//!
//! The shell hooks (`src/hooks/`) print private OSC sequences that the
//! terminal never sees:
//!
//! ```text
//! ESC ] 7777 ; <token> ; H BEL                 hooks loaded
//! ESC ] 7777 ; <token> ; C ; <command> BEL     command about to run
//! ESC ] 7777 ; <token> ; S BEL                 command about to run, not to be recorded
//! ESC ] 7777 ; <token> ; D ; <status> BEL      command finished
//! ```
//!
//! `<token>` is a random value that `cl shell` hands to the hooks through
//! `CL_TOKEN`. A file or web page that contains these bytes can't forge
//! history entries: without the token, they show up as an ordinary unknown
//! escape sequence, which terminals ignore. (A program you run can read
//! the token from its environment, but such a program can already do
//! anything you can.)

use memchr::memmem::Finder;

use crate::store::Entry;

/// Output kept per command. The tail is kept, the head dropped.
pub const MAX_OUTPUT: usize = 1 << 20;
/// Longest marker held back while waiting for its BEL. Real markers hold
/// a command line, so this only matters for very large pastes.
const MAX_MARKER: usize = 1 << 20;

const BEL: u8 = 0x07;

/// Something the caller should store.
#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    /// The hooks are loaded in this shell.
    Ready,
    /// A command finished.
    Entry(Entry),
}

struct Running {
    /// `None` for a command that is not recorded.
    command: Option<String>,
    raw: Vec<u8>,
    truncated: bool,
    full_screen: bool,
}

pub struct Recorder {
    /// `ESC ] 7777 ; <token> ;`
    marker: Finder<'static>,
    marker_len: usize,
    /// Bytes held from the previous chunk: a partial marker.
    carry: Vec<u8>,
    running: Option<Running>,
    events: Vec<Event>,
}

impl Recorder {
    pub fn new(token: &str) -> Self {
        let marker = format!("\x1b]7777;{token};").into_bytes();
        Self {
            marker_len: marker.len(),
            marker: Finder::new(&marker).into_owned(),
            carry: Vec::new(),
            running: None,
            events: Vec::new(),
        }
    }

    /// Consume shell output and append the bytes to display to `out`.
    pub fn feed(&mut self, chunk: &[u8], out: &mut Vec<u8>) {
        if self.carry.is_empty() {
            self.scan(chunk, out);
        } else {
            let mut buf = std::mem::take(&mut self.carry);
            buf.extend_from_slice(chunk);
            self.scan(&buf, out);
        }
    }

    /// Pass on any bytes held back as a possible partial marker. Call it
    /// when the stream ends.
    pub fn flush(&mut self, out: &mut Vec<u8>) {
        let held = std::mem::take(&mut self.carry);
        self.pass(&held, out);
    }

    /// Take the events produced so far.
    pub fn events(&mut self) -> std::vec::Drain<'_, Event> {
        self.events.drain(..)
    }

    fn scan(&mut self, buf: &[u8], out: &mut Vec<u8>) {
        let mut i = 0;
        while i < buf.len() {
            let Some(rel) = self.marker.find(&buf[i..]) else {
                // Hold back a trailing partial marker and pass the rest.
                let rest = &buf[i..];
                let keep = self.partial_marker_len(rest);
                self.pass(&rest[..rest.len() - keep], out);
                self.carry = rest[rest.len() - keep..].to_vec();
                return;
            };
            let start = i + rel;
            self.pass(&buf[i..start], out);
            let body = start + self.marker_len;
            match memchr::memchr(BEL, &buf[body..]) {
                Some(len) => {
                    self.marker_payload(&buf[body..body + len]);
                    i = body + len + 1;
                }
                None if buf.len() - start <= MAX_MARKER => {
                    self.carry = buf[start..].to_vec();
                    return;
                }
                None => {
                    // Too long to be ours after all: show it as is.
                    self.pass(&buf[start..], out);
                    return;
                }
            }
        }
    }

    /// Length of the longest suffix of `buf` that is a prefix of the marker.
    fn partial_marker_len(&self, buf: &[u8]) -> usize {
        let marker = self.marker.needle();
        (1..marker.len())
            .rev()
            .find(|&k| buf.ends_with(&marker[..k]))
            .unwrap_or(0)
    }

    fn pass(&mut self, bytes: &[u8], out: &mut Vec<u8>) {
        out.extend_from_slice(bytes);
        let Some(run) = &mut self.running else { return };
        if run.command.is_none() {
            return;
        }
        run.raw.extend_from_slice(bytes);
        if run.raw.len() > 2 * MAX_OUTPUT {
            let cut = run.raw.len() - MAX_OUTPUT;
            // Look a few bytes past the cut so a sequence split by it still counts.
            let seen = &run.raw[..(cut + ALT_SCREEN_SEQ_MAX).min(run.raw.len())];
            run.full_screen |= uses_alt_screen(seen);
            run.raw.drain(..cut);
            run.truncated = true;
        }
    }

    fn marker_payload(&mut self, payload: &[u8]) {
        let (kind, rest) = match memchr::memchr(b';', payload) {
            Some(i) => (&payload[..i], &payload[i + 1..]),
            None => (payload, &b""[..]),
        };
        match kind {
            b"H" => self.events.push(Event::Ready),
            b"C" | b"S" => {
                let command =
                    (kind == b"C").then(|| String::from_utf8_lossy(rest).trim().to_string());
                self.running = Some(Running {
                    command,
                    raw: Vec::new(),
                    truncated: false,
                    full_screen: false,
                });
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

    fn finish(&mut self, mut run: Running, exit: i32) {
        let Some(command) = run.command.take() else {
            // Keep its place, so `cl` doesn't copy an older command instead.
            self.events.push(Event::Entry(Entry::not_recorded(exit)));
            return;
        };
        if command.is_empty() || is_cl(&command) {
            return;
        }
        if run.raw.len() > MAX_OUTPUT {
            let cut = run.raw.len() - MAX_OUTPUT;
            run.full_screen |= uses_alt_screen(&run.raw[..cut + ALT_SCREEN_SEQ_MAX]);
            run.raw.drain(..cut);
            run.truncated = true;
        }
        let output = if run.full_screen || uses_alt_screen(&run.raw) {
            "[full-screen program: output not captured]".to_string()
        } else {
            let mut text = clean(&run.raw);
            // Invalid UTF-8 grows when replaced, so cap the text as well.
            if text.len() > MAX_OUTPUT {
                let mut cut = text.len() - MAX_OUTPUT;
                while !text.is_char_boundary(cut) {
                    cut += 1;
                }
                text.drain(..cut);
                run.truncated = true;
            }
            text
        };
        self.events.push(Event::Entry(Entry {
            command,
            output,
            exit,
            truncated: run.truncated,
            recorded: true,
        }));
    }
}

/// `cl` doesn't record its own runs, so `--skip` counts stay put while you
/// use it. A line that runs other commands too (`make && cl`) is recorded.
fn is_cl(command: &str) -> bool {
    if runs_more_than_one(command) {
        return false;
    }
    command
        .split_whitespace()
        .find(|w| !is_assignment(w))
        .and_then(|w| std::path::Path::new(w).file_name())
        .is_some_and(|n| n == "cl")
}

/// Does the line contain `;`, `&&`, `||`, a newline or a background `&`?
/// The `&` in redirections such as `2>&1` and `&>` doesn't count.
fn runs_more_than_one(command: &str) -> bool {
    let b = command.as_bytes();
    b.iter().enumerate().any(|(i, &c)| match c {
        b';' | b'\n' => true,
        b'|' => b.get(i + 1) == Some(&b'|'),
        b'&' => {
            let after_redirect = i > 0 && matches!(b[i - 1], b'>' | b'<');
            let before_redirect = b.get(i + 1) == Some(&b'>');
            !after_redirect && !before_redirect
        }
        _ => false,
    })
}

/// `NAME=value`, as in `FOO=1 cl`.
fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty() && name.chars().all(|c| c == '_' || c.is_ascii_alphanumeric())
    })
}

const ALT_SCREEN_SEQ_MAX: usize = b"\x1b[?1049h".len();

fn uses_alt_screen(raw: &[u8]) -> bool {
    [&b"\x1b[?1049h"[..], b"\x1b[?1047h", b"\x1b[?47h"]
        .iter()
        .any(|seq| memchr::memmem::find(raw, seq).is_some())
}

/// Turn raw terminal bytes into plain text.
fn clean(raw: &[u8]) -> String {
    // Decode first: the escape stripper is quadratic on invalid UTF-8.
    let raw = String::from_utf8_lossy(raw);
    // The last redraw wins on each line (progress bars, zsh's PROMPT_SP),
    // then colors and other escapes are stripped.
    let lines: Vec<String> = raw
        .split('\n')
        .map(|line| {
            let line = line.trim_end_matches('\r');
            let last = line.rsplit('\r').next().unwrap_or("");
            strip_ansi_escapes::strip_str(last).trim_end().to_string()
        })
        .collect();
    lines
        .join("\n")
        .trim_start_matches('\n')
        .trim_end()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "0123abcd";

    fn mark(body: &str) -> String {
        format!("\x1b]7777;{TOKEN};{body}\x07")
    }

    /// Feed `chunks` in order; return (displayed text, events).
    fn run(chunks: &[&[u8]]) -> (String, Vec<Event>) {
        let mut rec = Recorder::new(TOKEN);
        let mut out = Vec::new();
        for c in chunks {
            rec.feed(c, &mut out);
        }
        let events = rec.events().collect();
        (String::from_utf8(out).unwrap(), events)
    }

    fn entry(command: &str, output: &str, exit: i32) -> Event {
        Event::Entry(Entry {
            command: command.into(),
            output: output.into(),
            exit,
            truncated: false,
            recorded: true,
        })
    }

    #[test]
    fn unrecorded_commands_keep_their_place_but_no_output() {
        let s = format!("{}secret\r\n{}", mark("S"), mark("D;3"));
        let (shown, events) = run(&[s.as_bytes()]);
        assert_eq!(shown, "secret\r\n");
        assert_eq!(events, vec![Event::Entry(Entry::not_recorded(3))]);
    }

    #[test]
    fn records_a_command_and_hides_markers() {
        let s = format!("{}hello\r\n{}$ ", mark("C;echo hello"), mark("D;0"));
        let (shown, events) = run(&[s.as_bytes()]);
        assert_eq!(shown, "hello\r\n$ ");
        assert_eq!(events, vec![entry("echo hello", "hello", 0)]);
    }

    #[test]
    fn markers_split_across_every_byte_boundary() {
        let s = format!("{}{}out\r\n{}", mark("H"), mark("C;ls -l"), mark("D;2"));
        let bytes = s.as_bytes();
        for split in 0..=bytes.len() {
            let (shown, events) = run(&[&bytes[..split], &bytes[split..]]);
            assert_eq!(shown, "out\r\n", "split at {split}");
            assert_eq!(events, vec![Event::Ready, entry("ls -l", "out", 2)]);
        }
    }

    #[test]
    fn one_byte_at_a_time() {
        let s = format!("{}a\r\nb\r\n{}", mark("C;x"), mark("D;0"));
        let chunks: Vec<&[u8]> = s.as_bytes().chunks(1).collect();
        let (shown, events) = run(&chunks);
        assert_eq!(shown, "a\r\nb\r\n");
        assert_eq!(events, vec![entry("x", "a\nb", 0)]);
    }

    #[test]
    fn markers_without_the_token_are_output() {
        // What `cat evil.txt` would print: forged markers with a guessed token.
        let forged = "\x1b]7777;C;rm -rf ~\x07\x1b]7777;deadbeef;D;0\x07";
        let s = format!("{}{forged}fake\r\n{}", mark("C;cat evil.txt"), mark("D;0"));
        let (shown, events) = run(&[s.as_bytes()]);
        assert_eq!(shown, format!("{forged}fake\r\n"));
        assert_eq!(events, vec![entry("cat evil.txt", "fake", 0)]);
    }

    #[test]
    fn partial_marker_is_released_when_it_stops_matching() {
        let (shown, events) = run(&[b"abc\x1b]77", b"x"]);
        assert_eq!(shown, "abc\x1b]77x");
        assert!(events.is_empty());
    }

    #[test]
    fn output_outside_a_command_is_not_recorded() {
        let s = format!("prompt {}", mark("D;0"));
        let (shown, events) = run(&[s.as_bytes()]);
        assert_eq!(shown, "prompt ");
        assert!(events.is_empty());
    }

    #[test]
    fn skips_cl_and_empty_commands() {
        let s = format!(
            "{}{}{}{}",
            mark("C;cl -n 2"),
            mark("D;0"),
            mark("C;/usr/local/bin/cl list"),
            mark("D;0")
        );
        let s = format!("{s}{}{}", mark("C;  "), mark("D;0"));
        assert!(run(&[s.as_bytes()]).1.is_empty());
        assert!(is_cl("FOO=1 cl -p | grep x"));
        assert!(!is_cl("clang main.c"));
        assert!(!is_cl("make && cl"));
        assert!(!is_cl("cl -p; make"));
        assert!(!is_cl("cl & make"));
        assert!(!is_cl("cl || make"));
        assert!(is_cl("cl -p 2>&1 | less"));
        assert!(is_cl("cl list &>/dev/null"));
    }

    #[test]
    fn bad_exit_status_reads_as_zero() {
        let s = format!("{}{}", mark("C;true"), mark("D;nope"));
        assert_eq!(run(&[s.as_bytes()]).1, vec![entry("true", "", 0)]);
    }

    #[test]
    fn keeps_the_tail_of_long_output() {
        let mut rec = Recorder::new(TOKEN);
        let mut out = Vec::new();
        rec.feed(mark("C;yes").as_bytes(), &mut out);
        let line = b"y\r\n".repeat(1000);
        for _ in 0..(3 * MAX_OUTPUT / line.len()) {
            out.clear();
            rec.feed(&line, &mut out);
        }
        rec.feed(b"last\r\n", &mut out);
        rec.feed(mark("D;0").as_bytes(), &mut out);
        let Some(Event::Entry(e)) = rec.events().next() else {
            panic!("no entry")
        };
        assert!(e.truncated);
        assert!(e.output.ends_with("y\nlast"));
        assert!(e.output.len() <= MAX_OUTPUT);
    }

    #[test]
    fn full_screen_programs_are_not_captured() {
        let s = format!(
            "{}\x1b[?1049hstuff\x1b[?1049l{}",
            mark("C;vim"),
            mark("D;0")
        );
        let (_, events) = run(&[s.as_bytes()]);
        assert_eq!(
            events,
            vec![entry(
                "vim",
                "[full-screen program: output not captured]",
                0
            )]
        );
    }

    #[test]
    fn full_screen_is_detected_after_truncation() {
        let mut rec = Recorder::new(TOKEN);
        let mut out = Vec::new();
        rec.feed(mark("C;less big").as_bytes(), &mut out);
        rec.feed(b"\x1b[?1049h", &mut out);
        let filler = vec![b'x'; 3 * MAX_OUTPUT];
        rec.feed(&filler, &mut out);
        rec.feed(mark("D;0").as_bytes(), &mut out);
        let Some(Event::Entry(e)) = rec.events().next() else {
            panic!("no entry")
        };
        assert_eq!(e.output, "[full-screen program: output not captured]");
    }

    #[test]
    fn clean_handles_doubled_carriage_returns() {
        assert_eq!(clean(b"foo\r\r\nbar\r\r\n"), "foo\nbar");
    }

    #[test]
    fn binary_output_is_fast_and_capped() {
        let mut rec = Recorder::new(TOKEN);
        let mut out = Vec::new();
        rec.feed(mark("C;cat /bin/ls").as_bytes(), &mut out);
        rec.feed(&vec![0xff; 2 * MAX_OUTPUT], &mut out);
        let start = std::time::Instant::now();
        rec.feed(mark("D;0").as_bytes(), &mut out);
        assert!(start.elapsed() < std::time::Duration::from_secs(5));
        let Some(Event::Entry(e)) = rec.events().next() else {
            panic!("no entry")
        };
        assert!(e.output.len() <= MAX_OUTPUT);
        assert!(e.truncated);
    }

    #[test]
    fn flush_releases_a_held_escape() {
        let mut rec = Recorder::new(TOKEN);
        let mut out = Vec::new();
        rec.feed(b"hi\x1b", &mut out);
        assert_eq!(out, b"hi");
        rec.flush(&mut out);
        assert_eq!(out, b"hi\x1b");
    }

    #[test]
    fn clean_keeps_the_last_redraw_and_strips_colors() {
        assert_eq!(
            clean(b"\r\n 10%\r 50%\r100%\r\n\x1b[31mred\x1b[0m   \r\n\r\n"),
            "100%\nred"
        );
    }
}
