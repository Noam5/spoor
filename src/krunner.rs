//! KRunner D-Bus runner (org.kde.krunner1).
//!
//! Registered as a DBus2 runner, so KRunner calls Config() and we can declare a
//! minimum letter count — worth doing because each Match is a full index scan.
//!
//! This process is long-lived and holds the daemon socket, so a keystroke costs
//! one socket round-trip rather than a process spawn.

use std::collections::HashMap;
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
    fn query(&self, pattern: &str, limit: usize) -> Vec<String> {
        crate::query::query(&self.socket, pattern, limit)
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
            .map(|path| {
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
                props.insert(
                    "urls".to_string(),
                    Value::from(vec![format!("file://{}", path)]),
                );
                props.insert("category".to_string(), Value::from("Files"));

                (
                    path.clone(),
                    name,
                    icon_for(&path).to_string(),
                    mtype,
                    relevance,
                    props,
                )
            })
            .collect()
    }

    #[zbus(name = "Run")]
    fn run(&self, match_id: &str, action_id: &str) {
        let target = if action_id == "folder" {
            match_id.rfind('/').map(|i| &match_id[..i]).unwrap_or("/")
        } else {
            match_id
        };
        let _ = std::process::Command::new("xdg-open").arg(target).spawn();
    }

    #[zbus(name = "SetActivationToken")]
    fn set_activation_token(&self, _token: &str) {}

    #[zbus(name = "Teardown")]
    fn teardown(&self) {}
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
