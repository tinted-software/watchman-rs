//! Windows watch backend using `ReadDirectoryChangesW` with
//! `bWatchSubtree = TRUE`. This is the closest Windows equivalent to
//! fanotify's whole-filesystem watch / FSEvents' whole-tree watch: a single
//! handle recursively covers the entire directory tree instead of one watch
//! per directory (unlike inotify), which is exactly why real Watchman also
//! uses it as its Windows backend (see docs/watchman-cpp.md).
//!
//! On overflow (`ERROR_NOTIFY_ENUM_DIR`, i.e. the fixed-size buffer couldn't
//! hold all changes since the last read) we recrawl the whole tree, mirroring
//! the `IN_Q_OVERFLOW` / FSEvents "must scan subdirs" handling on the other
//! platforms.

use crate::config::Ignore;
use crate::tree::Tree;
use crate::watcher::RootState;
use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_NOTIFY_ENUM_DIR, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ACTION_ADDED, FILE_ACTION_MODIFIED, FILE_ACTION_REMOVED, FILE_ACTION_RENAMED_NEW_NAME,
    FILE_ACTION_RENAMED_OLD_NAME, FILE_FLAG_BACKUP_SEMANTICS, FILE_LIST_DIRECTORY,
    FILE_NOTIFY_CHANGE_ATTRIBUTES, FILE_NOTIFY_CHANGE_CREATION, FILE_NOTIFY_CHANGE_DIR_NAME,
    FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE,
    FILE_NOTIFY_INFORMATION, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    ReadDirectoryChangesW,
};

const NOTIFY_FILTER: u32 = FILE_NOTIFY_CHANGE_FILE_NAME
    | FILE_NOTIFY_CHANGE_DIR_NAME
    | FILE_NOTIFY_CHANGE_ATTRIBUTES
    | FILE_NOTIFY_CHANGE_SIZE
    | FILE_NOTIFY_CHANGE_LAST_WRITE
    | FILE_NOTIFY_CHANGE_CREATION;

/// Wraps the directory handle so it's closed exactly once even though we
/// hand a raw copy of it to the blocking OS call each iteration.
struct DirHandle(HANDLE);
unsafe impl Send for DirHandle {}
impl Drop for DirHandle {
    fn drop(&mut self) {
        if self.0 != INVALID_HANDLE_VALUE && !self.0.is_null() {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

pub fn spawn(root: PathBuf, ignore: Ignore) -> std::io::Result<Arc<RootState>> {
    let mut wide: Vec<u16> = root.as_os_str().encode_wide().collect();
    wide.push(0);

    let handle = unsafe {
        windows_sys::Win32::Storage::FileSystem::CreateFileW(
            wide.as_ptr(),
            FILE_LIST_DIRECTORY,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    let handle = DirHandle(handle);

    let mut tree = Tree::with_ignore(root.clone(), ignore);
    tree.crawl();

    let state = Arc::new(RootState {
        root: root.clone(),
        tree: Mutex::new(tree),
        changed: Condvar::new(),
        change_lock: Mutex::new(()),
    });

    let thread_state = state.clone();
    let thread_root = root.clone();
    std::thread::spawn(move || {
        watch_loop(handle, thread_root, thread_state);
    });

    eprintln!(
        "watchman-rs: using ReadDirectoryChangesW (recursive whole-tree watch) for {}",
        root.display()
    );

    Ok(state)
}

fn watch_loop(handle: DirHandle, root: PathBuf, state: Arc<RootState>) {
    // 64KiB is the largest buffer guaranteed to work over the network too;
    // real Watchman uses the same figure for this API.
    let mut buf = vec![0u8; 64 * 1024];

    loop {
        let mut bytes_returned: u32 = 0;
        let ok = unsafe {
            ReadDirectoryChangesW(
                handle.0,
                buf.as_mut_ptr() as *mut _,
                buf.len() as u32,
                1, // bWatchSubtree = TRUE: recursive
                NOTIFY_FILTER,
                &mut bytes_returned,
                std::ptr::null_mut(),
                None,
            )
        };

        if ok == 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(ERROR_NOTIFY_ENUM_DIR as i32) {
                let mut tree = state.tree.lock().unwrap();
                tree.crawl();
                let guard = state.change_lock.lock().unwrap();
                state.changed.notify_all();
                drop(guard);
                continue;
            }
            break;
        }

        if bytes_returned == 0 {
            // Buffer overflow: too many changes since the last read to fit.
            let mut tree = state.tree.lock().unwrap();
            tree.crawl();
            let guard = state.change_lock.lock().unwrap();
            state.changed.notify_all();
            drop(guard);
            continue;
        }

        let mut changed: Vec<PathBuf> = Vec::new();
        let mut offset = 0usize;
        loop {
            if offset + std::mem::size_of::<FILE_NOTIFY_INFORMATION>() > buf.len() {
                break;
            }
            let info = unsafe { &*(buf[offset..].as_ptr() as *const FILE_NOTIFY_INFORMATION) };
            let name_len_bytes = info.FileNameLength as usize;
            let name_ptr = unsafe {
                (buf[offset..].as_ptr() as *const u8)
                    .add(std::mem::offset_of!(FILE_NOTIFY_INFORMATION, FileName))
            } as *const u16;
            let name_units = name_len_bytes / 2;
            let name_slice = unsafe { std::slice::from_raw_parts(name_ptr, name_units) };
            let name = OsString::from_wide(name_slice);
            let full = root.join(PathBuf::from(name));

            match info.Action {
                FILE_ACTION_ADDED
                | FILE_ACTION_MODIFIED
                | FILE_ACTION_REMOVED
                | FILE_ACTION_RENAMED_NEW_NAME
                | FILE_ACTION_RENAMED_OLD_NAME => changed.push(full),
                _ => {}
            }

            if info.NextEntryOffset == 0 {
                break;
            }
            offset += info.NextEntryOffset as usize;
        }

        if !changed.is_empty() {
            let mut tree = state.tree.lock().unwrap();
            for p in changed {
                if !tree.is_ignored(&p) {
                    tree.record_change(&p);
                }
            }
        }

        let guard = state.change_lock.lock().unwrap();
        state.changed.notify_all();
        drop(guard);
    }
}
