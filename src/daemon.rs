//! The persistent background daemon: listens on a UNIX domain socket and
//! serves BSER/JSON requests, one thread per connection, sharing watched
//! roots across all clients (docs/watchman-cpp.md section A/3).
//!
//! Wire compatibility note: real watchman clients (e.g. buck2's
//! `watchman_client` crate) always speak BSER "v2" framing over this socket
//! (see protocol.rs / bser.rs), discovering the socket path by exec'ing
//! `watchman --output-encoding bser-v2 get-sockname` once. All of the actual
//! watch/query traffic then goes straight over the socket, never through the
//! CLI again.

use crate::config::{Config, Ignore};
use crate::cookie;
use crate::ipc::{UnixListener, UnixStream};
use crate::paths;
use crate::protocol;
use crate::query::Expr;
use crate::tree;
use crate::value::Value;
use crate::watcher::{self, RootState};
use std::collections::HashMap;
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const VERSION: &str = "2024.01.01-rs";

/// Files whose presence marks a directory as a project root, used by
/// `watch-project` to consolidate watches (docs.watchman-cpp.md /
/// facebook.github.io/watchman/docs/cmd/watch-project).
const ROOT_FILES: &[&str] = &[".watchmanconfig", ".git", ".hg", ".svn"];

pub struct Daemon {
    roots: Mutex<HashMap<String, Arc<RootState>>>,
}

impl Daemon {
    pub fn new() -> Self {
        Daemon {
            roots: Mutex::new(HashMap::new()),
        }
    }

    fn watch(&self, root: PathBuf) -> std::io::Result<Arc<RootState>> {
        let key = root.to_string_lossy().into_owned();
        let mut roots = self.roots.lock().unwrap();
        if let Some(existing) = roots.get(&key) {
            return Ok(existing.clone());
        }
        let cfg = Config::load(&root);
        let ignore = Ignore::new(&root, &cfg);
        let state = watcher::spawn(root, ignore)?;
        roots.insert(key, state.clone());
        Ok(state)
    }
}

/// Locate the project root containing `path`, per watch-project's documented
/// search algorithm: walk upward looking for one of `ROOT_FILES`. Falls back
/// to `path` itself if nothing is found.
fn find_project_root(path: &Path) -> (PathBuf, Option<String>) {
    let mut candidate = path;
    loop {
        if ROOT_FILES.iter().any(|f| candidate.join(f).exists()) {
            let rel = path
                .strip_prefix(candidate)
                .ok()
                .filter(|p| !p.as_os_str().is_empty());
            return (
                candidate.to_path_buf(),
                rel.map(|p| p.to_string_lossy().into_owned()),
            );
        }
        match candidate.parent() {
            Some(parent) => candidate = parent,
            None => return (path.to_path_buf(), None),
        }
    }
}

pub fn run_foreground(sock_path: PathBuf) -> std::io::Result<()> {
    let _ = std::fs::remove_file(&sock_path);
    if let Some(parent) = sock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let listener = UnixListener::bind(&sock_path)?;
    let daemon = Arc::new(Daemon::new());
    eprintln!("watchman-rs: listening on {}", sock_path.display());
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let daemon = daemon.clone();
                std::thread::spawn(move || {
                    let _ = handle_conn(daemon, stream);
                });
            }
            Err(_) => continue,
        }
    }
    Ok(())
}

fn handle_conn(daemon: Arc<Daemon>, stream: UnixStream) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = BufWriter::new(stream);
    loop {
        let (req, framing) = match protocol::read_request(&mut reader)? {
            Some(v) => v,
            None => return Ok(()),
        };
        let resp = dispatch(&daemon, &req);
        protocol::write_response(&mut writer, &resp, &framing)?;
    }
}

fn err_resp(msg: impl Into<String>) -> Value {
    Value::obj()
        .set("error", msg.into())
        .set("version", VERSION)
        .build()
}

fn dispatch(daemon: &Daemon, req: &Value) -> Value {
    let cmd = match req
        .as_array()
        .and_then(|a| a.first())
        .and_then(|v| v.as_str())
    {
        Some(c) => c,
        None => return err_resp("request must be an array with a command name"),
    };
    let args = req.as_array().unwrap();

    match cmd {
        "version" => Value::obj()
            .set("version", VERSION)
            .set(
                "capabilities",
                Value::obj()
                    .set("relative_root", true)
                    .set("bser-v2", true)
                    .set("wildmatch", true)
                    .build(),
            )
            .build(),

        "get-sockname" => Value::obj()
            .set("version", VERSION)
            .set(
                "sockname",
                paths::sock_path().to_string_lossy().into_owned(),
            )
            .build(),

        "watch" => {
            let path = match args.get(1).and_then(|v| v.as_str()) {
                Some(p) => p,
                None => return err_resp("watch requires a path argument"),
            };
            let root = match paths::canonicalize(path) {
                Ok(r) => r,
                Err(e) => return err_resp(format!("failed to resolve {path}: {e}")),
            };
            match daemon.watch(root.clone()) {
                Ok(_) => Value::obj()
                    .set("watch", root.to_string_lossy().into_owned())
                    .set("watcher", watcher::watcher_name())
                    .set("version", VERSION)
                    .build(),
                Err(e) => err_resp(format!("failed to watch {}: {e}", root.display())),
            }
        }

        "watch-project" => {
            let path = match args.get(1).and_then(|v| v.as_str()) {
                Some(p) => p,
                None => return err_resp("watch-project requires a path argument"),
            };
            let resolved = match paths::canonicalize(path) {
                Ok(r) => r,
                Err(e) => return err_resp(format!("failed to resolve {path}: {e}")),
            };
            let (root, relative_path) = find_project_root(&resolved);
            match daemon.watch(root.clone()) {
                Ok(_) => {
                    let mut obj = Value::obj()
                        .set("version", VERSION)
                        .set("watch", root.to_string_lossy().into_owned())
                        .set("watcher", watcher::watcher_name());
                    if let Some(rel) = relative_path {
                        obj = obj.set("relative_path", rel);
                    }
                    obj.build()
                }
                Err(e) => err_resp(format!("failed to watch {}: {e}", root.display())),
            }
        }

        "clock" => {
            let path = match args.get(1).and_then(|v| v.as_str()) {
                Some(p) => p,
                None => return err_resp("clock requires a path argument"),
            };
            let root = match paths::canonicalize(path) {
                Ok(r) => r,
                Err(e) => return err_resp(format!("failed to resolve {path}: {e}")),
            };
            let state = match daemon.watch(root) {
                Ok(s) => s,
                Err(e) => return err_resp(e.to_string()),
            };
            let tree = state.tree.lock().unwrap();
            Value::obj()
                .set("clock", tree.clock.format(tree.clock.now()))
                .set("version", VERSION)
                .build()
        }

        "query" => handle_query(daemon, args),

        "flush-subscriptions" | "subscribe" | "unsubscribe" => Value::obj()
            .set("version", VERSION)
            .set("synced", true)
            .build(),

        "shutdown-server" => Value::obj()
            .set("version", VERSION)
            .set("shutdown-server", true)
            .build(),

        other => err_resp(format!("unknown command: {other}")),
    }
}

/// Extract the tick count from a watchman `Clock` value, which may be a bare
/// clockspec string (`"c:<time>:<pid>:<ticks>"`), a null clock, or an
/// scm-aware object of the form `{"clock": "...", "scm": {...}}`.
fn since_ticks_from(v: &Value) -> Option<u64> {
    match v {
        Value::Str(s) => tree::parse_clock_ticks(s),
        Value::Object(_) => v
            .get("clock")
            .and_then(|c| c.as_str())
            .and_then(tree::parse_clock_ticks),
        _ => None, // unix-timestamp clocks aren't supported by our tick-based clock
    }
}

fn handle_query(daemon: &Daemon, args: &[Value]) -> Value {
    let path = match args.get(1).and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return err_resp("query requires a path argument"),
    };
    let query_obj = match args.get(2) {
        Some(v) => v,
        None => return err_resp("query requires a query object"),
    };

    let root = match paths::canonicalize(path) {
        Ok(r) => r,
        Err(e) => return err_resp(format!("failed to resolve {path}: {e}")),
    };
    let state = match daemon.watch(root) {
        Ok(s) => s,
        Err(e) => return err_resp(e.to_string()),
    };

    let sync_timeout = query_obj
        .get("sync_timeout")
        .and_then(|v| v.as_i64())
        .unwrap_or(1000)
        .max(0) as u64;
    if sync_timeout > 0 {
        cookie::sync(&state, Duration::from_millis(sync_timeout));
    }

    let since_ticks = query_obj.get("since").and_then(since_ticks_from);

    let expr = match query_obj.get("expression") {
        Some(e) => match Expr::parse(e) {
            Ok(e) => Some(e),
            Err(msg) => return err_resp(format!("bad expression: {msg}")),
        },
        None => None,
    };

    let fields: Vec<&str> = query_obj
        .get("fields")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
        .filter(|v: &Vec<&str>| !v.is_empty())
        .unwrap_or_else(|| vec!["name"]);

    let tree_guard = state.tree.lock().unwrap();
    let clock_now = tree_guard.clock.format(tree_guard.clock.now());
    let is_fresh_instance = since_ticks.is_none();

    let candidates: Vec<_> = match since_ticks {
        Some(ticks) => tree_guard.since(ticks),
        None => tree_guard.files.values().collect(),
    };

    let mut files = Vec::new();
    for f in candidates {
        if tree::is_cookie(&f.name) {
            continue;
        }
        if let Some(expr) = &expr {
            if !expr.eval(f) {
                continue;
            }
        }
        let is_new = match since_ticks {
            Some(ticks) => f.first_tick > ticks,
            None => true,
        };
        files.push(file_to_value(f, &fields, is_new));
    }

    Value::obj()
        .set("version", VERSION)
        .set("clock", clock_now)
        .set("is_fresh_instance", is_fresh_instance)
        .set("files", Value::Array(files))
        .build()
}

fn file_to_value(f: &tree::FileInfo, fields: &[&str], is_new: bool) -> Value {
    let mut pairs = Vec::new();
    for field in fields {
        let v = match *field {
            "name" => Value::Str(f.name.clone()),
            "exists" => Value::Bool(f.exists),
            "new" => Value::Bool(is_new),
            "size" => Value::Int(f.size as i64),
            "mtime" => Value::Int(f.mtime),
            "mode" => Value::Int(f.mode as i64),
            "type" => Value::Str(f.kind.code().to_string()),
            _ => Value::Null,
        };
        pairs.push((field.to_string(), v));
    }
    Value::Object(pairs)
}
