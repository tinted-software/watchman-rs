//! In-memory representation of a watched root: a flat map of relative path ->
//! file metadata, plus a logical clock that is bumped on every observed
//! change. This lets queries be answered entirely in memory, as watchman
//! does (see docs/watchman-cpp.md section D).

use crate::config::Ignore;
use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Watchman's single-letter file type codes (see `FileType` in the
/// `watchman_client` crate / `docs/expr/type.html`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Regular,
    Directory,
    Symlink,
    BlockSpecial,
    CharSpecial,
    Fifo,
    Socket,
    Unknown,
}

impl Kind {
    pub fn from_metadata(meta: &fs::Metadata) -> Kind {
        let ft = meta.file_type();
        if ft.is_dir() {
            Kind::Directory
        } else if ft.is_symlink() {
            Kind::Symlink
        } else if ft.is_file() {
            Kind::Regular
        } else if ft.is_block_device() {
            Kind::BlockSpecial
        } else if ft.is_char_device() {
            Kind::CharSpecial
        } else if ft.is_fifo() {
            Kind::Fifo
        } else if ft.is_socket() {
            Kind::Socket
        } else {
            Kind::Unknown
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            Kind::BlockSpecial => "b",
            Kind::CharSpecial => "c",
            Kind::Directory => "d",
            Kind::Regular => "f",
            Kind::Fifo => "p",
            Kind::Symlink => "l",
            Kind::Socket => "s",
            Kind::Unknown => "?",
        }
    }
}

#[derive(Debug, Clone)]
pub struct FileInfo {
    pub name: String, // path relative to root, '/' separated
    pub exists: bool,
    pub kind: Kind,
    pub size: u64,
    pub mtime: i64,
    pub mode: u32,
    /// Logical clock tick at which this entry was last changed.
    pub ctick: u64,
    /// Logical clock tick at which this entry was first observed to exist.
    /// Used to compute the watchman `new` field.
    pub first_tick: u64,
}

pub struct Clock {
    pid: u32,
    ticks: u64,
}

impl Clock {
    pub fn new() -> Self {
        Clock {
            pid: std::process::id(),
            ticks: 1,
        }
    }

    pub fn bump(&mut self) -> u64 {
        self.ticks += 1;
        self.ticks
    }

    pub fn now(&self) -> u64 {
        self.ticks
    }

    pub fn format(&self, ticks: u64) -> String {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        format!("c:{}:{}:{}", now, self.pid, ticks)
    }
}

/// Parse a clock string of the form `c:<time>:<pid>:<ticks>`. Only the ticks
/// component matters for our in-memory "since" comparisons. A null clock
/// (`c:0:0` or anything unparseable) is treated as "no clock", forcing a
/// fresh-instance result, matching watchman's `ClockSpec::null()`.
pub fn parse_clock_ticks(s: &str) -> Option<u64> {
    let mut parts = s.split(':');
    if parts.next()? != "c" {
        return None;
    }
    let _time = parts.next()?;
    // Older/simplified clocks may omit the pid or ticks component; tolerate
    // both `c:<time>:<pid>:<ticks>` and `c:<time>:<ticks>`.
    let rest: Vec<&str> = parts.collect();
    match rest.len() {
        2 => rest[1].parse::<u64>().ok(),
        1 => rest[0].parse::<u64>().ok(),
        _ => None,
    }
}

pub struct Tree {
    pub root: PathBuf,
    pub files: HashMap<String, FileInfo>,
    pub clock: Clock,
    pub ignore: Ignore,
}

impl Tree {
    pub fn with_ignore(root: PathBuf, ignore: Ignore) -> Self {
        Tree {
            root,
            files: HashMap::new(),
            clock: Clock::new(),
            ignore,
        }
    }

    pub fn is_ignored(&self, path: &Path) -> bool {
        self.ignore.is_ignored(path)
    }

    fn rel(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    }

    /// Full recursive crawl, populating the in-memory tree. Returns the list
    /// of directories discovered so the caller can register inotify watches
    /// on each of them.
    pub fn crawl(&mut self) -> Vec<PathBuf> {
        let mut dirs = vec![self.root.clone()];
        let mut stack = vec![self.root.clone()];
        let tick = self.clock.bump();
        while let Some(dir) = stack.pop() {
            let entries = match fs::read_dir(&dir) {
                Ok(e) => e,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if self.ignore.is_ignored(&path) {
                    continue;
                }
                let meta = match fs::symlink_metadata(&path) {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                let is_dir = meta.is_dir();
                let rel = self.rel(&path);
                self.upsert(rel, &meta, tick);
                if is_dir {
                    dirs.push(path.clone());
                    stack.push(path);
                }
            }
        }
        dirs
    }

    fn upsert(&mut self, rel: String, meta: &fs::Metadata, tick: u64) {
        let first_tick = self.files.get(&rel).map(|f| f.first_tick).unwrap_or(tick);
        self.files.insert(
            rel.clone(),
            FileInfo {
                name: rel,
                exists: true,
                kind: Kind::from_metadata(meta),
                size: meta.len(),
                mtime: meta.mtime(),
                mode: meta.mode(),
                ctick: tick,
                first_tick,
            },
        );
    }

    /// Record that the given absolute path changed on disk (created,
    /// modified, or removed). Bumps the clock and updates/removes the entry.
    pub fn record_change(&mut self, path: &Path) -> u64 {
        let tick = self.clock.bump();
        let rel = self.rel(path);
        match fs::symlink_metadata(path) {
            Ok(meta) => {
                self.upsert(rel, &meta, tick);
            }
            Err(_) => {
                if let Some(info) = self.files.get_mut(&rel) {
                    info.exists = false;
                    info.ctick = tick;
                } else {
                    self.files.insert(
                        rel.clone(),
                        FileInfo {
                            name: rel,
                            exists: false,
                            kind: Kind::Unknown,
                            size: 0,
                            mtime: 0,
                            mode: 0,
                            ctick: tick,
                            first_tick: tick,
                        },
                    );
                }
            }
        }
        tick
    }

    pub fn since(&self, ticks: u64) -> Vec<&FileInfo> {
        self.files.values().filter(|f| f.ctick > ticks).collect()
    }
}

pub fn is_cookie(name: &str) -> bool {
    name.rsplit('/')
        .next()
        .unwrap_or(name)
        .starts_with(".watchman-cookie-")
}
