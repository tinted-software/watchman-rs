//! Filesystem locations for the daemon socket/pid/state, matching watchman's
//! convention of a per-user directory under the system temp dir.

use std::path::{Path, PathBuf};

pub fn canonicalize(p: &str) -> std::io::Result<PathBuf> {
    std::fs::canonicalize(Path::new(p))
}

fn user_name() -> String {
    #[cfg(windows)]
    {
        std::env::var("USERNAME").unwrap_or_else(|_| "unknown".to_string())
    }
    #[cfg(not(windows))]
    {
        std::env::var("USER")
            .or_else(|_| std::env::var("LOGNAME"))
            .unwrap_or_else(|_| "unknown".to_string())
    }
}

fn temp_dir() -> PathBuf {
    #[cfg(windows)]
    {
        std::env::var("TEMP")
            .or_else(|_| std::env::var("TMP"))
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("C:\\Windows\\Temp"))
    }
    #[cfg(not(windows))]
    {
        PathBuf::from(std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".to_string()))
    }
}

pub fn state_dir() -> PathBuf {
    temp_dir().join(format!("watchman-rs-{}", user_name()))
}

pub fn sock_path() -> PathBuf {
    state_dir().join("sock")
}

#[allow(dead_code)]
pub fn pid_path() -> PathBuf {
    state_dir().join("pid")
}
