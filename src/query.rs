//! Client side of the daemon's socket protocol, shared by every front end.

use crate::index::SearchOpts;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

/// Search with options, returning raw path bytes (SEARCH0: NUL-framed, so
/// names that are not UTF-8 or that contain newlines come back intact). Err
/// carries the daemon's message, e.g. an invalid regex, or a connection
/// failure, suitable for a status line.
pub fn search(
    socket: &str,
    pattern: &str,
    limit: usize,
    opts: &SearchOpts,
) -> Result<Vec<Vec<u8>>, String> {
    if pattern.is_empty() {
        return Ok(Vec::new());
    }
    let stream = UnixStream::connect(socket)
        .map_err(|e| format!("cannot reach daemon at {}: {}", socket, e))?;
    let mut w = stream.try_clone().map_err(|e| e.to_string())?;
    // The request is one line; a pasted newline must not split it.
    let pattern = pattern.replace(['\n', '\r'], " ");
    writeln!(w, "SEARCH0 {} {} {}", limit, opts.to_flags(), pattern).map_err(|e| e.to_string())?;
    let mut r = BufReader::new(stream);
    let mut out = Vec::new();
    let mut rec = Vec::new();
    loop {
        rec.clear();
        if r.read_until(0, &mut rec).map_err(|e| e.to_string())? == 0 {
            break; // connection closed
        }
        if rec.last() == Some(&0) {
            rec.pop();
        }
        if rec.is_empty() {
            break; // end of reply
        }
        // Paths start with '/', so a record starting "ERR " is an error. A
        // refusal sent before the request was read ("ERR busy") uses line
        // framing, hence the trim.
        if let Some(err) = rec.strip_prefix(b"ERR ") {
            return Err(String::from_utf8_lossy(err).trim().to_string());
        }
        out.push(rec.clone());
    }
    Ok(out)
}

pub fn stats(socket: &str) -> Option<String> {
    let stream = UnixStream::connect(socket).ok()?;
    let mut w = stream.try_clone().ok()?;
    writeln!(w, "STATS").ok()?;
    BufReader::new(stream).lines().map_while(Result::ok).next()
}
