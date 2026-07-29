//! Filesystem locations for the daemon socket/pid/state, matching watchman's
//! convention of a per-user directory under the system temp dir.

use std::path::{Path, PathBuf};

pub fn canonicalize(p: &str) -> std::io::Result<PathBuf> {
    std::fs::canonicalize(Path::new(p))
}

fn user_name() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| "unknown".to_string())
}

pub fn state_dir() -> PathBuf {
    let base = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(base).join(format!("watchman-rs-{}", user_name()))
}

pub fn sock_path() -> PathBuf {
    state_dir().join("sock")
}

#[allow(dead_code)]
pub fn pid_path() -> PathBuf {
    state_dir().join("pid")
}
