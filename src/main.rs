mod bser;
mod client;
mod config;
mod cookie;
mod daemon;
#[cfg(target_os = "linux")]
mod fanotify;
#[cfg(target_os = "macos")]
mod fsevents;
mod ipc;
mod json;
#[cfg(target_os = "linux")]
mod linux_watcher;
mod paths;
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
mod poll_watcher;
mod protocol;
mod query;
mod tree;
mod value;
mod watcher;
#[cfg(windows)]
mod windows_watcher;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.first().map(|s| s.as_str()) == Some("--foreground") {
        // Internal-only entry point used when the CLI spawns its own daemon.
        // Optionally followed by `--sockname <path>` to honor a `-U`/
        // `--sockname` override requested by the spawning client.
        let sock_path = match args.get(1).map(|s| s.as_str()) {
            Some("--sockname") => args
                .get(2)
                .map(std::path::PathBuf::from)
                .unwrap_or_else(paths::sock_path),
            _ => paths::sock_path(),
        };
        if let Err(e) = daemon::run_foreground(sock_path) {
            eprintln!("watchman-rs daemon error: {e}");
            std::process::exit(1);
        }
        return;
    }

    std::process::exit(client::run_command(&args));
}
