//! Unix-socket query protocol.
//!
//! Deliberately trivial and line-based so any client can speak it:
//!   QUERY <limit> <pattern>\n          -> one path per line, blank line ends
//!   SEARCH <limit> <flags> <pattern>\n -> same, with Search-menu options; flags
//!                                        per SearchOpts::to_flags, "-" for none.
//!                                        A line "ERR <message>" reports an error
//!                                        (paths always start with '/').
//!   STATS\n                            -> "entries <n> arena <n>", blank line
//!
//! The daemon runs as root and indexes names that ordinary users may not list,
//! so every reply is filtered by the asking process's own permissions. Its uid,
//! gid and supplementary groups come from the socket (SO_PEERCRED and
//! SO_PEERGROUPS, recorded by the kernel at connect time), and a path is
//! returned only if that user could list the directory that contains it.

use crate::index::{Index, SearchOpts};
use std::collections::HashMap;
use std::ffi::CString;
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

/// Upper bound on results per request, whatever the client asks for.
const MAX_LIMIT: usize = 100_000;
/// Any local user can connect, so bound what one can cost: at most this many
/// requests in flight (further connections are refused, not given a thread),
/// a request line of at most MAX_REQUEST bytes, and timeouts on both sides.
const MAX_CLIENTS: usize = 32;
const MAX_REQUEST: u64 = 64 * 1024;
static ACTIVE: AtomicUsize = AtomicUsize::new(0);

/// Not every libc binding exposes these; the values are fixed by the kernel ABI.
const SO_PEERGROUPS: libc::c_int = 59;
const SYS_FACCESSAT2: libc::c_long = 439;

pub fn serve(sock_path: &str, index: Arc<RwLock<Index>>) -> io::Result<()> {
    let _ = std::fs::remove_file(sock_path);
    let listener = UnixListener::bind(sock_path)?;
    // A root daemon filters every reply by the caller's permissions, so anyone
    // may connect. A daemon running as an ordinary user cannot check on other
    // users' behalf, so its socket is private to its owner.
    let mode = if unsafe { libc::geteuid() } == 0 { 0o666 } else { 0o600 };
    let _ = std::fs::set_permissions(
        sock_path,
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(mode),
    );

    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(_) => continue,
        };
        if ACTIVE.fetch_add(1, Ordering::SeqCst) >= MAX_CLIENTS {
            ACTIVE.fetch_sub(1, Ordering::SeqCst);
            let mut s = stream;
            let _ = s.write_all(b"ERR busy\n\n");
            continue;
        }
        let ix = Arc::clone(&index);
        std::thread::spawn(move || {
            let _ = handle(stream, ix);
            ACTIVE.fetch_sub(1, Ordering::SeqCst);
        });
    }
    Ok(())
}

fn handle(stream: UnixStream, index: Arc<RwLock<Index>>) -> io::Result<()> {
    let peer = peer_of(&stream)?;
    // Root may see everything. A non-root daemon's socket is 0600, so its only
    // possible peer is its own user, who can already list what it indexed.
    let filter = unsafe { libc::geteuid() } == 0 && peer.uid != 0;

    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let mut reader = BufReader::new(stream.try_clone()?.take(MAX_REQUEST));
    let mut writer = BufWriter::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let line = line.trim_end();

    if let Some(rest) = line.strip_prefix("QUERY ") {
        let mut parts = rest.splitn(2, ' ');
        let limit = parse_limit(parts.next());
        let pattern = parts.next().unwrap_or("");
        let result = run_query(&index, &peer, filter, limit, |ix, n| Ok(ix.search(pattern, n)));
        reply(&mut writer, result)?;
    } else if let Some(rest) = line.strip_prefix("SEARCH ") {
        let mut parts = rest.splitn(3, ' ');
        let limit = parse_limit(parts.next());
        let opts = SearchOpts::from_flags(parts.next().unwrap_or("-"));
        let pattern = parts.next().unwrap_or("");
        let result = run_query(&index, &peer, filter, limit, |ix, n| {
            ix.search_opts(pattern, n, &opts)
        });
        reply(&mut writer, result)?;
    } else if line == "STATS" {
        let ix = index.read().unwrap();
        writeln!(writer, "entries {} arena {}", ix.len(), ix.capacity_used())?;
        writeln!(writer)?;
    } else {
        writeln!(writer, "ERR unknown command")?;
        writeln!(writer)?;
    }
    writer.flush()?;
    Ok(())
}

fn parse_limit(s: Option<&str>) -> usize {
    s.and_then(|v| v.parse().ok()).unwrap_or(100).min(MAX_LIMIT)
}

fn reply(w: &mut impl Write, result: Result<Vec<String>, String>) -> io::Result<()> {
    match result {
        Ok(hits) => {
            for h in hits {
                // A file name may legally contain a newline, which would inject
                // a fake result line into this protocol. Until the protocol
                // carries arbitrary bytes, such paths are not sent at all.
                if h.contains(['\n', '\r']) {
                    continue;
                }
                writeln!(w, "{}", h)?;
            }
        }
        Err(e) => writeln!(w, "ERR {}", e)?,
    }
    writeln!(w)
}

/// Run a search and, when filtering, keep only what the peer may see. Results
/// the peer cannot see would leave the page short, so the search is repeated
/// with a larger window until the page fills or the matches run out.
fn run_query(
    index: &RwLock<Index>,
    peer: &Peer,
    filter: bool,
    limit: usize,
    search: impl Fn(&Index, usize) -> Result<Vec<String>, String>,
) -> Result<Vec<String>, String> {
    if !filter {
        let ix = index.read().unwrap();
        return search(&ix, limit);
    }
    let mut cache: HashMap<String, bool> = HashMap::new();
    let mut window = limit.max(1);
    loop {
        let hits = {
            let ix = index.read().unwrap();
            search(&ix, window)?
        };
        let exhausted = hits.len() < window || window >= MAX_LIMIT;
        let mut shown = as_peer(peer, || keep_visible(hits, &mut cache, listable))
            .map_err(|e| format!("cannot check permissions: {}", e))?;
        if shown.len() >= limit || exhausted {
            shown.truncate(limit);
            return Ok(shown);
        }
        window = window.saturating_mul(4).min(MAX_LIMIT);
    }
}

/// Keep the paths whose containing directory passes `can_list`, asking at most
/// once per directory.
fn keep_visible(
    hits: Vec<String>,
    cache: &mut HashMap<String, bool>,
    can_list: impl Fn(&str) -> bool,
) -> Vec<String> {
    hits.into_iter()
        .filter(|p| {
            let dir = parent_dir(p);
            if let Some(&v) = cache.get(dir) {
                return v;
            }
            let v = can_list(dir);
            cache.insert(dir.to_string(), v);
            v
        })
        .collect()
}

fn parent_dir(p: &str) -> &str {
    match p.rfind('/') {
        Some(0) | None => "/",
        Some(i) => &p[..i],
    }
}

/// Could the current thread's filesystem identity list `dir`? Asks the kernel
/// directly (faccessat2 with AT_EACCESS), so ACLs and LSMs are honoured.
fn listable(dir: &str) -> bool {
    let Ok(c) = CString::new(dir) else {
        return false;
    };
    unsafe {
        libc::syscall(
            SYS_FACCESSAT2,
            libc::AT_FDCWD,
            c.as_ptr(),
            libc::R_OK | libc::X_OK,
            libc::AT_EACCESS,
        ) == 0
    }
}

/// Who is asking, as the kernel recorded it when they connected.
#[derive(Debug)]
struct Peer {
    uid: u32,
    gid: u32,
    groups: Vec<libc::gid_t>,
}

fn peer_of(stream: &UnixStream) -> io::Result<Peer> {
    let fd = stream.as_raw_fd();
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut libc::ucred as *mut libc::c_void,
            &mut len,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    let gsize = std::mem::size_of::<libc::gid_t>();
    let mut groups: Vec<libc::gid_t> = vec![0; 32];
    loop {
        let mut glen = (groups.len() * gsize) as libc::socklen_t;
        let rc = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                SO_PEERGROUPS,
                groups.as_mut_ptr() as *mut libc::c_void,
                &mut glen,
            )
        };
        if rc == 0 {
            groups.truncate(glen as usize / gsize);
            break;
        }
        let err = io::Error::last_os_error();
        // ERANGE reports the size needed in glen.
        if err.raw_os_error() == Some(libc::ERANGE) && groups.len() < 65_536 {
            groups.resize((glen as usize / gsize).max(groups.len() * 2), 0);
            continue;
        }
        return Err(err);
    }
    Ok(Peer {
        uid: cred.uid,
        gid: cred.gid,
        groups,
    })
}

/// Run `f` with this thread's filesystem identity switched to the peer's, so
/// that permission checks inside it are the kernel's own answer for that user.
///
/// A non-zero fsuid makes the kernel drop the thread's DAC-override
/// capabilities, so root's privileges do not leak into the checks. Only the
/// calling thread changes: raw syscalls are used deliberately, because glibc's
/// setgroups() is applied to every thread in the process. If the switch cannot
/// be made, nothing is shown (fail closed); if the thread cannot be switched
/// back, the process aborts rather than serve anything under a wrong identity.
fn as_peer<T>(p: &Peer, f: impl FnOnce() -> T) -> io::Result<T> {
    use libc::{c_long, syscall, SYS_getgroups, SYS_setfsgid, SYS_setfsuid, SYS_setgroups};
    const QUERY: c_long = u32::MAX as c_long; // an invalid id: reports without changing
    unsafe {
        let n = syscall(SYS_getgroups, 0 as c_long, std::ptr::null_mut::<libc::gid_t>());
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut saved = vec![0 as libc::gid_t; n as usize];
        if n > 0 && syscall(SYS_getgroups, n, saved.as_mut_ptr()) < 0 {
            return Err(io::Error::last_os_error());
        }
        let orig_uid = syscall(SYS_setfsuid, QUERY);
        let orig_gid = syscall(SYS_setfsgid, QUERY);

        // Groups and gid first, while the thread still holds CAP_SETGID.
        if syscall(SYS_setgroups, p.groups.len() as c_long, p.groups.as_ptr()) != 0 {
            return Err(io::Error::last_os_error());
        }
        syscall(SYS_setfsgid, p.gid as c_long);
        syscall(SYS_setfsuid, p.uid as c_long);
        let became = syscall(SYS_setfsuid, QUERY) == p.uid as c_long
            && syscall(SYS_setfsgid, QUERY) == p.gid as c_long;
        let out = if became {
            Ok(f())
        } else {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "could not assume the caller's identity",
            ))
        };

        // fsuid first: returning to 0 restores the capabilities needed below.
        syscall(SYS_setfsuid, orig_uid);
        syscall(SYS_setfsgid, orig_gid);
        let restored = syscall(SYS_setgroups, saved.len() as c_long, saved.as_ptr()) == 0
            && syscall(SYS_setfsuid, QUERY) == orig_uid
            && syscall(SYS_setfsgid, QUERY) == orig_gid;
        if !restored {
            eprintln!("spoor: could not restore thread credentials; aborting");
            std::process::abort();
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn peer_credentials_come_from_the_socket() {
        let (a, _b) = UnixStream::pair().unwrap();
        let p = peer_of(&a).unwrap();
        assert_eq!(p.uid, unsafe { libc::getuid() });
        assert_eq!(p.gid, unsafe { libc::getgid() });
        let mut mine = vec![0 as libc::gid_t; 1024];
        let n = unsafe { libc::getgroups(1024, mine.as_mut_ptr()) };
        mine.truncate(n as usize);
        let (mut got, mut want) = (p.groups.clone(), mine);
        got.sort();
        want.sort();
        assert_eq!(got, want);
    }

    #[test]
    fn listability_is_the_kernels_answer() {
        if unsafe { libc::geteuid() } == 0 {
            return; // root can list anything; the check below needs a normal user
        }
        let base = std::env::temp_dir().join(format!("spoor-vis-{}", std::process::id()));
        let open = base.join("open");
        let shut = base.join("shut");
        std::fs::create_dir_all(&open).unwrap();
        std::fs::create_dir_all(&shut).unwrap();
        std::fs::set_permissions(&shut, std::fs::Permissions::from_mode(0o000)).unwrap();
        assert!(listable(open.to_str().unwrap()));
        assert!(!listable(shut.to_str().unwrap()));
        std::fs::set_permissions(&shut, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn filtering_asks_once_per_directory_and_keeps_order() {
        let hits: Vec<String> = ["/pub/a", "/priv/b", "/pub/c", "/priv/d", "/pub/sub/e"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let asked = std::cell::RefCell::new(Vec::new());
        let mut cache = HashMap::new();
        let kept = keep_visible(hits, &mut cache, |d| {
            asked.borrow_mut().push(d.to_string());
            !d.starts_with("/priv")
        });
        assert_eq!(kept, vec!["/pub/a", "/pub/c", "/pub/sub/e"]);
        assert_eq!(*asked.borrow(), vec!["/pub", "/priv", "/pub/sub"]);
    }

    #[test]
    fn parents_of_top_level_entries() {
        assert_eq!(parent_dir("/home"), "/");
        assert_eq!(parent_dir("/home/alice/x"), "/home/alice");
        assert_eq!(parent_dir("/"), "/");
    }

    #[test]
    fn identity_switch_fails_closed_without_privilege() {
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        // an unprivileged thread may not set groups: the switch must refuse,
        // and the callback must never run
        let p = Peer { uid: 65534, gid: 65534, groups: vec![65534] };
        let ran = std::cell::Cell::new(false);
        assert!(as_peer(&p, || ran.set(true)).is_err());
        assert!(!ran.get());
    }

    #[test]
    fn newline_paths_are_never_sent() {
        let mut out = Vec::new();
        reply(&mut out, Ok(vec!["/a".into(), "/evil\n/etc/shadow".into(), "/b".into()])).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "/a\n/b\n\n");
    }
}
