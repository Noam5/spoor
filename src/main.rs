//! spoor — a privileged file-index daemon for Linux.
//!
//! Design notes, earned the hard way:
//!  * The index lives in the daemon, not in a GUI. Closing a window cannot
//!    invalidate it.
//!  * One FAN_MARK_FILESYSTEM mark covers a whole mount, including directories
//!    created later. Per-directory marking is what makes unprivileged indexers
//!    miss new trees.
//!  * FAN_UNLIMITED_QUEUE means events are never silently dropped; if an
//!    overflow ever is reported, we say so loudly rather than going quietly
//!    stale.

mod gui;
mod index;
mod ipc;
mod krunner;
mod persist;
mod query;
mod scan;
mod watch;

use index::{Index, Kind, SearchOpts};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

const DEFAULT_SOCK: &str = "/run/spoor.sock";
const DEFAULT_STATE: &str = "/var/lib/spoor/index.bin";

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(|s| s.as_str()).unwrap_or("help");

    match cmd {
        "daemon" => {
            let root = arg_value(&args, "--root").unwrap_or_else(|| "/home".to_string());
            let sock = arg_value(&args, "--socket").unwrap_or_else(|| DEFAULT_SOCK.to_string());
            let duration: u64 = arg_value(&args, "--duration")
                .and_then(|d| d.parse().ok())
                .unwrap_or(0);
            let watch_enabled = !args.iter().any(|a| a == "--no-watch");
            let state = arg_value(&args, "--state").unwrap_or_else(|| DEFAULT_STATE.to_string());
            let save_interval: u64 = arg_value(&args, "--save-interval")
                .and_then(|d| d.parse().ok())
                .unwrap_or(60);
            let rescan = arg_values(&args, "--rescan");
            let rescan_interval: u64 = arg_value(&args, "--rescan-interval")
                .and_then(|d| d.parse().ok())
                .unwrap_or(900);
            let reconcile_interval: u64 = arg_value(&args, "--reconcile-interval")
                .and_then(|d| d.parse().ok())
                .unwrap_or(86_400);
            daemon(
                &root,
                &sock,
                duration,
                watch_enabled,
                &state,
                save_interval,
                rescan,
                rescan_interval,
                reconcile_interval,
            );
        }
        "query" => {
            let sock = arg_value(&args, "--socket").unwrap_or_else(|| DEFAULT_SOCK.to_string());
            let limit: usize = arg_value(&args, "--limit")
                .and_then(|v| v.parse().ok())
                .unwrap_or(100);
            let opts = SearchOpts {
                match_case: has_flag(&args, "--case"),
                regex: has_flag(&args, "--regex"),
                in_path: has_flag(&args, "--path"),
                kind: if has_flag(&args, "--files") {
                    Kind::Files
                } else if has_flag(&args, "--folders") {
                    Kind::Folders
                } else {
                    Kind::All
                },
                hide_hidden: has_flag(&args, "--no-hidden"),
            };
            let pattern = positionals(&args).join(" ");
            let null = has_flag(&args, "--null");
            match query::search(&sock, &pattern, limit, &opts) {
                Ok(hits) => {
                    // Raw bytes, as locate prints them: a name need not be UTF-8.
                    // --null separates with NUL, for names containing newlines.
                    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
                    for h in hits {
                        let _ = std::io::Write::write_all(&mut out, &h);
                        let _ =
                            std::io::Write::write_all(&mut out, if null { b"\0" } else { b"\n" });
                    }
                }
                Err(e) => {
                    eprintln!("spoor: {}", e);
                    std::process::exit(1);
                }
            }
        }
        "selftest" => {
            let root = arg_value(&args, "--root").unwrap_or_else(|| "/home".to_string());
            selftest(&root);
        }
        "gui" => {
            gui::run(arg_value(&args, "--socket"), DEFAULT_SOCK);
        }
        "krunner" => {
            let sock = arg_value(&args, "--socket").unwrap_or_else(|| DEFAULT_SOCK.to_string());
            if let Err(e) = krunner::serve(&sock) {
                eprintln!("spoor: krunner runner failed: {}", e);
                std::process::exit(1);
            }
        }
        "bench" => {
            // Measures server-side round-trip only: one process, one connection
            // per query, no process spawn or dynamic-linking cost in the number.
            let sock = arg_value(&args, "--socket").unwrap_or_else(|| DEFAULT_SOCK.to_string());
            let n: usize = arg_value(&args, "--n")
                .and_then(|v| v.parse().ok())
                .unwrap_or(20);
            let opts =
                SearchOpts::from_flags(&arg_value(&args, "--opts").unwrap_or_else(|| "-".into()));
            for pat in positionals(&args) {
                if let Err(e) = query::search(&sock, pat, 100, &opts) {
                    println!("  {:<24} error: {}", pat, e);
                    continue;
                }
                let t0 = Instant::now();
                let mut hits = 0;
                for _ in 0..n {
                    hits = query::search(&sock, pat, 100, &opts)
                        .map(|v| v.len())
                        .unwrap_or(0);
                }
                let per = t0.elapsed().as_secs_f64() * 1000.0 / n as f64;
                println!("  {:<24} {:7.2} ms   ({} hits)", pat, per, hits);
            }
        }
        "--version" | "-V" | "version" => println!("spoor {}", env!("CARGO_PKG_VERSION")),
        "stats" => {
            let sock = arg_value(&args, "--socket").unwrap_or_else(|| DEFAULT_SOCK.to_string());
            client(&sock, "STATS");
        }
        _ => {
            eprintln!("spoor — privileged file index\n");
            eprintln!("  spoor daemon [--root /home] [--socket PATH] [--state PATH]");
            eprintln!("                     [--save-interval SECS] [--duration SECS] [--no-watch]");
            eprintln!("                     [--rescan PATH]... [--rescan-interval SECS]");
            eprintln!(
                "                     [--reconcile-interval SECS]  (default 86400, 0 = never)"
            );
            eprintln!("  spoor query <pattern> [--limit N] [--case] [--regex] [--path]");
            eprintln!("                        [--files|--folders] [--no-hidden] [--null]");
            eprintln!("  spoor stats");
            eprintln!("  spoor --version");
            eprintln!("  spoor bench <pattern>… [--opts FLAGS] [--n N]  (server-side timing)");
            eprintln!("  spoor krunner            (KDE KRunner D-Bus runner)");
            eprintln!("  spoor gui [--socket PATH] (standalone window)");
            std::process::exit(2);
        }
    }
}

/// Flags that consume the following argument; any other "--x" is a switch.
const VALUE_FLAGS: &[&str] = &[
    "--socket",
    "--limit",
    "--n",
    "--root",
    "--state",
    "--save-interval",
    "--duration",
    "--opts",
    "--rescan",
    "--rescan-interval",
    "--reconcile-interval",
];

/// Every value of a repeatable flag.
fn arg_values(args: &[String], flag: &str) -> Vec<String> {
    args.windows(2)
        .filter(|w| w[0] == flag)
        .map(|w| w[1].clone())
        .collect()
}

/// Arguments after the subcommand that are neither flags nor flag values.
fn positionals(args: &[String]) -> Vec<&str> {
    let mut out = Vec::new();
    let mut it = args.iter().skip(2);
    while let Some(a) = it.next() {
        if VALUE_FLAGS.contains(&a.as_str()) {
            it.next();
        } else if !a.starts_with("--") {
            out.push(a.as_str());
        }
    }
    out
}

fn has_flag(args: &[String], flag: &str) -> bool {
    args.iter().skip(2).any(|a| a == flag)
}

fn arg_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1).cloned())
}

// One parameter per command-line flag, and one caller: a struct would only
// rename them.
#[allow(clippy::too_many_arguments)]
fn daemon(
    root: &str,
    sock: &str,
    duration_secs: u64,
    watch_enabled: bool,
    state_path: &str,
    save_interval: u64,
    rescan: Vec<String>,
    rescan_interval: u64,
    reconcile_interval: u64,
) {
    if watch_enabled && unsafe { libc::geteuid() } != 0 {
        eprintln!("spoor: must run as root (FAN_MARK_FILESYSTEM needs CAP_SYS_ADMIN)");
        eprintln!("spoor: pass --no-watch to serve a static index unprivileged");
        std::process::exit(1);
    }

    install_signal_handlers();

    // Watch before walking, so changes during the walk queue up rather than
    // being lost. FAN_UNLIMITED_QUEUE means the backlog cannot overflow.
    let mut watcher = if watch_enabled {
        match watch::Watcher::new(root) {
            Ok(w) => {
                eprintln!("spoor: watching {} (1 filesystem-wide mark)", root);
                Some(w)
            }
            Err(e) => {
                eprintln!("spoor: cannot watch {}: {}", root, e);
                std::process::exit(1);
            }
        }
    } else {
        eprintln!("spoor: --no-watch, index will be static");
        None
    };

    // Serve the previous snapshot immediately, so queries work during the
    // reconciliation scan instead of failing for the first 20 seconds.
    let index = match persist::load(state_path) {
        Ok(ix) => {
            eprintln!(
                "spoor: loaded snapshot {} ({} entries), serving while rescanning",
                state_path,
                ix.len()
            );
            Arc::new(RwLock::new(ix))
        }
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                eprintln!("spoor: snapshot unusable ({}), starting empty", e);
            }
            Arc::new(RwLock::new(Index::new()))
        }
    };

    let ipc_index = Arc::clone(&index);
    let sock_owned = sock.to_string();
    std::thread::spawn(move || {
        if let Err(e) = ipc::serve(&sock_owned, ipc_index) {
            eprintln!("spoor: ipc failed: {}", e);
        }
    });
    eprintln!("spoor: listening on {}", sock);

    // Reconciliation scan. ext4 has no change journal, so a restart cannot know
    // what changed while we were down; only a full walk can establish truth.
    let t0 = Instant::now();
    let mut fresh = Index::new();
    let stats = scan::scan(&mut fresh, root);
    eprintln!(
        "spoor: indexed {} files + {} dirs in {:.1}s ({} unreadable)",
        stats.files,
        stats.dirs,
        t0.elapsed().as_secs_f64(),
        stats.errors
    );
    let t1 = Instant::now();
    fresh.build_trigrams();
    let (distinct, postings) = fresh.trigram_stats();
    eprintln!(
        "spoor: trigram index built in {:.1}s ({} distinct, {} postings)",
        t1.elapsed().as_secs_f64(),
        distinct,
        postings
    );
    *index.write().unwrap() = fresh;
    // Last successful listing of each rescanned mount, so a reconciliation can
    // graft it into the fresh index instead of waiting for the next rescan.
    let listings: Listings = Arc::new(Mutex::new(HashMap::new()));

    // Mounts fanotify cannot watch are walked on a timer instead. Started only
    // now, after the swap, so the first graft lands in the live index rather
    // than in the snapshot being replaced.
    if !rescan.is_empty() {
        eprintln!(
            "spoor: will rescan {} every {}s",
            rescan.join(", "),
            rescan_interval
        );
        let rescan_index = Arc::clone(&index);
        let rescan_listings = Arc::clone(&listings);
        std::thread::spawn(move || {
            rescan_loop(rescan_index, rescan, rescan_interval, rescan_listings)
        });
    }
    if reconcile_interval > 0 {
        let rec_index = Arc::clone(&index);
        let rec_root = root.to_string();
        let rec_listings = Arc::clone(&listings);
        std::thread::spawn(move || {
            reconcile_loop(rec_index, rec_root, reconcile_interval, rec_listings)
        });
    }

    // Periodic snapshot, so a kill costs at most one interval.
    if save_interval > 0 {
        let save_index = Arc::clone(&index);
        let sp = state_path.to_string();
        std::thread::spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_secs(save_interval));
            if SHUTDOWN.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            let ix = save_index.read().unwrap();
            match persist::save(&ix, &sp) {
                Ok(n) => eprintln!("spoor: snapshot saved ({} slots)", n),
                Err(e) => eprintln!("spoor: snapshot failed: {}", e),
            }
        });
    }

    let deadline = if duration_secs > 0 {
        Some(Instant::now() + std::time::Duration::from_secs(duration_secs))
    } else {
        None
    };
    eprintln!("spoor: ready");

    loop {
        if SHUTDOWN.load(std::sync::atomic::Ordering::Relaxed) {
            eprintln!("spoor: signal received, shutting down");
            break;
        }
        if let Some(d) = deadline {
            if Instant::now() >= d {
                eprintln!("spoor: duration elapsed, exiting");
                break;
            }
        }
        match watcher.as_mut() {
            Some(w) => {
                if !w.wait(300) {
                    continue;
                }
                match w.read_events() {
                    Ok(events) => {
                        if events.is_empty() {
                            continue;
                        }
                        let mut ix = index.write().unwrap();
                        // Under the write lock, so a reconciliation swapping
                        // the index sees each batch either captured or not yet
                        // read -- never half of it.
                        if let Some(buf) = CAPTURE.lock().unwrap().as_mut() {
                            buf.extend(events.iter().cloned());
                        }
                        for ev in &events {
                            apply(&mut ix, ev);
                        }
                    }
                    Err(e) => {
                        if e.kind() == std::io::ErrorKind::Interrupted {
                            continue;
                        }
                        eprintln!("spoor: read error: {}", e);
                        break;
                    }
                }
            }
            None => std::thread::sleep(std::time::Duration::from_millis(200)),
        }
    }

    // Final save on the way out, in addition to (not instead of) the periodic one.
    let ix = index.read().unwrap();
    match persist::save(&ix, state_path) {
        Ok(n) => eprintln!("spoor: final snapshot saved ({} slots)", n),
        Err(e) => eprintln!("spoor: final snapshot failed: {}", e),
    }
    let _ = std::fs::remove_file(sock);
}

type Listings = Arc<Mutex<HashMap<String, Vec<(u32, Vec<u8>, bool)>>>>;

/// Events applied while a reconciliation walk runs, kept for replay onto the
/// fresh index. Pushed and taken only under the index write lock, so every
/// event is either replayed or applied after the swap: never lost, never twice.
static CAPTURE: Mutex<Option<Vec<watch::Event>>> = Mutex::new(None);

/// Rebuild the index from a fresh walk on a timer. This compacts the arena
/// (dead entries otherwise accumulate for as long as the daemon runs) and
/// corrects drift from any event that could not be applied. Changes made
/// during the walk are captured and replayed before the new index is swapped
/// in. Memory peaks at two indexes for the length of the walk.
fn reconcile_loop(index: Arc<RwLock<Index>>, root: String, interval: u64, listings: Listings) {
    let stopping = || SHUTDOWN.load(std::sync::atomic::Ordering::Relaxed);
    loop {
        for _ in 0..interval {
            if stopping() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
        let t0 = Instant::now();
        *CAPTURE.lock().unwrap() = Some(Vec::new());
        let mut fresh = Index::new();
        let st = scan::scan(&mut fresh, &root);
        for (path, list) in listings.lock().unwrap().iter() {
            if let Some(mount) = fresh.resolve_path(path) {
                fresh.graft(mount, list);
            }
        }
        fresh.build_trigrams();
        let (before, after, replayed) = {
            let mut ix = index.write().unwrap();
            let events = CAPTURE.lock().unwrap().take().unwrap_or_default();
            for ev in &events {
                apply(&mut fresh, ev);
            }
            let before = ix.capacity_used();
            let after = fresh.capacity_used();
            *ix = fresh;
            (before, after, events.len())
        };
        eprintln!(
            "spoor: reconciled in {:.1}s: {} files + {} dirs, arena {} -> {} slots, {} events replayed",
            t0.elapsed().as_secs_f64(),
            st.files,
            st.dirs,
            before,
            after,
            replayed
        );
    }
}

/// Keeps unwatchable mounts current by walking them on a timer. A mount that
/// is not available yet (rclone has not mounted it, or it refuses root) is
/// retried every minute instead of waiting out the whole interval -- at boot,
/// spoor usually starts before the network mount does.
fn rescan_loop(index: Arc<RwLock<Index>>, paths: Vec<String>, interval: u64, listings: Listings) {
    const RETRY: u64 = 60;
    let stopping = || SHUTDOWN.load(std::sync::atomic::Ordering::Relaxed);
    loop {
        let mut all_ok = true;
        for p in &paths {
            if stopping() {
                return;
            }
            let t0 = Instant::now();
            match scan::walk_foreign(p) {
                scan::ForeignWalk::Unavailable(why) => {
                    all_ok = false;
                    eprintln!(
                        "spoor: rescan {}: unavailable ({}), retrying in {}s",
                        p, why, RETRY
                    );
                }
                scan::ForeignWalk::Listed(list, errors) => {
                    let walk_s = t0.elapsed().as_secs_f64();
                    let t1 = Instant::now();
                    let mut ix = index.write().unwrap();
                    match ix.resolve_path(p) {
                        None => {
                            all_ok = false;
                            eprintln!("spoor: rescan {}: not inside the indexed tree", p);
                        }
                        Some(mount) => {
                            let st = ix.graft(mount, &list);
                            drop(ix);
                            listings.lock().unwrap().insert(p.clone(), list);
                            eprintln!(
                                "spoor: rescanned {}: {} entries (+{} -{}), walked in {:.1}s, merged in {:.0}ms{}",
                                p,
                                st.total,
                                st.added,
                                st.removed,
                                walk_s,
                                t1.elapsed().as_secs_f64() * 1000.0,
                                if errors > 0 {
                                    format!(", {} unreadable", errors)
                                } else {
                                    String::new()
                                }
                            );
                        }
                    }
                }
            }
        }
        let wait = if all_ok {
            interval
        } else {
            RETRY.min(interval)
        };
        for _ in 0..wait {
            if stopping() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }
}

static SHUTDOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

extern "C" fn on_signal(_sig: i32) {
    SHUTDOWN.store(true, std::sync::atomic::Ordering::Relaxed);
}

fn install_signal_handlers() {
    unsafe {
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
    }
}

static UNRESOLVED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn apply(ix: &mut Index, ev: &watch::Event) {
    if ev.mask & watch::FAN_Q_OVERFLOW != 0 {
        eprintln!("spoor: FAN_Q_OVERFLOW — index may be incomplete, rescan needed");
        return;
    }

    // Prefer the inode map; fall back to resolving the path, which covers
    // directories created since the walk.
    let parent = ix
        .dir_by_ino(ev.parent_ino)
        .or_else(|| ev.parent_path.as_deref().and_then(|p| ix.resolve_path(p)));

    let parent = match parent {
        Some(p) => p,
        None => {
            UNRESOLVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return;
        }
    };

    if ev.is_create() {
        // Reconstruct the path from the index rather than from the event, so we
        // do not depend on resolving handles back to paths.
        let mut full = ix.path_bytes(parent);
        if full.last() != Some(&b'/') {
            full.push(b'/');
        }
        full.extend_from_slice(&ev.name);
        let full = std::path::PathBuf::from(
            <std::ffi::OsString as std::os::unix::ffi::OsStringExt>::from_vec(full),
        );
        let ino = std::fs::symlink_metadata(&full)
            .map(|m| std::os::unix::fs::MetadataExt::ino(&m))
            .unwrap_or(0);

        let id = ix.add(parent, &ev.name, ev.is_dir(), ino);
        if ev.is_dir() {
            scan::scan_subtree(ix, id, &full, 0);
        }
    } else if ev.is_delete() {
        if let Some(id) = ix.find_child(parent, &ev.name) {
            ix.remove(id);
        }
    }
}

/// End-to-end proof: watch, scan, then create a deep NEW directory tree and
/// confirm the file inside it becomes searchable. This is exactly the case
/// FSearch misses.
fn selftest(root: &str) {
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("selftest: must run as root");
        std::process::exit(1);
    }
    let mut watcher = match watch::Watcher::new(root) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("selftest: watch failed: {}", e);
            std::process::exit(1);
        }
    };
    println!("[1] fanotify: ONE filesystem-wide mark on {}", root);

    let t0 = Instant::now();
    let mut ix = Index::new();
    let st = scan::scan(&mut ix, root);
    println!(
        "[2] initial walk: {} files + {} dirs in {:.1}s ({} unreadable)",
        st.files,
        st.dirs,
        t0.elapsed().as_secs_f64(),
        st.errors
    );

    let marker = format!("spoor-selftest-{}", unsafe { libc::getpid() });
    let base = format!("{}/.spoor-selftest", root.trim_end_matches('/'));
    let base = base.as_str();
    let deep = format!("{}/a/b/c", base);
    let _ = std::fs::remove_dir_all(base);
    std::fs::create_dir_all(&deep).ok();
    let target = format!("{}/{}.txt", deep, marker);
    std::fs::write(&target, b"probe").ok();
    println!("[3] created 4 levels of NEW dirs + {}", target);

    let deadline = Instant::now() + std::time::Duration::from_secs(8);
    let mut applied = 0usize;
    while Instant::now() < deadline {
        if !watcher.wait(300) {
            continue;
        }
        if let Ok(events) = watcher.read_events() {
            for ev in &events {
                apply(&mut ix, ev);
                applied += 1;
            }
        }
    }
    let unresolved = UNRESOLVED.load(std::sync::atomic::Ordering::Relaxed);
    println!(
        "[4] applied {} live events ({} could not resolve a parent, {} handle failures)",
        applied,
        unresolved,
        watch::HANDLE_FAILURES.load(std::sync::atomic::Ordering::Relaxed)
    );

    let hits = ix.search(&marker, 10);
    let lower = ix.search(&marker.to_uppercase(), 10);
    println!("[5] search(\"{}\") -> {:?}", marker, hits);
    println!(
        "[6] case-insensitive (uppercased query) -> {} hit(s)",
        lower.len()
    );

    let _ = std::fs::remove_dir_all(base);
    if hits.len() == 1 && lower.len() == 1 {
        println!("\nPASS: new nested tree indexed live, case-insensitive match works");
    } else {
        println!("\nFAIL");
        std::process::exit(1);
    }
}

fn client(sock: &str, request: &str) {
    use std::io::{BufRead, BufReader, Write};
    let mut stream = match std::os::unix::net::UnixStream::connect(sock) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("spoor: cannot reach daemon at {}: {}", sock, e);
            std::process::exit(1);
        }
    };
    if writeln!(stream, "{}", request).is_err() {
        eprintln!("spoor: write failed");
        std::process::exit(1);
    }
    let reader = BufReader::new(stream);
    for line in reader.lines().map_while(Result::ok) {
        if line.is_empty() {
            break;
        }
        println!("{}", line);
    }
}
