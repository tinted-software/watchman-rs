//! FSEvents-based watch backend for macOS.
//!
//! Like Linux's `fanotify` backend, FSEvents lets us watch an entire
//! directory tree recursively with a single subscription instead of one
//! watch per directory (the approach `inotify`, and Watchman's own
//! per-directory fallback on Linux, are forced into). This is exactly the
//! mechanism real Watchman uses on macOS (see docs/watchman-cpp.md).
//!
//! We still keep an in-memory `Tree` (built by an initial crawl) and
//! recrawl on any event whose flags indicate the kernel couldn't keep up
//! (`kFSEventStreamEventFlagMustScanSubDirs` / `UserDropped` /
//! `KernelDropped`), mirroring the `IN_Q_OVERFLOW` handling on Linux.
//!
//! Uses the actively-maintained `objc2-core-services` bindings (rather than
//! the deprecated `fsevent-sys`), and delivers events via a private
//! dispatch queue (`FSEventStreamSetDispatchQueue`) instead of the
//! also-deprecated `FSEventStreamScheduleWithRunLoop`, so no dedicated
//! CFRunLoop-pumping thread is needed at all -- libdispatch runs the
//! callback on its own worker thread.

use crate::config::Ignore;
use crate::tree::Tree;
use crate::watcher::RootState;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2_core_foundation::{CFArray, CFString};
use objc2_core_services::{
    ConstFSEventStreamRef, FSEventStreamContext, FSEventStreamCreate, FSEventStreamEventFlags,
    FSEventStreamEventId, FSEventStreamRef, FSEventStreamRelease, FSEventStreamSetDispatchQueue,
    FSEventStreamStart, kFSEventStreamCreateFlagFileEvents, kFSEventStreamCreateFlagNoDefer,
    kFSEventStreamCreateFlagWatchRoot, kFSEventStreamEventFlagKernelDropped,
    kFSEventStreamEventFlagMustScanSubDirs, kFSEventStreamEventFlagUserDropped,
    kFSEventStreamEventIdSinceNow,
};
use std::ffi::CStr;
use std::os::raw::c_void;
use std::path::PathBuf;
use std::ptr::NonNull;
use std::sync::{Arc, Condvar, Mutex};

struct StreamContext {
    state: Arc<RootState>,
}

/// Owns the FSEventStream + the dispatch queue delivering its callbacks, and
/// the boxed `StreamContext` handed to it as `info`. Never dropped in
/// practice (the watcher lives for the process lifetime), but modeled
/// properly so it's not UB if it ever were.
struct StreamHandle {
    stream: FSEventStreamRef,
    _queue: DispatchRetained<DispatchQueue>,
    ctx_ptr: *mut StreamContext,
}
unsafe impl Send for StreamHandle {}

impl Drop for StreamHandle {
    fn drop(&mut self) {
        unsafe {
            FSEventStreamRelease(self.stream);
            drop(Box::from_raw(self.ctx_ptr));
        }
    }
}

pub fn spawn(root: PathBuf, ignore: Ignore) -> std::io::Result<Arc<RootState>> {
    let mut tree = Tree::with_ignore(root.clone(), ignore);
    tree.crawl();

    let state = Arc::new(RootState {
        root: root.clone(),
        tree: Mutex::new(tree),
        changed: Condvar::new(),
        change_lock: Mutex::new(()),
    });

    let handle = create_stream(&root, state.clone()).ok_or_else(|| {
        std::io::Error::other(format!("FSEventStreamCreate failed for {}", root.display()))
    })?;
    // Leak the handle: the stream must stay alive for the process lifetime,
    // same as every other backend's spawned watcher thread.
    std::mem::forget(handle);

    eprintln!(
        "watchman-rs: using FSEvents (recursive whole-tree watch) for {}",
        root.display()
    );

    Ok(state)
}

extern "C-unwind" fn fsevents_callback(
    _stream_ref: ConstFSEventStreamRef,
    info: *mut c_void,
    num_events: usize,
    event_paths: NonNull<c_void>,
    event_flags: NonNull<FSEventStreamEventFlags>,
    _event_ids: NonNull<FSEventStreamEventId>,
) {
    if info.is_null() {
        return;
    }
    let ctx = unsafe { &*(info as *const StreamContext) };
    let paths = event_paths.as_ptr() as *const *const std::os::raw::c_char;
    let event_flags = event_flags.as_ptr();

    let mut overflowed = false;
    let mut changed: Vec<PathBuf> = Vec::with_capacity(num_events);

    for i in 0..num_events {
        let flags = unsafe { *event_flags.add(i) };
        if flags
            & (kFSEventStreamEventFlagMustScanSubDirs
                | kFSEventStreamEventFlagUserDropped
                | kFSEventStreamEventFlagKernelDropped)
            != 0
        {
            overflowed = true;
            continue;
        }
        let cpath = unsafe { *paths.add(i) };
        if cpath.is_null() {
            continue;
        }
        let path_str = unsafe { CStr::from_ptr(cpath) }
            .to_string_lossy()
            .into_owned();
        changed.push(PathBuf::from(path_str));
    }

    if overflowed {
        let mut tree = ctx.state.tree.lock().unwrap();
        tree.crawl();
    } else if !changed.is_empty() {
        let mut tree = ctx.state.tree.lock().unwrap();
        for p in changed {
            if !tree.is_ignored(&p) {
                tree.record_change(&p);
            }
        }
    }

    let guard = ctx.state.change_lock.lock().unwrap();
    ctx.state.changed.notify_all();
    drop(guard);
}

fn create_stream(root: &std::path::Path, state: Arc<RootState>) -> Option<StreamHandle> {
    let ctx_ptr = Box::into_raw(Box::new(StreamContext { state }));

    let mut stream_context = FSEventStreamContext {
        version: 0,
        info: ctx_ptr as *mut c_void,
        retain: None,
        release: None,
        copyDescription: None,
    };

    let path_cf = CFString::from_str(&root.to_string_lossy());
    let paths_to_watch = CFArray::from_objects(&[&*path_cf]);

    // NoDefer gives us prompt notifications rather than batching for the
    // default ~1s coalescing window; FileEvents gives per-file paths
    // instead of just directories, closer to inotify's granularity.
    let flags = kFSEventStreamCreateFlagNoDefer
        | kFSEventStreamCreateFlagFileEvents
        | kFSEventStreamCreateFlagWatchRoot;

    let stream = unsafe {
        FSEventStreamCreate(
            None,
            Some(fsevents_callback),
            &mut stream_context,
            paths_to_watch.as_opaque(),
            kFSEventStreamEventIdSinceNow,
            0.05,
            flags,
        )
    };

    if stream.is_null() {
        unsafe {
            drop(Box::from_raw(ctx_ptr));
        }
        return None;
    }

    // Deliver callbacks on our own private serial queue rather than the
    // (deprecated for this purpose) CFRunLoop scheduling API; libdispatch
    // runs it on its own worker thread for the life of the process.
    let queue = DispatchQueue::new("watchman-rs.fsevents", None);
    unsafe {
        FSEventStreamSetDispatchQueue(stream, Some(&queue));
        FSEventStreamStart(stream);
    }

    Some(StreamHandle {
        stream,
        _queue: queue,
        ctx_ptr,
    })
}
