//! Cookie-file synchronization: makes sure in-flight inotify events have been
//! observed before answering a query, mirroring docs/watchman-cpp.md section C.

use crate::watcher::RootState;
use std::fs::File;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Write a cookie file into the root, then wait (bounded by `timeout`) until
/// the watcher thread observes and records it, so the in-memory tree is known
/// to be caught up with the filesystem at the time of the query.
pub fn sync(state: &Arc<RootState>, timeout: Duration) {
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let pid = std::process::id();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let name = format!(".watchman-cookie-{pid}-{now}-{seq}");
    let path = state.root.join(&name);

    if File::create(&path).is_err() {
        return;
    }

    let deadline = Instant::now() + timeout;
    let rel = name.clone();
    loop {
        {
            let tree = state.tree.lock().unwrap();
            if tree.files.get(&rel).map(|f| f.exists).unwrap_or(false) {
                break;
            }
        }
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let guard = state.change_lock.lock().unwrap();
        let _ = state.changed.wait_timeout(guard, deadline - now);
    }

    let _ = std::fs::remove_file(&path);
}
