//! Client side of the daemon's socket protocol, shared by every front end.

use crate::index::SearchOpts;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

pub fn query(socket: &str, pattern: &str, limit: usize) -> Vec<String> {
    let mut out = Vec::new();
    if pattern.is_empty() {
        return out;
    }
    let Ok(stream) = UnixStream::connect(socket) else {
        return out;
    };
    let Ok(mut w) = stream.try_clone() else {
        return out;
    };
    if writeln!(w, "QUERY {} {}", limit, pattern).is_err() {
        return out;
    }
    for line in BufReader::new(stream).lines().map_while(Result::ok) {
        if line.is_empty() {
            break;
        }
        out.push(line);
    }
    out
}

/// Search with options. Err carries the daemon's message (e.g. an invalid
/// regex) or a connection failure, suitable for a status line.
pub fn search(
    socket: &str,
    pattern: &str,
    limit: usize,
    opts: &SearchOpts,
) -> Result<Vec<String>, String> {
    if pattern.is_empty() {
        return Ok(Vec::new());
    }
    let stream = UnixStream::connect(socket)
        .map_err(|e| format!("cannot reach daemon at {}: {}", socket, e))?;
    let mut w = stream.try_clone().map_err(|e| e.to_string())?;
    // The protocol is line-based; a pasted newline must not split the request.
    let pattern = pattern.replace(['\n', '\r'], " ");
    writeln!(w, "SEARCH {} {} {}", limit, opts.to_flags(), pattern).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for line in BufReader::new(stream).lines().map_while(Result::ok) {
        if line.is_empty() {
            break;
        }
        if let Some(err) = line.strip_prefix("ERR ") {
            return Err(err.to_string());
        }
        out.push(line);
    }
    Ok(out)
}

pub fn stats(socket: &str) -> Option<String> {
    let stream = UnixStream::connect(socket).ok()?;
    let mut w = stream.try_clone().ok()?;
    writeln!(w, "STATS").ok()?;
    BufReader::new(stream).lines().map_while(Result::ok).next()
}
