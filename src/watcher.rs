//! inotify-backed directory watcher. Recursively watches every directory
//! under a root (inotify has no recursive-watch primitive), reacts to
//! IN_CREATE by adding watches to new subdirectories, and treats
//! IN_Q_OVERFLOW by triggering a full recrawl -- mirroring watchman's
//! documented behavior (docs/watchman-cpp.md section B/2).

use crate::config::Ignore;
use crate::tree::Tree;
use inotify::{Inotify, WatchDescriptor, WatchMask};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};

pub struct RootState {
    pub root: PathBuf,
    pub tree: Mutex<Tree>,
    /// Bumped + notified any time the tree changes, so cookie-sync and
    /// subscription waiters can wake up.
    pub changed: Condvar,
    pub change_lock: Mutex<()>,
}

const WATCH_MASK: WatchMask = WatchMask::from_bits_truncate(
    WatchMask::CREATE.bits()
        | WatchMask::DELETE.bits()
        | WatchMask::DELETE_SELF.bits()
        | WatchMask::MODIFY.bits()
        | WatchMask::MOVED_FROM.bits()
        | WatchMask::MOVED_TO.bits()
        | WatchMask::ATTRIB.bits()
        | WatchMask::CLOSE_WRITE.bits(),
);

/// Start watching `root`, preferring the fanotify backend (a single fd can
/// cover an entire filesystem with no per-directory watch-descriptor limit)
/// and transparently falling back to the classic per-directory inotify
/// backend when fanotify is unavailable (e.g. no `CAP_SYS_ADMIN`).
pub fn spawn(root: PathBuf, ignore: Ignore) -> std::io::Result<Arc<RootState>> {
    match crate::fanotify::try_spawn(root.clone(), ignore.clone()) {
        Ok(Some(state)) => return Ok(state),
        Ok(None) => {
            eprintln!(
                "watchman-rs: fanotify (FAN_MARK_FILESYSTEM) unavailable for {} -- needs CAP_SYS_ADMIN or root; falling back to per-directory inotify, which is subject to fs.inotify.max_user_watches",
                root.display()
            );
        }
        Err(e) => {
            eprintln!(
                "watchman-rs: fanotify setup failed for {}: {e}; falling back to inotify",
                root.display()
            );
        }
    }
    spawn_inotify(root, ignore)
}

fn spawn_inotify(root: PathBuf, ignore: Ignore) -> std::io::Result<Arc<RootState>> {
    let inotify = Inotify::init()?;
    let mut tree = Tree::with_ignore(root.clone(), ignore);
    let dirs = tree.crawl();

    let mut wd_to_path: HashMap<WatchDescriptor, PathBuf> = HashMap::new();
    {
        let mut watches = inotify.watches();
        for dir in &dirs {
            if let Ok(wd) = watches.add(dir, WATCH_MASK) {
                wd_to_path.insert(wd, dir.clone());
            }
        }
    }

    let state = Arc::new(RootState {
        root: root.clone(),
        tree: Mutex::new(tree),
        changed: Condvar::new(),
        change_lock: Mutex::new(()),
    });

    let thread_state = state.clone();
    std::thread::spawn(move || {
        watch_loop(inotify, wd_to_path, thread_state);
    });

    Ok(state)
}

fn watch_loop(
    mut inotify: Inotify,
    mut wd_to_path: HashMap<WatchDescriptor, PathBuf>,
    state: Arc<RootState>,
) {
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let events = match inotify.read_events_blocking(&mut buffer) {
            Ok(events) => events,
            Err(_) => break,
        };

        let mut any_dir_created: Vec<PathBuf> = Vec::new();
        let mut overflowed = false;

        {
            let mut tree = state.tree.lock().unwrap();
            for event in events {
                if event.mask.contains(inotify::EventMask::Q_OVERFLOW) {
                    overflowed = true;
                    continue;
                }
                let dir = match wd_to_path.get(&event.wd) {
                    Some(d) => d.clone(),
                    None => continue,
                };
                let name = match &event.name {
                    Some(n) => n.to_string_lossy().into_owned(),
                    None => {
                        // Event on the watched directory itself (e.g. self-attrib).
                        tree.record_change(&dir);
                        continue;
                    }
                };
                let path = dir.join(&name);
                if tree.is_ignored(&path) {
                    continue;
                }
                tree.record_change(&path);
                if event.mask.contains(inotify::EventMask::CREATE)
                    && event.mask.contains(inotify::EventMask::ISDIR)
                {
                    any_dir_created.push(path);
                }
            }
        }

        if overflowed {
            // Recrawl from scratch and rebuild watch descriptor map.
            let mut tree = state.tree.lock().unwrap();
            let dirs = tree.crawl();
            drop(tree);
            let mut watches = inotify.watches();
            for wd in wd_to_path.keys().cloned().collect::<Vec<_>>() {
                let _ = watches.remove(wd);
            }
            wd_to_path.clear();
            for dir in &dirs {
                if let Ok(wd) = watches.add(dir, WATCH_MASK) {
                    wd_to_path.insert(wd, dir.clone());
                }
            }
        } else {
            for dir in &any_dir_created {
                add_watch_recursive(&mut inotify, &mut wd_to_path, dir, &state);
            }
        }

        // Wake anyone waiting on cookie files / subscriptions.
        let _guard = state.change_lock.lock().unwrap();
        state.changed.notify_all();
    }
}

fn add_watch_recursive(
    inotify: &mut Inotify,
    wd_to_path: &mut HashMap<WatchDescriptor, PathBuf>,
    dir: &Path,
    state: &Arc<RootState>,
) {
    let mut stack = vec![dir.to_path_buf()];
    let mut watches = inotify.watches();
    while let Some(d) = stack.pop() {
        if let Ok(wd) = watches.add(&d, WATCH_MASK) {
            wd_to_path.insert(wd, d.clone());
        }
        let entries = std::fs::read_dir(&d).ok();
        let mut tree = state.tree.lock().unwrap();
        tree.record_change(&d);
        if let Some(entries) = entries {
            for entry in entries.flatten() {
                let path = entry.path();
                if tree.is_ignored(&path) {
                    continue;
                }
                tree.record_change(&path);
                if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    stack.push(path);
                }
            }
        }
    }
}
