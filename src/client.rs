//! CLI client: connects to (or spawns) the background daemon over a UNIX
//! socket, sends a single request, and prints the response -- mirroring how
//! the real `watchman` binary doubles as both client and server (see
//! docs/watchman-cpp.md and https://facebook.github.io/watchman/docs/cli-options).
//!
//! Real-world clients such as buck2's `watchman_client` crate only ever
//! shell out to this CLI once, to run:
//!
//!     watchman --output-encoding bser-v2 get-sockname
//!
//! in order to discover (and lazily spawn) the daemon; everything else is
//! spoken directly over the UNIX socket using BSER. This module therefore
//! focuses on faithfully supporting the small set of global flags used for
//! that handshake, while still being usable interactively for the other
//! commands.

use crate::bser::PduVersion;
use crate::json;
use crate::paths;
use crate::protocol::{self, Framing};
use crate::value::Value;
use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::time::Duration;

#[derive(Clone, Copy, PartialEq)]
pub enum Encoding {
    Json,
    Bser,
    BserV2,
}

impl Encoding {
    fn parse(s: &str) -> Option<Encoding> {
        match s {
            "json" => Some(Encoding::Json),
            "bser" => Some(Encoding::Bser),
            "bser-v2" => Some(Encoding::BserV2),
            _ => None,
        }
    }
}

#[derive(Default)]
struct CliOptions {
    output_encoding: Option<Encoding>,
    sockname: Option<String>,
    no_spawn: bool,
    /// Options we accept for CLI-compatibility but don't need to act on.
    _no_pretty: bool,
    _persistent: bool,
    _server_encoding: Option<Encoding>,
}

/// Split `argv` into global options and the remaining `<command> [args...]`,
/// following the flag surface documented at
/// <https://facebook.github.io/watchman/docs/cli-options>.
fn parse_global_opts(argv: &[String]) -> (CliOptions, Vec<String>) {
    let mut opts = CliOptions::default();
    let mut i = 0;
    while i < argv.len() {
        let arg = argv[i].as_str();
        let consume_value = |i: &mut usize, inline: Option<&str>| -> Option<String> {
            if let Some(v) = inline {
                return Some(v.to_string());
            }
            *i += 1;
            argv.get(*i).cloned()
        };
        match arg {
            _ if arg == "--output-encoding" || arg.starts_with("--output-encoding=") => {
                let inline = arg.strip_prefix("--output-encoding=");
                if let Some(v) = consume_value(&mut i, inline) {
                    opts.output_encoding = Encoding::parse(&v);
                }
            }
            _ if arg == "--server-encoding" || arg.starts_with("--server-encoding=") => {
                let inline = arg.strip_prefix("--server-encoding=");
                if let Some(v) = consume_value(&mut i, inline) {
                    opts._server_encoding = Encoding::parse(&v);
                }
            }
            _ if arg == "-U" || arg == "--sockname" || arg.starts_with("--sockname=") => {
                let inline = arg.strip_prefix("--sockname=");
                opts.sockname = consume_value(&mut i, inline);
            }
            "--no-pretty" => opts._no_pretty = true,
            "-p" | "--persistent" => opts._persistent = true,
            "--no-spawn" => opts.no_spawn = true,
            "--no-local" => {}
            "-j" | "--json-command" => opts._server_encoding = Some(Encoding::Json),
            // Server-only options: recognized for CLI compatibility but only
            // matter when starting the daemon, which we do transparently.
            "-f" | "--foreground" | "-n" | "--no-save-state" | "--inetd" => {}
            _ if arg == "--statefile"
                || arg == "-o"
                || arg == "--logfile"
                || arg.starts_with("--statefile=")
                || arg.starts_with("--logfile=") =>
            {
                let inline = arg.split_once('=').map(|(_, v)| v.to_string());
                let _ = consume_value(&mut i, inline.as_deref());
            }
            _ if arg == "--log-level" || arg.starts_with("--log-level=") => {
                let inline = arg.strip_prefix("--log-level=");
                let _ = consume_value(&mut i, inline);
            }
            _ => {
                // First non-flag token is the command; stop parsing options.
                return (opts, argv[i..].to_vec());
            }
        }
        i += 1;
    }
    (opts, Vec::new())
}

fn sock_path(opts: &CliOptions) -> std::path::PathBuf {
    match &opts.sockname {
        Some(p) => std::path::PathBuf::from(p),
        None => paths::sock_path(),
    }
}

fn try_connect(opts: &CliOptions) -> Option<UnixStream> {
    UnixStream::connect(sock_path(opts)).ok()
}

/// Ensure the daemon is running, spawning it (detached, re-execing ourselves
/// with a hidden subcommand) if we can't connect.
fn ensure_daemon(opts: &CliOptions) -> std::io::Result<UnixStream> {
    if let Some(s) = try_connect(opts) {
        return Ok(s);
    }
    if opts.no_spawn {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "daemon is not running and --no-spawn was given",
        ));
    }

    std::fs::create_dir_all(paths::state_dir())?;
    let exe = std::env::current_exe()?;
    let mut spawn_cmd = std::process::Command::new(exe);
    spawn_cmd.arg("--foreground");
    if let Some(sockname) = &opts.sockname {
        spawn_cmd.arg("--sockname").arg(sockname);
    }
    spawn_cmd
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;

    // Poll briefly for the socket to come up.
    for _ in 0..100 {
        std::thread::sleep(Duration::from_millis(50));
        if let Some(s) = try_connect(opts) {
            return Ok(s);
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "daemon did not start in time",
    ))
}

/// Send a request to the daemon (always using JSON framing internally, which
/// the daemon understands regardless of what encoding the *caller* wants
/// printed) and return its response value.
fn send_request(opts: &CliOptions, req: Value) -> std::io::Result<Value> {
    let stream = ensure_daemon(opts)?;
    let mut writer = stream.try_clone()?;
    protocol::write_response(&mut writer, &req, &Framing::Json)?;
    let mut reader = BufReader::new(stream);
    match protocol::read_request(&mut reader)? {
        Some((v, _)) => Ok(v),
        None => Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "no response from daemon",
        )),
    }
}

fn print_response(resp: &Value, encoding: Encoding) {
    use std::io::Write;
    match encoding {
        Encoding::Json => println!("{}", json::encode(resp)),
        Encoding::Bser => {
            let bytes = crate::bser::encode(resp, PduVersion::V1);
            let _ = std::io::stdout().write_all(&bytes);
        }
        Encoding::BserV2 => {
            let bytes = crate::bser::encode(resp, PduVersion::V2);
            let _ = std::io::stdout().write_all(&bytes);
        }
    }
}

pub fn run_command(argv: &[String]) -> i32 {
    let (opts, rest) = parse_global_opts(argv);
    let encoding = opts.output_encoding.unwrap_or(Encoding::Json);

    if rest.is_empty() {
        eprintln!("usage: watchman [options] <command> [args...]");
        return 1;
    }

    let cmd = rest[0].as_str();
    let mut req_items = vec![Value::Str(cmd.to_string())];

    match cmd {
        "version" | "get-sockname" | "shutdown-server" | "list-command" => {}
        "watch" | "watch-project" | "clock" | "watch-del" | "watch-list" => {
            if let Some(path) = rest.get(1) {
                req_items.push(Value::Str(path.clone()));
            }
        }
        "query" => {
            let path = match rest.get(1) {
                Some(p) => p.clone(),
                None => {
                    eprintln!("usage: watchman query <path> <query-json>");
                    return 1;
                }
            };
            let query_json = match rest.get(2) {
                Some(q) => q.clone(),
                None => {
                    eprintln!("usage: watchman query <path> <query-json>");
                    return 1;
                }
            };
            let query_val = match json::decode(&query_json) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("invalid query JSON: {e}");
                    return 1;
                }
            };
            req_items.push(Value::Str(path));
            req_items.push(query_val);
        }
        _ => {
            // Generic fallback: pass remaining args through as strings.
            for a in &rest[1..] {
                req_items.push(Value::Str(a.clone()));
            }
        }
    }

    match send_request(&opts, Value::Array(req_items)) {
        Ok(resp) => {
            print_response(&resp, encoding);
            if resp.get("error").is_some() { 1 } else { 0 }
        }
        Err(e) => {
            eprintln!("error talking to watchman-rs daemon: {e}");
            1
        }
    }
}
