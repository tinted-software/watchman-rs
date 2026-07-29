//! fanotify-based watch backend.
//!
//! A single fanotify group can watch an *entire filesystem* with one file
//! descriptor (`FAN_MARK_FILESYSTEM`) and, since Linux 5.9, report a
//! directory file handle + child name per event (`FAN_REPORT_DFID_NAME`)
//! instead of requiring a watch descriptor per directory. That completely
//! sidesteps `inotify_add_watch`'s per-directory bookkeeping and the kernel
//! limits that come with it (`fs.inotify.max_user_watches`,
//! `max_user_instances`) -- the exact failure mode that made buck2's
//! built-in `notify`-based watcher unreliable on large repos in the first
//! place (see docs/watchman-cpp.md, section 2).
//!
//! This requires `CAP_SYS_ADMIN` (or root); when unavailable we return
//! `Ok(None)` so the caller can transparently fall back to the per-directory
//! inotify backend in `watcher.rs`.

use crate::config::Ignore;
use crate::tree::Tree;
use crate::watcher::RootState;
use std::ffi::{CStr, CString};
use std::io;
use std::os::unix::io::RawFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};

/// Linux caps file handles at 128 bytes (`MAX_HANDLE_SZ` in the kernel).
const MAX_HANDLE_SZ: usize = 128;

/// Overlay struct used to build a `libc::file_handle` with trailing storage,
/// since the real struct declares `f_handle` as a zero-length array.
#[repr(C)]
struct FileHandleBuf {
    handle_bytes: libc::c_uint,
    handle_type: libc::c_int,
    f_handle: [u8; MAX_HANDLE_SZ],
}

const WATCH_MASK: u64 = libc::FAN_CREATE
    | libc::FAN_DELETE
    | libc::FAN_MODIFY
    | libc::FAN_MOVED_FROM
    | libc::FAN_MOVED_TO
    | libc::FAN_ATTRIB
    | libc::FAN_CLOSE_WRITE
    | libc::FAN_DELETE_SELF
    | libc::FAN_MOVE_SELF
    | libc::FAN_ONDIR
    | libc::FAN_EVENT_ON_CHILD;

struct OwnedFd(RawFd);
impl Drop for OwnedFd {
    fn drop(&mut self) {
        if self.0 >= 0 {
            unsafe {
                libc::close(self.0);
            }
        }
    }
}

/// Try to start a fanotify-backed watcher for `root`. Returns `Ok(None)`
/// (never `Err` for expected failure modes) when fanotify is unusable here
/// -- e.g. missing `CAP_SYS_ADMIN`, an ancient kernel, or an unsupported
/// filesystem -- so the caller can fall back to inotify.
pub fn try_spawn(root: PathBuf, ignore: Ignore) -> io::Result<Option<Arc<RootState>>> {
    let init_flags = libc::FAN_CLASS_NOTIF
        | libc::FAN_REPORT_DFID_NAME
        | libc::FAN_UNLIMITED_QUEUE
        | libc::FAN_UNLIMITED_MARKS;
    let event_flags = (libc::O_RDONLY | libc::O_LARGEFILE | libc::O_CLOEXEC) as libc::c_uint;

    let fan_fd = unsafe { libc::fanotify_init(init_flags, event_flags) };
    if fan_fd < 0 {
        // Most commonly EPERM (no CAP_SYS_ADMIN) or ENOSYS (kernel too old).
        return Ok(None);
    }
    let fan_fd = OwnedFd(fan_fd);

    let root_c = match CString::new(root.as_os_str().as_encoded_bytes()) {
        Ok(c) => c,
        Err(_) => return Ok(None),
    };

    let rc = unsafe {
        libc::fanotify_mark(
            fan_fd.0,
            libc::FAN_MARK_ADD | libc::FAN_MARK_FILESYSTEM,
            WATCH_MASK,
            libc::AT_FDCWD,
            root_c.as_ptr(),
        )
    };
    if rc != 0 {
        // e.g. filesystem doesn't support FAN_REPORT_DFID_NAME (some
        // network/overlay filesystems); fall back to inotify.
        return Ok(None);
    }

    // `open_by_handle_at` needs *an* open fd on the filesystem the handle
    // came from; keep one open on the watched root itself for the lifetime
    // of the watcher.
    let mount_fd = unsafe {
        libc::open(
            root_c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if mount_fd < 0 {
        return Ok(None);
    }
    let mount_fd = OwnedFd(mount_fd);

    let mut tree = Tree::with_ignore(root.clone(), ignore);
    tree.crawl();
    let state = Arc::new(RootState {
        root: root.clone(),
        tree: Mutex::new(tree),
        changed: Condvar::new(),
        change_lock: Mutex::new(()),
    });

    let thread_state = state.clone();
    let watch_fd = fan_fd.0;
    let mnt_fd = mount_fd.0;
    // The spawned thread now owns both fds.
    std::mem::forget(fan_fd);
    std::mem::forget(mount_fd);
    std::thread::spawn(move || {
        let _watch_fd_guard = OwnedFd(watch_fd);
        let _mnt_fd_guard = OwnedFd(mnt_fd);
        read_loop(watch_fd, mnt_fd, thread_state);
    });

    eprintln!(
        "watchman-rs: using fanotify (whole-filesystem watch, no per-directory watch-descriptor limit) for {}",
        root.display()
    );
    Ok(Some(state))
}

fn read_loop(fan_fd: RawFd, mount_fd: RawFd, state: Arc<RootState>) {
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = unsafe { libc::read(fan_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n <= 0 {
            break;
        }
        let n = n as usize;

        let mut overflowed = false;
        let mut changed_paths: Vec<PathBuf> = Vec::new();
        let mut offset = 0usize;
        let meta_size = std::mem::size_of::<libc::fanotify_event_metadata>();

        while offset + meta_size <= n {
            let meta: libc::fanotify_event_metadata =
                unsafe { std::ptr::read_unaligned(buf[offset..].as_ptr() as *const _) };
            let event_len = meta.event_len as usize;
            if event_len < meta_size || offset + event_len > n {
                break; // corrupt/truncated read; bail out of this batch
            }

            if meta.mask & libc::FAN_Q_OVERFLOW != 0 {
                overflowed = true;
            } else if let Some(path) = decode_event_path(
                &buf[offset..offset + event_len],
                meta.metadata_len as usize,
                mount_fd,
                &state.root,
            ) {
                changed_paths.push(path);
            }

            offset += event_len;
        }

        if overflowed {
            let mut tree = state.tree.lock().unwrap();
            tree.crawl();
        } else if !changed_paths.is_empty() {
            let mut tree = state.tree.lock().unwrap();
            for p in changed_paths {
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

/// Parse the `FAN_EVENT_INFO_TYPE_DFID_NAME` extra record trailing a
/// `fanotify_event_metadata`, resolve the parent directory handle to a path
/// via `open_by_handle_at`, and join the reported child name. Returns `None`
/// for events outside our watched root (since `FAN_MARK_FILESYSTEM` reports
/// for the whole containing filesystem) or on any parsing/resolution error.
fn decode_event_path(
    event: &[u8],
    metadata_len: usize,
    mount_fd: RawFd,
    root: &Path,
) -> Option<PathBuf> {
    let hdr_size = std::mem::size_of::<libc::fanotify_event_info_header>();
    let mut off = metadata_len;
    while off + hdr_size <= event.len() {
        let hdr: libc::fanotify_event_info_header =
            unsafe { std::ptr::read_unaligned(event[off..].as_ptr() as *const _) };
        let rec_len = hdr.len as usize;
        if rec_len == 0 || off + rec_len > event.len() {
            break;
        }

        if hdr.info_type == libc::FAN_EVENT_INFO_TYPE_DFID_NAME {
            // Layout: header(4) + fsid(8) + file_handle{handle_bytes:4,handle_type:4,f_handle[..]} + NUL-terminated name.
            let fsid_off = off + hdr_size;
            let fh_off = fsid_off + 8;
            if fh_off + 8 > event.len() {
                return None;
            }
            let handle_bytes =
                u32::from_ne_bytes(event[fh_off..fh_off + 4].try_into().ok()?) as usize;
            let handle_type = i32::from_ne_bytes(event[fh_off + 4..fh_off + 8].try_into().ok()?);
            let f_handle_off = fh_off + 8;
            if handle_bytes > MAX_HANDLE_SZ || f_handle_off + handle_bytes > event.len() {
                return None;
            }

            let name_off = f_handle_off + handle_bytes;
            let name_end = (off + rec_len).min(event.len());
            let name_bytes = &event[name_off.min(name_end)..name_end];
            let name = CStr::from_bytes_until_nul(name_bytes)
                .ok()
                .map(|c| c.to_string_lossy().into_owned())
                .unwrap_or_default();

            let mut fh = FileHandleBuf {
                handle_bytes: handle_bytes as libc::c_uint,
                handle_type,
                f_handle: [0u8; MAX_HANDLE_SZ],
            };
            fh.f_handle[..handle_bytes]
                .copy_from_slice(&event[f_handle_off..f_handle_off + handle_bytes]);

            let dir_fd = unsafe {
                libc::open_by_handle_at(
                    mount_fd,
                    &mut fh as *mut FileHandleBuf as *mut libc::file_handle,
                    libc::O_PATH,
                )
            };
            if dir_fd < 0 {
                return None;
            }
            let dir_path = read_proc_fd_link(dir_fd);
            unsafe {
                libc::close(dir_fd);
            }
            let dir_path = dir_path?;

            let full = if name.is_empty() || name == "." {
                dir_path
            } else {
                dir_path.join(name)
            };

            return if full.starts_with(root) {
                Some(full)
            } else {
                None
            };
        }

        off += rec_len;
    }
    None
}

fn read_proc_fd_link(fd: RawFd) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/self/fd/{fd}")).ok()
}
