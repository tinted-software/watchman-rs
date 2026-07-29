//! `.watchmanconfig` support: a JSON file at the project root, per
//! https://facebook.github.io/watchman/docs/config -- used here mainly for
//! `ignore_dirs` / `ignore_vcs`, letting a repo declare directories that
//! should never be crawled, watched, or reported in query results (e.g.
//! build output directories that would otherwise churn constantly and eat
//! into watch-descriptor / fanotify mark budgets).

use crate::json;
use crate::value::Value;
use std::path::{Path, PathBuf};

/// Version-control directories that are ignored by default, matching real
/// watchman's built-in behavior (`ignore_vcs` defaults to `[".git", ".hg",
/// ".svn"]`).
const DEFAULT_VCS_DIRS: &[&str] = &[".git", ".hg", ".svn"];

/// Parsed contents of `.watchmanconfig`. Unrecognized keys are ignored
/// (matching real watchman's forward-compatible parsing); a missing or
/// unparseable file yields defaults, never an error, so watching still
/// works without any config present.
#[derive(Debug, Clone, Default)]
pub struct Config {
    /// Directories (relative to the watch root) to fully ignore.
    pub ignore_dirs: Vec<String>,
    /// Names of VCS directories to auto-ignore. `None` means "use the
    /// built-in default list"; `Some(vec![])` disables VCS ignoring
    /// entirely (matches setting `"ignore_vcs": []` in real watchman).
    pub ignore_vcs: Option<Vec<String>>,
}

impl Config {
    /// Load `<root>/.watchmanconfig` if present; falls back to `Config::default()`
    /// on any I/O or parse error.
    pub fn load(root: &Path) -> Config {
        let path = root.join(".watchmanconfig");
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(_) => return Config::default(),
        };
        let value = match json::decode(&text) {
            Ok(v) => v,
            Err(_) => return Config::default(),
        };
        Config::from_value(&value)
    }

    fn from_value(value: &Value) -> Config {
        let mut cfg = Config::default();
        let Value::Object(pairs) = value else {
            return cfg;
        };
        for (key, val) in pairs {
            match key.as_str() {
                "ignore_dirs" => {
                    if let Value::Array(items) = val {
                        cfg.ignore_dirs = items
                            .iter()
                            .filter_map(|v| v.as_str())
                            .map(|s| s.trim_matches('/').to_string())
                            .filter(|s| !s.is_empty())
                            .collect();
                    }
                }
                "ignore_vcs" => {
                    if let Value::Array(items) = val {
                        cfg.ignore_vcs = Some(
                            items
                                .iter()
                                .filter_map(|v| v.as_str())
                                .map(|s| s.to_string())
                                .collect(),
                        );
                    }
                }
                _ => {}
            }
        }
        cfg
    }
}

/// Runtime ignore-matcher derived from a loaded `Config` for one watch root,
/// holding absolute paths so path comparisons at crawl/watch time are cheap.
#[derive(Debug, Clone)]
pub struct Ignore {
    dirs: Vec<PathBuf>,
    vcs_names: Vec<String>,
}

impl Ignore {
    pub fn new(root: &Path, cfg: &Config) -> Ignore {
        let dirs = cfg.ignore_dirs.iter().map(|d| root.join(d)).collect();
        let vcs_names = cfg
            .ignore_vcs
            .clone()
            .unwrap_or_else(|| DEFAULT_VCS_DIRS.iter().map(|s| s.to_string()).collect());
        Ignore { dirs, vcs_names }
    }

    pub fn is_ignored(&self, path: &Path) -> bool {
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if self.vcs_names.iter().any(|v| v == name) {
                return true;
            }
        }
        self.dirs.iter().any(|d| path == d || path.starts_with(d))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ignore_dirs_and_vcs() {
        let cfg = Config::from_value(
            &json::decode(r#"{"ignore_dirs": ["build", "/node_modules/"], "ignore_vcs": []}"#)
                .unwrap(),
        );
        assert_eq!(
            cfg.ignore_dirs,
            vec!["build".to_string(), "node_modules".to_string()]
        );
        assert_eq!(cfg.ignore_vcs, Some(vec![]));

        let ignore = Ignore::new(Path::new("/root"), &cfg);
        assert!(ignore.is_ignored(Path::new("/root/build")));
        assert!(ignore.is_ignored(Path::new("/root/build/inner")));
        assert!(ignore.is_ignored(Path::new("/root/node_modules")));
        assert!(!ignore.is_ignored(Path::new("/root/.git"))); // ignore_vcs: [] disables it
        assert!(!ignore.is_ignored(Path::new("/root/src")));
    }

    #[test]
    fn defaults_ignore_vcs_dirs() {
        let cfg = Config::default();
        let ignore = Ignore::new(Path::new("/root"), &cfg);
        assert!(ignore.is_ignored(Path::new("/root/.git")));
        assert!(ignore.is_ignored(Path::new("/root/.hg")));
        assert!(!ignore.is_ignored(Path::new("/root/src")));
    }

    #[test]
    fn missing_file_yields_defaults() {
        let cfg = Config::load(Path::new("/nonexistent-dir-xyz"));
        assert!(cfg.ignore_dirs.is_empty());
        assert!(cfg.ignore_vcs.is_none());
    }
}
