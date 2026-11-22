//! Where history lives on disk.
//!
//! Each `cl shell` session owns three files in `$XDG_STATE_HOME/cl`
//! (default `~/.local/state/cl`), named by a random session ID:
//!
//! - `<id>.lock`: held with `flock` for the life of the session. A lock
//!   file that nobody holds belongs to a session that died without
//!   cleaning up, so the next `cl shell` deletes its files.
//! - `<id>.jsonl`: one JSON entry per line, appended as commands finish.
//!   It is created when the hooks first report in, so a missing file means
//!   the shell integration isn't loaded.
//! - `<id>.tmp`: used briefly while compacting the history.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// Commands kept per session.
pub const HISTORY_LIMIT: usize = 100;
/// Compact the file once it holds this many entries.
const COMPACT_AT: usize = HISTORY_LIMIT + HISTORY_LIMIT / 2;

/// Path of the session's history file, set inside `cl shell`.
pub const ENV_SESSION: &str = "CL_SESSION";
/// The session's marker token, set inside `cl shell`.
pub const ENV_TOKEN: &str = "CL_TOKEN";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub command: String,
    pub output: String,
    pub exit: i32,
    #[serde(default)]
    pub truncated: bool,
    /// False for a command that ran but was kept out of the record: it
    /// started with a space, or bash kept it out of its history.
    #[serde(default = "yes")]
    pub recorded: bool,
}

const fn yes() -> bool {
    true
}

impl Entry {
    pub const fn not_recorded(exit: i32) -> Self {
        Self {
            command: String::new(),
            output: String::new(),
            exit,
            truncated: false,
            recorded: false,
        }
    }
}

/// The state directory, created with mode 0700 if needed.
pub fn state_dir() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .context("HOME is not set")?;
    let dir = base.join("cl");
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .with_context(|| format!("cannot create {}", dir.display()))?;
    check_private(&dir)?;
    Ok(dir)
}

/// Refuse a state directory that other users could read or swap out.
fn check_private(dir: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(dir)?;
    // SAFETY: getuid has no preconditions and can't fail.
    let uid = unsafe { libc::getuid() };
    if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o077 != 0 {
        bail!(
            "refusing to use {0}: it must be a directory you own that only you can access \
             (fix with `chmod 700 {0}`)",
            dir.display()
        );
    }
    Ok(())
}

/// The writing side of a session, owned by `cl shell`.
pub struct Session {
    history: PathBuf,
    lock_path: PathBuf,
    _lock: File,
    file: Option<File>,
    entries: usize,
}

impl Session {
    /// Start a new session in `dir`, first removing files left by dead ones.
    pub fn create(dir: &Path) -> Result<Self> {
        sweep(dir);
        let id = random_hex(8)?;
        // Lock the file under a name `sweep` ignores, then rename it, so no
        // other `cl shell` can see it unlocked and delete it.
        let new_path = dir.join(format!("{id}.new"));
        let lock_path = dir.join(format!("{id}.lock"));
        let lock = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&new_path)
            .with_context(|| format!("cannot create {}", new_path.display()))?;
        if !try_lock(&lock) {
            let _ = fs::remove_file(&new_path);
            bail!("cannot lock {}", new_path.display());
        }
        fs::rename(&new_path, &lock_path)
            .with_context(|| format!("cannot create {}", lock_path.display()))?;
        Ok(Self {
            history: dir.join(format!("{id}.jsonl")),
            lock_path,
            _lock: lock,
            file: None,
            entries: 0,
        })
    }

    pub fn history_path(&self) -> &Path {
        &self.history
    }

    /// Create the history file, so `cl` knows the hooks are loaded.
    pub fn mark_ready(&mut self) -> io::Result<()> {
        self.file().map(|_| ())
    }

    pub fn append(&mut self, entry: &Entry) -> io::Result<()> {
        let mut line = serde_json::to_vec(entry)?;
        line.push(b'\n');
        // One write per entry: with O_APPEND, a reader sees whole lines.
        self.file()?.write_all(&line)?;
        self.entries += 1;
        if self.entries >= COMPACT_AT {
            self.compact()?;
        }
        Ok(())
    }

    /// Delete the session's files.
    pub fn remove(&mut self) {
        self.file = None;
        let _ = fs::remove_file(&self.history);
        let _ = fs::remove_file(self.history.with_extension("tmp"));
        let _ = fs::remove_file(&self.lock_path);
    }

    fn file(&mut self) -> io::Result<&mut File> {
        if self.file.is_none() {
            self.file = Some(open_private(&self.history, false)?);
        }
        Ok(self.file.as_mut().expect("just opened"))
    }

    /// Rewrite the file with only the newest `HISTORY_LIMIT` entries.
    fn compact(&mut self) -> io::Result<()> {
        let keep = tail_lines(&self.history, HISTORY_LIMIT)?;
        let tmp = self.history.with_extension("tmp");
        let mut f = io::BufWriter::new(open_private(&tmp, true)?);
        for line in &keep {
            f.write_all(line)?;
            f.write_all(b"\n")?;
        }
        f.into_inner().map_err(io::IntoInnerError::into_error)?;
        fs::rename(&tmp, &self.history)?;
        self.file = None;
        self.entries = keep.len();
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.remove();
    }
}

/// Open a file only the user can read, refusing to follow a symlink.
fn open_private(path: &Path, truncate: bool) -> io::Result<File> {
    let mut opts = OpenOptions::new();
    if truncate {
        opts.write(true).truncate(true);
    } else {
        opts.append(true);
    }
    opts.create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

/// A `.new` file older than this belongs to a session that died while starting.
const NEW_LOCK_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(60);

/// Delete the files of sessions whose lock nobody holds.
fn sweep(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let paths: Vec<PathBuf> = entries.filter_map(|e| Some(e.ok()?.path())).collect();
    let live: HashSet<&std::ffi::OsStr> = paths
        .iter()
        .filter(|p| p.extension().is_some_and(|e| e == "lock"))
        .filter(|p| File::open(p).is_ok_and(|f| !try_lock(&f)))
        .filter_map(|p| p.file_stem())
        .collect();
    for path in &paths {
        if path.extension().is_some_and(|e| e == "new") {
            let old = fs::metadata(path)
                .and_then(|m| m.modified())
                .is_ok_and(|t| t.elapsed().unwrap_or_default() > NEW_LOCK_MAX_AGE);
            if old {
                let _ = fs::remove_file(path);
            }
            continue;
        }
        let ours = path
            .extension()
            .is_some_and(|e| e == "lock" || e == "jsonl" || e == "tmp");
        if ours && path.file_stem().is_some_and(|s| !live.contains(s)) {
            let _ = fs::remove_file(path);
        }
    }
}

/// Take an exclusive lock without waiting. The lock lasts until `file` is
/// closed, which the kernel does for us if the process dies.
fn try_lock(file: &File) -> bool {
    // SAFETY: the descriptor is valid for as long as `file` is borrowed.
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

/// `bytes` random bytes from the kernel, as lowercase hex.
pub fn random_hex(bytes: usize) -> Result<String> {
    let mut buf = vec![0u8; bytes];
    File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .context("cannot read /dev/urandom")?;
    Ok(buf.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    }))
}

/// The last `n` entries of the current session (at most `HISTORY_LIMIT`),
/// oldest first.
pub fn recent(n: usize) -> Result<Vec<Entry>> {
    let n = n.min(HISTORY_LIMIT);
    let path =
        std::env::var_os(ENV_SESSION).context("not inside a cl session. Run `cl shell` first.")?;
    if path.is_empty() {
        bail!("`cl shell` could not start recording in this terminal. Run `cl shell` to see why.");
    }
    match tail_lines(Path::new(&path), n) {
        Ok(lines) => Ok(lines
            .iter()
            .filter_map(|l| serde_json::from_slice(l).ok())
            .collect()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => bail!(
            "this shell does not report commands to cl\n\
             Add the hook to your rc file, then start a new `cl shell`:\n  \
             eval \"$(cl init zsh)\"   # or: eval \"$(cl init bash)\""
        ),
        Err(e) => Err(e.into()),
    }
}

/// The last `n` non-empty lines of a file, without their newlines.
/// Reads backwards from the end, so the cost depends on `n`, not the file.
fn tail_lines(path: &Path, n: usize) -> io::Result<Vec<Vec<u8>>> {
    let mut f = File::open(path)?;
    if n == 0 {
        return Ok(Vec::new());
    }
    let mut pos = f.seek(SeekFrom::End(0))?;
    let mut buf = Vec::new();
    // Stop once the buffer holds n whole non-empty lines after its first
    // (possibly partial) line.
    let whole_lines = |buf: &[u8]| {
        buf.split(|&b| b == b'\n')
            .skip(1)
            .filter(|l| !l.is_empty())
            .count()
    };
    while pos > 0 && whole_lines(&buf) <= n {
        let step = (buf.len() as u64).max(64 * 1024).min(pos);
        pos -= step;
        let mut chunk = vec![0; usize::try_from(step).expect("step fits in memory")];
        f.seek(SeekFrom::Start(pos))?;
        f.read_exact(&mut chunk)?;
        chunk.extend_from_slice(&buf);
        buf = chunk;
    }
    let mut lines: Vec<&[u8]> = buf.split(|&b| b == b'\n').collect();
    if pos > 0 {
        lines.remove(0); // may start mid-line
    }
    lines.retain(|l| !l.is_empty());
    let skip = lines.len().saturating_sub(n);
    Ok(lines[skip..].iter().map(|l| l.to_vec()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cl-test-{}", random_hex(6).unwrap()));
        fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        dir
    }

    fn entry(i: usize) -> Entry {
        Entry {
            command: format!("echo {i}"),
            output: i.to_string(),
            exit: 0,
            truncated: false,
            recorded: true,
        }
    }

    fn read_all(path: &Path) -> Vec<Entry> {
        tail_lines(path, usize::MAX)
            .unwrap()
            .iter()
            .map(|l| serde_json::from_slice(l).unwrap())
            .collect()
    }

    #[test]
    fn tail_lines_reads_the_end() {
        let dir = temp_dir();
        let path = dir.join("f");
        let long = "x".repeat(100_000);
        fs::write(&path, format!("a\n{long}\nb\nc\n")).unwrap();
        assert_eq!(
            tail_lines(&path, 2).unwrap(),
            vec![b"b".to_vec(), b"c".to_vec()]
        );
        assert_eq!(tail_lines(&path, 3).unwrap()[0], long.as_bytes());
        assert_eq!(tail_lines(&path, 10).unwrap().len(), 4);
        assert_eq!(tail_lines(&path, 0).unwrap(), Vec::<Vec<u8>>::new());
        let blanks = "\n".repeat(200_000);
        fs::write(&path, format!("a\n{blanks}b\n")).unwrap();
        assert_eq!(
            tail_lines(&path, 2).unwrap(),
            vec![b"a".to_vec(), b"b".to_vec()]
        );
        fs::write(&path, "").unwrap();
        assert!(tail_lines(&path, 5).unwrap().is_empty());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn append_compacts_to_the_newest_entries() {
        let dir = temp_dir();
        let mut s = Session::create(&dir).unwrap();
        for i in 0..COMPACT_AT + 3 {
            s.append(&entry(i)).unwrap();
        }
        let all = read_all(s.history_path());
        assert_eq!(all.len(), HISTORY_LIMIT + 3);
        assert_eq!(all.last(), Some(&entry(COMPACT_AT + 2)));
        assert_eq!(all[0], entry(COMPACT_AT - HISTORY_LIMIT));
        s.remove();
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn history_file_is_private() {
        let dir = temp_dir();
        let mut s = Session::create(&dir).unwrap();
        s.mark_ready().unwrap();
        let mode = fs::metadata(s.history_path()).unwrap().mode();
        assert_eq!(mode & 0o777, 0o600);
        s.remove();
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn sweep_removes_dead_sessions_only() {
        let dir = temp_dir();
        let mut live = Session::create(&dir).unwrap();
        live.mark_ready().unwrap();
        // A dead session: files nobody holds a lock on, plus an old-style file.
        for name in [
            "dead.lock",
            "dead.jsonl",
            "dead.tmp",
            "4242.jsonl",
            "other.txt",
        ] {
            fs::write(dir.join(name), "").unwrap();
        }
        let _second = Session::create(&dir).unwrap();
        let mut names: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert!(names.contains(&"other.txt".to_string()));
        assert!(live.history_path().exists());
        assert!(!names
            .iter()
            .any(|n| n.starts_with("dead") || n == "4242.jsonl"));
        assert_eq!(names.len(), 4); // other.txt, live lock + jsonl, second lock
        live.remove();
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn refuses_a_shared_state_dir() {
        let dir = temp_dir();
        fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        assert!(check_private(&dir).is_err());
        fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
        assert!(check_private(&dir).is_ok());
        fs::remove_dir_all(dir).unwrap();
    }
}
