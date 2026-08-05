//! Generic polling fallback for platforms without a native recursive-watch
//! API wired up yet (anything other than Linux/macOS/Windows). Simply
//! recrawls the tree on an interval; correct but not scalable, matching real
//! watchman's own last-resort behavior on unsupported platforms.

use crate::config::Ignore;
use crate::tree::Tree;
use crate::watcher::RootState;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

const POLL_INTERVAL: Duration = Duration::from_millis(1000);

pub fn spawn(root: PathBuf, ignore: Ignore) -> std::io::Result<Arc<RootState>> {
    let mut tree = Tree::with_ignore(root.clone(), ignore);
    tree.crawl();

    let state = Arc::new(RootState {
        root: root.clone(),
        tree: Mutex::new(tree),
        changed: Condvar::new(),
        change_lock: Mutex::new(()),
    });

    eprintln!(
        "watchman-rs: no native watch backend for this platform; polling {} every {:?}",
        root.display(),
        POLL_INTERVAL
    );

    let thread_state = state.clone();
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(POLL_INTERVAL);
            {
                let mut tree = thread_state.tree.lock().unwrap();
                tree.crawl();
            }
            let guard = thread_state.change_lock.lock().unwrap();
            thread_state.changed.notify_all();
            drop(guard);
        }
    });

    Ok(state)
}
