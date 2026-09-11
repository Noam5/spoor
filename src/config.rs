//! Which folders the daemon indexes: `/etc/spoor/spoor.conf`.
//!
//! One `key = value` per line; list keys repeat:
//!
//! ```text
//! root = /home
//! root = /srv/data
//! exclude = /home/alice/.cache
//! rescan = /home/alice/GoogleDrive
//! rescan_interval = 900
//! ```
//!
//! The GUI's Preferences edit it through `spoor configure`, run as root via
//! pkexec; the daemon reads it when it starts. With no file, the daemon
//! indexes /home.

use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};

pub const DEFAULT_PATH: &str = "/etc/spoor/spoor.conf";

/// Largest settings file `spoor configure` accepts.
const MAX_BYTES: u64 = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// Trees kept current live through fanotify.
    pub roots: Vec<String>,
    /// Folders left out, with everything beneath them.
    pub exclude: Vec<String>,
    /// Mounts fanotify cannot see (rclone, network filesystems), walked on a
    /// timer instead. Each must lie inside a root.
    pub rescan: Vec<String>,
    /// Seconds between walks of the `rescan` folders.
    pub rescan_interval: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            roots: vec!["/home".to_string()],
            exclude: Vec::new(),
            rescan: Vec::new(),
            rescan_interval: 900,
        }
    }
}

impl Config {
    /// Parses the file format. Errors name the line but never quote it: as
    /// `spoor configure` runs as root, echoing input would let it print lines
    /// of files the caller cannot read.
    pub fn parse(text: &str) -> Result<Config, String> {
        let mut c = Config {
            roots: Vec::new(),
            ..Config::default()
        };
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else {
                return Err(format!("line {}: expected `key = value`", n + 1));
            };
            let v = v.trim().to_string();
            match k.trim() {
                "root" => c.roots.push(v),
                "exclude" => c.exclude.push(v),
                "rescan" => c.rescan.push(v),
                "rescan_interval" => {
                    c.rescan_interval = v.parse().map_err(|_| {
                        format!("line {}: rescan_interval must be whole seconds", n + 1)
                    })?
                }
                _ => return Err(format!("line {}: unknown setting", n + 1)),
            }
        }
        Ok(c)
    }

    pub fn to_text(&self) -> String {
        let mut s = String::from(
            "# Which folders spoor indexes. Written by `spoor configure` (the\n\
             # Preferences window); read when the spoor service starts.\n",
        );
        for (key, list) in [
            ("root", &self.roots),
            ("exclude", &self.exclude),
            ("rescan", &self.rescan),
        ] {
            for p in list {
                s.push_str(&format!("{} = {}\n", key, p));
            }
        }
        s.push_str(&format!("rescan_interval = {}\n", self.rescan_interval));
        s
    }

    /// The file at `path`, None if there is none.
    pub fn load(path: &str) -> Result<Option<Config>, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => Config::parse(&text).map(Some),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("{}: {}", path, e)),
        }
    }

    /// Checks everything the daemon relies on, and normalises in place:
    /// absolute paths without `..`, symlinks resolved, duplicates removed.
    /// A root inside another root on the same filesystem would be indexed
    /// twice, so it is dropped; one on a different filesystem (a separate
    /// mount) is kept, since the outer walk stops at mount points. Network
    /// folders must lie inside a root, as their listing is grafted into that
    /// root's tree. Returns a note for everything dropped.
    pub fn validate(&mut self) -> Result<Vec<String>, String> {
        let mut notes = Vec::new();
        if !(60..=7 * 86_400).contains(&self.rescan_interval) {
            return Err("rescan interval must be between 1 minute and 7 days".into());
        }

        let mut roots: Vec<String> = Vec::new();
        for r in &self.roots {
            let p = existing_dir(r, "folder to index")?;
            if !roots.contains(&p) {
                roots.push(p);
            }
        }
        if roots.is_empty() {
            return Err("choose at least one folder to index".into());
        }
        // Outermost first, so a nested root is compared with its container.
        roots.sort_by_key(|r| r.len());
        let mut kept: Vec<String> = Vec::new();
        for r in roots {
            let dev = dev_of(&r);
            match kept.iter().find(|k| is_within(&r, k)) {
                Some(outer) if dev_of(outer) == dev => {
                    notes.push(format!("{} is already indexed as part of {}", r, outer));
                }
                _ => kept.push(r),
            }
        }

        let mut rescan: Vec<String> = Vec::new();
        for r in &self.rescan {
            let p = existing_dir(r, "network folder")?;
            if !kept.iter().any(|k| is_within(&p, k)) {
                return Err(format!(
                    "network folder {} must be inside one of the folders to index",
                    p
                ));
            }
            if !rescan.contains(&p) {
                rescan.push(p);
            }
        }

        let mut exclude: Vec<String> = Vec::new();
        for e in &self.exclude {
            // An excluded folder need not exist (yet); resolve it if it does.
            let p = match std::fs::canonicalize(e) {
                Ok(c) => path_string(&c)?,
                Err(_) => lexical(e, "excluded folder")?,
            };
            if kept.contains(&p) {
                return Err(format!("{} is both indexed and excluded", p));
            }
            if !kept.iter().any(|k| is_within(&p, k)) {
                notes.push(format!("{} is not inside an indexed folder; ignored", p));
                continue;
            }
            if !exclude.contains(&p) {
                exclude.push(p);
            }
        }

        self.roots = kept;
        self.rescan = rescan;
        self.exclude = exclude;
        Ok(notes)
    }
}

impl Config {
    /// The daemon's lighter check. It trusts what `spoor configure` validated,
    /// but must keep running when a folder is missing (a disk not plugged in)
    /// or unreadable to root (a FUSE mount without allow_other), so nothing
    /// here touches the filesystem: paths are normalised, repeats dropped, and
    /// a malformed path is dropped with a note.
    pub fn normalize(&mut self) -> Vec<String> {
        let mut notes = Vec::new();
        for list in [&mut self.roots, &mut self.exclude, &mut self.rescan] {
            let mut out: Vec<String> = Vec::new();
            for p in list.iter() {
                match lexical(p, "folder") {
                    Ok(n) if !out.contains(&n) => out.push(n),
                    Ok(_) => {}
                    Err(e) => notes.push(e),
                }
            }
            *list = out;
        }
        notes
    }
}

/// Validates `text` and installs it at `dest`, atomically and world-readable
/// (the GUI shows it to every user; it holds folder names, not file names).
/// Root only. Returns the notes from `validate`.
pub fn install(text: &str, dest: &str) -> Result<Vec<String>, String> {
    if text.len() as u64 > MAX_BYTES {
        return Err("settings too large".into());
    }
    let mut c = Config::parse(text)?;
    let notes = c.validate()?;
    let dest = Path::new(dest);
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {}", dir.display(), e))?;
    }
    let tmp = dest.with_extension("conf.tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(&tmp)
        .map_err(|e| format!("{}: {}", tmp.display(), e))?;
    f.write_all(c.to_text().as_bytes())
        .and_then(|_| f.sync_all())
        .map_err(|e| format!("{}: {}", tmp.display(), e))?;
    std::fs::rename(&tmp, dest).map_err(|e| format!("{}: {}", dest.display(), e))?;
    Ok(notes)
}

/// Reads settings for `spoor configure`: from stdin for "-", else a file.
pub fn read_input(src: &str) -> Result<String, String> {
    use std::io::Read;
    let mut text = String::new();
    let res = if src == "-" {
        std::io::stdin()
            .take(MAX_BYTES + 1)
            .read_to_string(&mut text)
    } else {
        std::fs::File::open(src).and_then(|f| f.take(MAX_BYTES + 1).read_to_string(&mut text))
    };
    res.map_err(|e| format!("cannot read settings: {}", e))?;
    Ok(text)
}

/// `p` is `dir` or lies beneath it.
pub fn is_within(p: &str, dir: &str) -> bool {
    dir == "/" || p == dir || (p.starts_with(dir) && p.as_bytes().get(dir.len()) == Some(&b'/'))
}

fn dev_of(p: &str) -> u64 {
    std::fs::metadata(p).map(|m| m.dev()).unwrap_or(0)
}

/// Canonical form of an existing directory.
fn existing_dir(p: &str, what: &str) -> Result<String, String> {
    lexical(p, what)?;
    let c = std::fs::canonicalize(p).map_err(|e| format!("{} {}: {}", what, p, e))?;
    if !c.is_dir() {
        return Err(format!("{} {} is not a folder", what, p));
    }
    path_string(&c)
}

/// Absolute, no `..`, no stray separators; not required to exist.
fn lexical(p: &str, what: &str) -> Result<String, String> {
    if p.contains(['\n', '\r', '\0']) {
        return Err(format!("{} contains a line break", what));
    }
    let path = Path::new(p);
    if !path.is_absolute() {
        return Err(format!("{} {} is not an absolute path", what, p));
    }
    let mut out = PathBuf::from("/");
    for c in path.components() {
        match c {
            Component::Normal(n) => out.push(n),
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir | Component::Prefix(_) => {
                return Err(format!("{} {} may not contain ..", what, p))
            }
        }
    }
    path_string(&out)
}

fn path_string(p: &Path) -> Result<String, String> {
    p.to_str()
        .map(str::to_string)
        .ok_or_else(|| format!("{} is not valid UTF-8", p.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("spoor-cfg-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::canonicalize(&d).unwrap()
    }

    #[test]
    fn text_round_trip() {
        let c = Config {
            roots: vec!["/home".into(), "/srv/a b".into()],
            exclude: vec!["/home/x/.cache".into()],
            rescan: vec!["/home/x/Drive".into()],
            rescan_interval: 600,
        };
        assert_eq!(Config::parse(&c.to_text()).unwrap(), c);
    }

    #[test]
    fn parse_errors_name_the_line_not_its_text() {
        let e = Config::parse("root = /home\nsecret stuff\n").unwrap_err();
        assert_eq!(e, "line 2: expected `key = value`");
        let e = Config::parse("password = hunter2").unwrap_err();
        assert!(!e.contains("password") && !e.contains("hunter2"), "{}", e);
        assert!(Config::parse("rescan_interval = soon").is_err());
    }

    #[test]
    fn validation_normalises_and_explains() {
        let d = tmpdir("val");
        let a = d.join("a");
        std::fs::create_dir_all(a.join("inner")).unwrap();
        std::fs::create_dir_all(a.join("drive")).unwrap();
        std::os::unix::fs::symlink(&a, d.join("link")).unwrap();
        let s = |p: &Path| p.to_str().unwrap().to_string();

        let mut c = Config {
            roots: vec![
                s(&a.join("inner")),   // nested, same filesystem: dropped
                format!("{}/", s(&a)), // trailing slash normalised
                s(&d.join("link")),    // symlink resolved: a duplicate
            ],
            exclude: vec![s(&a.join("gone")), "/nowhere/else".into()],
            rescan: vec![s(&a.join("drive"))],
            rescan_interval: 900,
        };
        let notes = c.validate().unwrap();
        assert_eq!(c.roots, vec![s(&a)]);
        assert_eq!(c.exclude, vec![s(&a.join("gone"))]);
        assert_eq!(c.rescan, vec![s(&a.join("drive"))]);
        assert_eq!(notes.len(), 2, "{:?}", notes);

        let bad = |roots: Vec<String>, rescan: Vec<String>| {
            let mut c = Config {
                roots,
                rescan,
                ..Config::default()
            };
            c.validate()
        };
        assert!(bad(vec!["relative/path".into()], vec![]).is_err());
        assert!(bad(vec![format!("{}/../a", s(&a))], vec![]).is_err());
        assert!(bad(vec![s(&a.join("missing"))], vec![]).is_err());
        assert!(bad(vec![], vec![]).is_err());
        assert!(bad(vec![s(&a.join("inner"))], vec![s(&a.join("drive"))]).is_err());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn install_writes_atomically_and_readably() {
        let d = tmpdir("inst");
        let dest = d.join("etc/spoor.conf");
        let text = format!("root = {}\n", d.display());
        let notes = install(&text, dest.to_str().unwrap()).unwrap();
        assert!(notes.is_empty());
        let back = Config::load(dest.to_str().unwrap()).unwrap().unwrap();
        assert_eq!(back.roots, vec![d.to_str().unwrap().to_string()]);
        let mode = std::fs::metadata(&dest).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o644);
        assert!(install("root = relative", dest.to_str().unwrap()).is_err());
        // a failed install leaves the previous file in place
        assert_eq!(Config::load(dest.to_str().unwrap()).unwrap().unwrap(), back);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn daemon_normalisation_never_needs_the_folders() {
        let mut c = Config {
            roots: vec![
                "/no/such/disk/".into(),
                "/no/such/disk".into(),
                "rel".into(),
            ],
            exclude: vec!["/a//b/./c".into()],
            rescan: vec![],
            rescan_interval: 900,
        };
        let notes = c.normalize();
        assert_eq!(c.roots, vec!["/no/such/disk"]);
        assert_eq!(c.exclude, vec!["/a/b/c"]);
        assert_eq!(notes.len(), 1, "{:?}", notes);
    }

    #[test]
    fn within() {
        assert!(is_within("/home/a", "/home"));
        assert!(is_within("/home", "/home"));
        assert!(!is_within("/homework", "/home"));
        assert!(is_within("/anything", "/"));
    }
}
