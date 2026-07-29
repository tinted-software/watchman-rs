Under the hood on a Linux host, **Facebook’s Watchman** does a lot more than just call `inotify` in a loop.

---

## 1. How Watchman Works Under the Hood on Linux

Watchman was created because raw Linux `inotify` fails when scaling to giant repositories with millions of files and rapid git operations (like branch switching).

### A. Client-Daemon Architecture & IPC

* **Persistent Daemon Process:** Watchman operates as a single background service (per user or root). When you run `watchman watch <path>`, the CLI communicates with the background daemon over a local **UNIX domain socket** using **BSER** (Binary Serialization) or JSON.
* **Single Watch Instance:** Instead of each tool (Buck2, ESLint, Mercurial/Sapling, Jest) spinning up its own watching process and consuming system inotify instances, all tools share a single persistent Watchman daemon instance.

### B. Inotify Management & Directory Crawling

* **Inotify Directory Watched (Not Files):** Linux’s `inotify` cannot watch subdirectories recursively with a single system call; `IN_WATCH_MASK` must be added manually to *every single directory* in the tree.
* **Crawl & Dynamic Watch Attachment:** When Watchman starts watching a root:
1. It performs a **full directory crawl** (using `fts` / `readdir`) to discover all subdirectories and builds an in-memory representation of the file tree.
2. It calls `inotify_add_watch(fd, dir_path, flags)` for **every directory**.
3. When `IN_CREATE` events occur for new directories, Watchman automatically adds new `inotify` watches recursively.
4. When `IN_DELETE` or `IN_IGNORED` events fire (e.g. `rm -rf`), Watchman cleans up its watch handles.



### C. Cookie Files & Settling (Synchronization)

* **The Cookie Mechanism:** `inotify` events are asynchronous, meaning events might still be sitting in kernel queues when a client queries Watchman. To ensure "read-your-writes" consistency:
1. Watchman writes a temporary **cookie file** (`.watchman-cookie-<hostname>-<pid>-<seq>`) into the root directory.
2. It then blocks client query responses until it receives the `inotify` event acknowledging that the cookie file was created/deleted.
3. Once the cookie event passes through the queue, Watchman knows its in-memory tree is up-to-date with disk state.


* **Settling Delay:** Watchman queues events and waits for the filesystem to "settle" (quiet period) before dispatching updates to triggers or subscriptions to prevent intermediate build artifacts from triggering cascade builds.

### D. In-Memory AST & Clock-Based Query Engine

* **In-Memory File Tree:** Watchman maintains an in-memory graph of the filesystem metadata (stat info: mtime, size, mode, sha1/content hashes).
* **Logical Clocks (`c:12345:67`):** Every mutation bumps a global logical clock ticks counter.
* **JSON Expression Evaluator:** Clients run queries like `{"since": "c:12345:67", "expression": ["match", "*.rs"]}`. Watchman evaluates this query entirely **in-memory** across its tree without hitting the disk, returning matching paths in milliseconds.

---

## 2. Why Buck2’s Internal Inotify Fails (and Watchman Works)

When you run Buck2 without Watchman, Buck2 uses its own internal file-watching loop (often powered by the `notify` Rust crate using `inotify` underneath).

1. **`max_user_watches` Limit:** The Linux kernel defaults `fs.inotify.max_user_watches` to a low value (historically 8,192). Large repos with >100k directories crash with `ENOSPC` (No space left on device) unless you `sysctl` bump it.
2. **`max_queued_events` Overflow:** During massive git checkouts (e.g., changing branches with 50,000 changed files), kernel queues fill up, leading to `IN_Q_OVERFLOW`.
* *Buck2 internal:* Drop events or throw errors, forcing full workspace rescans.
* *Watchman:* Detects `IN_Q_OVERFLOW`, marks the watch state as dirty, and triggers a lightweight internal recrawl to reconcile state.


3. **Shared State:** If you run multiple commands, Buck2 internal watcher starts and stops or competes with other dev tools. Watchman shares watches across processes.

---

## 3. Architecture for Building a Rust Alternative

If your goal is a lightweight, single `cargo build`-able binary that impersoantes Watchman’s CLI/IPC for Buck2, here is the roadmap:

### Core Components Needed

```
┌─────────────────────────────────────────┐
│        Buck2 / CLI Client               │
└───────────────────┬─────────────────────┘
                    │ Unix Domain Socket (BSER or JSON)
┌───────────────────▼─────────────────────┐
│             IPC Daemon                  │
├─────────────────────────────────────────┤
│  • Cookie Synchronization System        │
│  • In-Memory Directory Tree Index       │
│  • BSER / Watchman JSON Query Engine    │
├─────────────────────────────────────────┤
│    Linux Filesystem Layer (choose 1)    │
│  A) inotify (legacy) + tokio-epoll      │
│  B) fanotify (Linux 5.1+ / root/caps)   │
└─────────────────────────────────────────┘

```

### A. IPC Protocol (BSER Parser)

Buck2 speaks Watchman’s native binary serialization protocol, **BSER**:

* You must implement a BSER encoder/decoder (or use serde with BSER, though BSER has variable-length integer wire formats).
* Alternatively, handle Watchman’s UNIX socket text JSON fallback if Buck2 supports JSON mode, but BSER is default for speed.

### B. Linux Kernel Watch Backend Choice

You have two choices for watching files on Linux in Rust:

1. **`inotify` (standard user space):**
* Use crates like `inotify` or `notify` (with raw inotify feature).
* You **must** recursively add watches to directories with `inotify_add_watch`.
* Handle `IN_Q_OVERFLOW` by triggering an asynchronous directory recrawl.


2. **`fanotify` (`FAN_REPORT_DFID_NAME` - Linux 5.9+):**
* *Why:* Modern Linux kernel supports `fanotify` at the filesystem/mount level (`FAN_MARK_FILESYSTEM`), allowing you to watch an **entire directory tree with a single file descriptor** without hitting `max_user_watches` limits!
* *Caveat:* Requires `CAP_SYS_ADMIN` capability or root unless configured via sysctl (`fs.fanotify.max_queued_events`), but avoids registering 500,000 separate watch descriptors.



### C. Key Watchman Commands to Implement for Buck2

Buck2 only relies on a subset of the Watchman RPC interface:

* `version`: Returns capabilities (e.g. `{"version": "2026.01.01", "capabilities": {...}}`).
* `watch-project`: Resolves the repository root and `.watchmanconfig`.
* `clock`: Returns current state token (`c:<timestamp>:<pid>:<seq>`).
* `query`: Accepts JSON query payload (`since`, `expression` matching globs like `*.rs`, `generator`, `fields`: `["name", "exists", "mtime", "mode"]`).
* `flush-subscriptions` / `subscribe`: For live updates.

### D. Cookie Files logic

When Buck2 sends a query with `"sync_timeout": 5000`:

1. Drop a `.watchman-cookie-<random>` file in the root directory.
2. Wait for your filesystem monitor thread to yield an event for that cookie file name.
3. Once seen, respond to the query with the changes recorded up to that clock.
