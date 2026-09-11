//! KRunner D-Bus runner (org.kde.krunner1).
//!
//! Registered as a DBus2 runner, so KRunner calls Config() and we can declare a
//! minimum letter count — worth doing because each Match is a full index scan.
//!
//! This process is long-lived and holds the daemon socket, so a keystroke costs
//! one socket round-trip rather than a process spawn.
//!
//! D-Bus strings must be UTF-8 and a Linux file name need not be, so a match is
//! identified by its percent-encoded file:// URI, which carries any bytes and is
//! also what KRunner wants in "urls".

use crate::index::SearchOpts;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use zbus::zvariant::Value;

/// KRunner::QueryMatch::Type
const TYPE_POSSIBLE: i32 = 30;
const TYPE_EXACT: i32 = 100;

const MAX_RESULTS: usize = 30;

pub struct Runner {
    pub socket: String,
}

type Match = (String, String, String, i32, f64, HashMap<String, Value<'static>>);

impl Runner {
    fn query(&self, pattern: &str, limit: usize) -> Vec<Vec<u8>> {
        crate::query::search(&self.socket, pattern, limit, &SearchOpts::default())
            .unwrap_or_default()
    }
}

#[zbus::interface(name = "org.kde.krunner1")]
impl Runner {
    /// Declared once by KRunner for DBus2 runners.
    #[zbus(name = "Config")]
    fn config(&self) -> HashMap<String, Value<'static>> {
        let mut m = HashMap::new();
        // Each Match is a full scan; do not run one for one or two letters.
        m.insert("MinLetterCount".to_string(), Value::from(3i32));
        m
    }

    #[zbus(name = "Actions")]
    fn actions(&self) -> Vec<(String, String, String)> {
        vec![(
            "folder".to_string(),
            "Open Containing Folder".to_string(),
            "folder-open".to_string(),
        )]
    }

    #[zbus(name = "Match")]
    fn match_query(&self, query: &str) -> Vec<Match> {
        let q = query.trim();
        if q.len() < 3 {
            return Vec::new();
        }
        let paths = self.query(q, MAX_RESULTS);
        let needle = q.to_ascii_lowercase();

        paths
            .into_iter()
            .map(|raw| {
                let uri = file_uri(&raw);
                let path = String::from_utf8_lossy(&raw);
                let name = path.rsplit('/').next().unwrap_or(&path).to_string();
                let parent = path
                    .rfind('/')
                    .map(|i| path[..i].to_string())
                    .unwrap_or_default();

                // An exact filename match ranks above an incidental path match.
                let lname = name.to_ascii_lowercase();
                let (mtype, relevance) = if lname == needle {
                    (TYPE_EXACT, 1.0)
                } else if lname.starts_with(&needle) {
                    (TYPE_EXACT, 0.9)
                } else if lname.contains(&needle) {
                    (TYPE_POSSIBLE, 0.7)
                } else {
                    (TYPE_POSSIBLE, 0.4)
                };

                let mut props: HashMap<String, Value<'static>> = HashMap::new();
                props.insert("subtext".to_string(), Value::from(parent));
                props.insert("urls".to_string(), Value::from(vec![uri.clone()]));
                props.insert("category".to_string(), Value::from("Files"));

                let icon = icon_for(&path).to_string();
                (uri, name, icon, mtype, relevance, props)
            })
            .collect()
    }

    #[zbus(name = "Run")]
    fn run(&self, match_id: &str, action_id: &str) {
        let Some(path) = path_from_uri(match_id) else {
            return;
        };
        let target = if action_id == "folder" {
            parent_dir(&path)
        } else {
            &path[..]
        };
        let target = OsStr::from_bytes(target).to_os_string();
        // This process lives as long as the session: reap each child, or every
        // file opened from KRunner leaves a zombie behind.
        std::thread::spawn(move || {
            if let Ok(mut c) = std::process::Command::new("xdg-open").arg(&target).spawn() {
                let _ = c.wait();
            }
        });
    }

    #[zbus(name = "SetActivationToken")]
    fn set_activation_token(&self, _token: &str) {}

    #[zbus(name = "Teardown")]
    fn teardown(&self) {}
}

fn parent_dir(p: &[u8]) -> &[u8] {
    match p.iter().rposition(|&b| b == b'/') {
        Some(0) | None => b"/",
        Some(i) => &p[..i],
    }
}

/// RFC 8089 file URI: every byte outside the unreserved set and '/' is
/// percent-encoded, so spaces, '#', '%' and non-UTF-8 bytes all survive.
fn file_uri(path: &[u8]) -> String {
    let mut s = String::with_capacity(path.len() + 8);
    s.push_str("file://");
    for &b in path {
        if b.is_ascii_alphanumeric() || b"/-._~".contains(&b) {
            s.push(b as char);
        } else {
            s.push_str(&format!("%{:02X}", b));
        }
    }
    s
}

/// Inverse of `file_uri`. None for anything that is not a well-formed
/// file:// URI naming an absolute local path.
fn path_from_uri(uri: &str) -> Option<Vec<u8>> {
    let rest = uri.strip_prefix("file://")?.as_bytes();
    let mut out = Vec::with_capacity(rest.len());
    let mut i = 0;
    while i < rest.len() {
        if rest[i] == b'%' {
            let hex = std::str::from_utf8(rest.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(rest[i]);
            i += 1;
        }
    }
    (out.first() == Some(&b'/')).then_some(out)
}

fn icon_for(path: &str) -> &'static str {
    let lower = path.to_ascii_lowercase();
    let ext = lower.rsplit('.').next().unwrap_or("");
    match ext {
        "txt" | "md" | "log" | "rst" => "text-x-generic",
        "pdf" => "application-pdf",
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" => "image-x-generic",
        "mp4" | "mkv" | "webm" | "avi" | "mov" => "video-x-generic",
        "mp3" | "flac" | "ogg" | "wav" | "opus" => "audio-x-generic",
        "zip" | "gz" | "xz" | "bz2" | "tar" | "7z" | "rar" => "package-x-generic",
        "rs" | "c" | "h" | "cpp" | "py" | "sh" | "js" | "ts" | "go" => "text-x-script",
        _ => "text-x-generic",
    }
}

pub fn serve(socket: &str) -> Result<(), Box<dyn std::error::Error>> {
    let runner = Runner {
        socket: socket.to_string(),
    };
    let _conn = zbus::blocking::connection::Builder::session()?
        .name("org.kde.spoor")?
        .serve_at("/runner", runner)?
        .build()?;
    eprintln!("spoor: krunner runner registered at org.kde.spoor /runner");
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uris_carry_any_path_bytes() {
        let raw = b"/home/alice/a b/100%#?/caf\xe9\n.txt";
        let uri = file_uri(raw);
        assert_eq!(uri, "file:///home/alice/a%20b/100%25%23%3F/caf%E9%0A.txt");
        assert_eq!(path_from_uri(&uri).as_deref(), Some(&raw[..]));
        assert_eq!(file_uri(b"/plain/name.txt"), "file:///plain/name.txt");
    }

    #[test]
    fn malformed_uris_are_refused() {
        assert_eq!(path_from_uri("/no/scheme"), None);
        assert_eq!(path_from_uri("file://relative"), None);
        assert_eq!(path_from_uri("file:///bad%zz"), None);
        assert_eq!(path_from_uri("file:///cut%4"), None);
    }

    #[test]
    fn parents() {
        assert_eq!(parent_dir(b"/a/b"), b"/a");
        assert_eq!(parent_dir(b"/a"), b"/");
    }
}
