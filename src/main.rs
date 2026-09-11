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

mod catalog;
mod config;
mod gui;
mod index;
mod ipc;
mod krunner;
mod persist;
mod query;
mod scan;
mod watch;

use catalog::{Catalog, Exclude, Part};
use index::{Index, Kind, SearchOpts};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

const DEFAULT_SOCK: &str = "/run/spoor.sock";
const DEFAULT_STATE: &str = "/var/lib/spoor/index.bin";

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(|s| s.as_str()).unwrap_or("help");

    match cmd {
        "daemon" => {
            // The settings file first (the service passes --config); flags
            // add to it, for tests and one-off runs.
            let mut cfg = match arg_value(&args, "--config") {
                Some(p) => match config::Config::load(&p) {
                    Ok(Some(c)) => c,
                    Ok(None) => config::Config::default(),
                    Err(e) => {
                        eprintln!("spoor: {}", e);
                        std::process::exit(1);
                    }
                },
                None => config::Config {
                    roots: Vec::new(),
                    ..config::Config::default()
                },
            };
            cfg.roots.extend(arg_values(&args, "--root"));
            cfg.exclude.extend(arg_values(&args, "--exclude"));
            cfg.rescan.extend(arg_values(&args, "--rescan"));
            if let Some(v) = arg_value(&args, "--rescan-interval").and_then(|d| d.parse().ok()) {
                cfg.rescan_interval = v;
            }
            if cfg.roots.is_empty() {
                cfg.roots.push("/home".to_string());
            }
            for note in cfg.normalize() {
                eprintln!("spoor: settings: {}", note);
            }
            // A folder on a disk that is not plugged in must not keep the
            // rest from being searchable.
            cfg.roots.retain(|r| {
                let ok = std::path::Path::new(r).is_dir();
                if !ok {
                    eprintln!(
                        "spoor: {} is not available; not indexing it until restart",
                        r
                    );
                }
                ok
            });
            if cfg.roots.is_empty() {
                eprintln!("spoor: none of the folders to index is available");
                std::process::exit(1);
            }
            daemon(DaemonOpts {
                sock: arg_value(&args, "--socket").unwrap_or_else(|| DEFAULT_SOCK.to_string()),
                duration_secs: arg_value(&args, "--duration")
                    .and_then(|d| d.parse().ok())
                    .unwrap_or(0),
                watch_enabled: !args.iter().any(|a| a == "--no-watch"),
                state_path: arg_value(&args, "--state")
                    .unwrap_or_else(|| DEFAULT_STATE.to_string()),
                save_interval: arg_value(&args, "--save-interval")
                    .and_then(|d| d.parse().ok())
                    .unwrap_or(60),
                reconcile_interval: arg_value(&args, "--reconcile-interval")
                    .and_then(|d| d.parse().ok())
                    .unwrap_or(86_400),
                config: cfg,
            });
        }
        "configure" => {
            // Run as root through pkexec by the window's Preferences: validate
            // the settings, install them, and restart the daemon to apply them.
            if unsafe { libc::geteuid() } != 0 {
                eprintln!("spoor: configure must run as root (the Preferences window uses pkexec)");
                std::process::exit(1);
            }
            let src = positionals(&args)
                .first()
                .copied()
                .unwrap_or("-")
                .to_string();
            let dest =
                arg_value(&args, "--config").unwrap_or_else(|| config::DEFAULT_PATH.to_string());
            match config::read_input(&src).and_then(|text| config::install(&text, &dest)) {
                Ok(notes) => {
                    for n in notes {
                        println!("note: {}", n);
                    }
                    let restarted = std::process::Command::new("systemctl")
                        .args(["try-restart", "spoor.service"])
                        .status()
                        .map(|st| st.success())
                        .unwrap_or(false);
                    if restarted {
                        println!("saved {}; spoor is re-indexing", dest);
                    } else {
                        println!("saved {}; restart spoor.service to apply it", dest);
                    }
                }
                Err(e) => {
                    eprintln!("spoor: {}", e);
                    std::process::exit(1);
                }
            }
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
            eprintln!("  spoor daemon [--config /etc/spoor/spoor.conf] [--root DIR]... [--exclude DIR]...");
            eprintln!("                     [--socket PATH] [--state PATH]");
            eprintln!("                     [--save-interval SECS] [--duration SECS] [--no-watch]");
            eprintln!("                     [--rescan PATH]... [--rescan-interval SECS]");
            eprintln!(
                "                     [--reconcile-interval SECS]  (default 86400, 0 = never)"
            );
            eprintln!("  spoor query <pattern> [--limit N] [--case] [--regex] [--path]");
            eprintln!("                        [--files|--folders] [--no-hidden] [--null]");
            eprintln!("  spoor configure [FILE|-]  (root: install index settings and restart)");
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
    "--config",
    "--exclude",
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

/// Daemon settings: /etc/spoor/spoor.conf plus command-line flags.
struct DaemonOpts {
    sock: String,
    duration_secs: u64,
    watch_enabled: bool,
    state_path: String,
    save_interval: u64,
    reconcile_interval: u64,
    config: config::Config,
}

fn daemon(o: DaemonOpts) {
    if o.watch_enabled && unsafe { libc::geteuid() } != 0 {
        eprintln!("spoor: must run as root (FAN_MARK_FILESYSTEM needs CAP_SYS_ADMIN)");
        eprintln!("spoor: pass --no-watch to serve a static index unprivileged");
        std::process::exit(1);
    }
    install_signal_handlers();
    let cfg = &o.config;
    eprintln!("spoor: indexing {}", cfg.roots.join(", "));
    if !cfg.exclude.is_empty() {
        eprintln!("spoor: excluding {}", cfg.exclude.join(", "));
    }

    // Watch before walking, so changes during the walk queue up rather than
    // being lost. FAN_UNLIMITED_QUEUE means the backlog cannot overflow. A
    // folder that cannot be watched (a filesystem fanotify does not support)
    // is still indexed, and refreshed by the reconciliation walk.
    let mut watchers: Vec<Option<watch::Watcher>> = Vec::new();
    for r in &cfg.roots {
        if !o.watch_enabled {
            watchers.push(None);
            continue;
        }
        match watch::Watcher::new(r) {
            Ok(w) => {
                eprintln!("spoor: watching {} (1 filesystem-wide mark)", r);
                watchers.push(Some(w));
            }
            Err(e) => {
                eprintln!(
                    "spoor: cannot watch {}: {}; only reconciliation will refresh it",
                    r, e
                );
                watchers.push(None);
            }
        }
    }
    if !o.watch_enabled {
        eprintln!("spoor: --no-watch, index will be static");
    }

    // Serve the previous snapshot immediately, so queries work during the walk
    // instead of failing for the first 20 seconds. Folders no longer
    // configured are dropped; new ones start empty.
    let mut previous: HashMap<String, Index> = match persist::load(&o.state_path) {
        Ok(parts) => {
            let n: usize = parts.iter().map(|(_, ix)| ix.len()).sum();
            eprintln!(
                "spoor: loaded snapshot {} ({} entries), serving while rescanning",
                o.state_path, n
            );
            parts.into_iter().collect()
        }
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                eprintln!("spoor: snapshot unusable ({}), starting empty", e);
            }
            HashMap::new()
        }
    };
    let parts = cfg
        .roots
        .iter()
        .map(|r| (r.clone(), previous.remove(r).unwrap_or_else(Index::new)))
        .collect();
    let cat = Arc::new(Catalog::new(parts, &cfg.exclude));

    let ipc_cat = Arc::clone(&cat);
    let sock_owned = o.sock.clone();
    std::thread::spawn(move || {
        if let Err(e) = ipc::serve(&sock_owned, ipc_cat) {
            eprintln!("spoor: ipc failed: {}", e);
        }
    });
    eprintln!("spoor: listening on {}", o.sock);

    // A full walk of every folder. ext4 has no change journal, so a restart
    // cannot know what changed while we were down; only a walk establishes
    // truth. Each folder is swapped in as soon as its own walk is done.
    for p in &cat.parts {
        let (fresh, st, took) = walk_part(p, &cat.exclude);
        let (distinct, postings) = fresh.trigram_stats();
        eprintln!(
            "spoor: indexed {}: {} files + {} dirs in {:.1}s ({} unreadable; {} trigrams, {} postings)",
            p.root,
            st.files,
            st.dirs,
            took.as_secs_f64(),
            st.errors,
            distinct,
            postings
        );
        *p.index.write().unwrap() = fresh;
    }

    // Mounts fanotify cannot watch are walked on a timer instead. Started only
    // now, after the swap, so the first graft lands in the live index rather
    // than in the snapshot being replaced.
    if !cfg.rescan.is_empty() {
        eprintln!(
            "spoor: will rescan {} every {}s",
            cfg.rescan.join(", "),
            cfg.rescan_interval
        );
        let (c, paths, every) = (Arc::clone(&cat), cfg.rescan.clone(), cfg.rescan_interval);
        std::thread::spawn(move || rescan_loop(c, paths, every));
    }
    if o.reconcile_interval > 0 {
        let (c, every) = (Arc::clone(&cat), o.reconcile_interval);
        std::thread::spawn(move || reconcile_loop(c, every));
    }
    // Periodic snapshot, so a kill costs at most one interval.
    if o.save_interval > 0 {
        let (c, sp, every) = (Arc::clone(&cat), o.state_path.clone(), o.save_interval);
        std::thread::spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_secs(every));
            if stopping() {
                return;
            }
            match save_snapshot(&c, &sp) {
                Ok(n) => eprintln!("spoor: snapshot saved ({} slots)", n),
                Err(e) => eprintln!("spoor: snapshot failed: {}", e),
            }
        });
    }
    for (i, w) in watchers.into_iter().enumerate() {
        if let Some(w) = w {
            let c = Arc::clone(&cat);
            std::thread::spawn(move || watch_loop(c, i, w));
        }
    }

    let deadline = (o.duration_secs > 0)
        .then(|| Instant::now() + std::time::Duration::from_secs(o.duration_secs));
    eprintln!("spoor: ready");
    loop {
        if stopping() {
            if FAILED.load(std::sync::atomic::Ordering::Relaxed) {
                eprintln!("spoor: stopping after a watch error");
            } else {
                eprintln!("spoor: signal received, shutting down");
            }
            break;
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            eprintln!("spoor: duration elapsed, exiting");
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    // Final save on the way out, in addition to (not instead of) the periodic one.
    match save_snapshot(&cat, &o.state_path) {
        Ok(n) => eprintln!("spoor: final snapshot saved ({} slots)", n),
        Err(e) => eprintln!("spoor: final snapshot failed: {}", e),
    }
    let _ = std::fs::remove_file(&o.sock);
    // A failure exit, so systemd's Restart=on-failure brings the watch back.
    if FAILED.load(std::sync::atomic::Ordering::Relaxed) {
        std::process::exit(1);
    }
}

fn stopping() -> bool {
    SHUTDOWN.load(std::sync::atomic::Ordering::Relaxed)
}

/// Every part, each under its own read lock for the length of the write.
fn save_snapshot(cat: &Catalog, path: &str) -> std::io::Result<usize> {
    let guards: Vec<_> = cat.parts.iter().map(|p| p.index.read().unwrap()).collect();
    let parts: Vec<(&str, &Index)> = cat
        .parts
        .iter()
        .zip(&guards)
        .map(|(p, g)| (p.root.as_str(), &**g))
        .collect();
    persist::save(&parts, path)
}

/// A freshly walked index of one folder, with the last complete listing of
/// each rescanned mount inside it grafted in: ready to swap in.
fn walk_part(p: &Part, exclude: &Exclude) -> (Index, scan::ScanStats, std::time::Duration) {
    let t0 = Instant::now();
    let mut fresh = Index::new();
    let st = scan::scan(&mut fresh, &p.root, exclude);
    for (path, list) in p.listings.lock().unwrap().iter() {
        if let Some(mount) = fresh.resolve_path(path) {
            fresh.graft(mount, list);
        }
    }
    fresh.build_trigrams();
    (fresh, st, t0.elapsed())
}

/// Applies one folder's fanotify events to its index. Every folder has its own
/// mark and thread, so a busy filesystem never delays another's updates.
fn watch_loop(cat: Arc<Catalog>, i: usize, mut w: watch::Watcher) {
    let part = &cat.parts[i];
    while !stopping() {
        if !w.wait(300) {
            continue;
        }
        match w.read_events() {
            Ok(events) => {
                if events.is_empty() {
                    continue;
                }
                let mut ix = part.index.write().unwrap();
                // Under the write lock, so a reconciliation swapping the index
                // sees each batch either captured or not yet read -- never half.
                if let Some(buf) = part.capture.lock().unwrap().as_mut() {
                    buf.extend(events.iter().cloned());
                }
                for ev in &events {
                    apply(&mut ix, ev, &cat.exclude);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => {
                eprintln!("spoor: {}: read error: {}", part.root, e);
                FAILED.store(true, std::sync::atomic::Ordering::Relaxed);
                SHUTDOWN.store(true, std::sync::atomic::Ordering::Relaxed);
                return;
            }
        }
    }
}

/// Rebuilds every folder's index from a fresh walk on a timer, one folder at a
/// time. This compacts the arena (dead entries otherwise accumulate for as long
/// as the daemon runs) and corrects drift from any event that could not be
/// applied. Changes made during a walk are captured and replayed before the new
/// index is swapped in. Memory peaks at one extra index for the length of a walk.
fn reconcile_loop(cat: Arc<Catalog>, interval: u64) {
    loop {
        for _ in 0..interval {
            if stopping() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
        for p in &cat.parts {
            if stopping() {
                return;
            }
            *p.capture.lock().unwrap() = Some(Vec::new());
            let (mut fresh, st, took) = walk_part(p, &cat.exclude);
            let (before, after, replayed) = {
                let mut ix = p.index.write().unwrap();
                let events = p.capture.lock().unwrap().take().unwrap_or_default();
                for ev in &events {
                    apply(&mut fresh, ev, &cat.exclude);
                }
                let before = ix.capacity_used();
                let after = fresh.capacity_used();
                *ix = fresh;
                (before, after, events.len())
            };
            eprintln!(
                "spoor: reconciled {} in {:.1}s: {} files + {} dirs, arena {} -> {} slots, {} events replayed",
                p.root,
                took.as_secs_f64(),
                st.files,
                st.dirs,
                before,
                after,
                replayed
            );
        }
    }
}

/// Keeps unwatchable mounts current by walking them on a timer. A mount that
/// is not available yet (rclone has not mounted it, or it refuses root) is
/// retried every minute instead of waiting out the whole interval -- at boot,
/// spoor usually starts before the network mount does.
fn rescan_loop(cat: Arc<Catalog>, paths: Vec<String>, interval: u64) {
    const RETRY: u64 = 60;
    loop {
        let mut all_ok = true;
        for p in &paths {
            if stopping() {
                return;
            }
            let t0 = Instant::now();
            match scan::walk_foreign(p, &cat.exclude) {
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
                    let Some(part) = cat.part_for(p) else {
                        all_ok = false;
                        eprintln!("spoor: rescan {}: not inside an indexed folder", p);
                        continue;
                    };
                    let mut ix = part.index.write().unwrap();
                    match ix.resolve_path(p) {
                        None => {
                            all_ok = false;
                            eprintln!("spoor: rescan {}: not in the index (excluded?)", p);
                        }
                        Some(mount) => {
                            let st = ix.graft(mount, &list);
                            drop(ix);
                            part.listings.lock().unwrap().insert(p.clone(), list);
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

/// Set by a watch thread that cannot continue, so the daemon exits with a
/// failure status and systemd restarts it.
static FAILED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

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

fn apply(ix: &mut Index, ev: &watch::Event, exclude: &Exclude) {
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
        // Anything inside an excluded folder has no parent in the index and is
        // dropped above; only the folder itself, created anew, arrives here.
        if exclude.contains(&full) {
            return;
        }
        let ino = std::fs::symlink_metadata(&full)
            .map(|m| std::os::unix::fs::MetadataExt::ino(&m))
            .unwrap_or(0);

        let id = ix.add(parent, &ev.name, ev.is_dir(), ino);
        if ev.is_dir() {
            scan::scan_subtree(ix, id, &full, 0, exclude);
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
    let st = scan::scan(&mut ix, root, &Exclude::new());
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
                apply(&mut ix, ev, &Exclude::new());
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
