//! Unix-socket query protocol.
//!
//! Deliberately trivial and line-based so any client can speak it:
//!   QUERY <limit> <pattern>\n          -> one path per line, blank line ends
//!   SEARCH <limit> <flags> <pattern>\n -> same, with Search-menu options; flags
//!                                        per SearchOpts::to_flags, "-" for none.
//!                                        A line "ERR <message>" reports an error
//!                                        (paths always start with '/').
//!   STATS\n                            -> "entries <n> arena <n>", blank line

use crate::index::{Index, SearchOpts};
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, RwLock};

/// Upper bound on results per request, whatever the client asks for.
const MAX_LIMIT: usize = 100_000;
/// Any local user can connect, so bound what one can cost: at most this many
/// requests in flight (further connections are refused, not given a thread),
/// a request line of at most MAX_REQUEST bytes, and timeouts on both sides.
const MAX_CLIENTS: usize = 32;
const MAX_REQUEST: u64 = 64 * 1024;
static ACTIVE: AtomicUsize = AtomicUsize::new(0);

pub fn serve(sock_path: &str, index: Arc<RwLock<Index>>) -> std::io::Result<()> {
    let _ = std::fs::remove_file(sock_path);
    let listener = UnixListener::bind(sock_path)?;
    // World-writable: queries are read-only and carry no privilege.
    let _ = std::fs::set_permissions(
        sock_path,
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o666),
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

fn handle(stream: UnixStream, index: Arc<RwLock<Index>>) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let mut reader = BufReader::new(stream.try_clone()?.take(MAX_REQUEST));
    let mut writer = BufWriter::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let line = line.trim_end();

    if let Some(rest) = line.strip_prefix("QUERY ") {
        let mut parts = rest.splitn(2, ' ');
        let limit: usize = parts.next().unwrap_or("100").parse().unwrap_or(100);
        let pattern = parts.next().unwrap_or("");
        let hits = {
            let ix = index.read().unwrap();
            ix.search(pattern, limit)
        };
        for h in hits {
            writeln!(writer, "{}", h)?;
        }
        writeln!(writer)?;
    } else if let Some(rest) = line.strip_prefix("SEARCH ") {
        let mut parts = rest.splitn(3, ' ');
        let limit: usize = parts
            .next()
            .unwrap_or("100")
            .parse()
            .unwrap_or(100)
            .min(MAX_LIMIT);
        let opts = SearchOpts::from_flags(parts.next().unwrap_or("-"));
        let pattern = parts.next().unwrap_or("");
        let result = {
            let ix = index.read().unwrap();
            ix.search_opts(pattern, limit, &opts)
        };
        match result {
            Ok(hits) => {
                for h in hits {
                    writeln!(writer, "{}", h)?;
                }
            }
            Err(e) => writeln!(writer, "ERR {}", e)?,
        }
        writeln!(writer)?;
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
