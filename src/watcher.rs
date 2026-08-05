//! Cross-platform watch-backend dispatcher.
//!
//! `RootState` (the in-memory tree + change-notification condvar) is shared
//! by every backend. The actual OS-level watching mechanism is chosen per
//! platform, mirroring what real watchman does (docs/watchman-cpp.md):
//!
//! * Linux: fanotify (`FAN_MARK_FILESYSTEM`, whole-filesystem, single fd)
//!   with a fallback to per-directory inotify -- see `fanotify.rs` /
//!   `linux_watcher.rs`.
//! * macOS: FSEvents, which -- like fanotify -- can watch an entire
//!   directory tree recursively with a single subscription instead of one
//!   watch per directory -- see `fsevents.rs`.
//! * Windows: `ReadDirectoryChangesW` with `bWatchSubtree = TRUE`, which is
//!   the closest Windows equivalent to a single recursive watch -- see
//!   `windows_watcher.rs`.

use crate::config::Ignore;
use crate::tree::Tree;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};

pub struct RootState {
    pub root: PathBuf,
    pub tree: Mutex<Tree>,
    /// Bumped + notified any time the tree changes, so cookie-sync and
    /// subscription waiters can wake up.
    pub changed: Condvar,
    pub change_lock: Mutex<()>,
}

/// Start watching `root`, using the best available backend for the current
/// platform.
pub fn spawn(root: PathBuf, ignore: Ignore) -> std::io::Result<Arc<RootState>> {
    #[cfg(target_os = "linux")]
    {
        crate::linux_watcher::spawn(root, ignore)
    }
    #[cfg(target_os = "macos")]
    {
        crate::fsevents::spawn(root, ignore)
    }
    #[cfg(target_os = "windows")]
    {
        crate::windows_watcher::spawn(root, ignore)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        crate::poll_watcher::spawn(root, ignore)
    }
}

/// Name reported to clients for the `watcher` field (e.g. in `watch`/
/// `watch-project` responses), identifying which backend is active. Note
/// this reflects platform selection, not the Linux fanotify-vs-inotify
/// runtime fallback (`fanotify.rs` logs that choice separately).
pub fn watcher_name() -> &'static str {
    #[cfg(target_os = "linux")]
    {
        "inotify"
    }
    #[cfg(target_os = "macos")]
    {
        "fsevents"
    }
    #[cfg(target_os = "windows")]
    {
        "win32"
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        "poll"
    }
}
