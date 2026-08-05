//! Cross-platform local IPC transport used by the daemon/client.
//!
//! On Unix this is a plain UNIX domain socket. On Windows, native `AF_UNIX`
//! support (Windows 10 1803+) is used via the `uds_windows` crate, which
//! mirrors `std::os::unix::net`'s API closely enough that `daemon.rs` /
//! `client.rs` need no further changes beyond importing from here. This
//! matches real watchman, which also speaks AF_UNIX on modern Windows
//! (falling back to named pipes only on very old versions, which we don't
//! target).

#[cfg(unix)]
pub use std::os::unix::net::{UnixListener, UnixStream};

#[cfg(windows)]
pub use uds_windows::{UnixListener, UnixStream};
