//! Filesystem walks that seed the index.
//!
//! Names are kept as raw bytes and paths as `PathBuf`s throughout: Linux does
//! not require file names to be UTF-8, and a lossily converted name finds the
//! file but cannot open it.

use crate::index::{Index, NO_PARENT};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirEntryExt, MetadataExt};
use std::path::{Path, PathBuf};

pub struct ScanStats {
    pub files: usize,
    pub dirs: usize,
    pub errors: usize,
}

/// Walk `root`, staying on its device (so /proc, /sys and FUSE or network
/// mounts are skipped, as updatedb's PRUNEFS does).
///
/// `DirEntry::file_type()` does NOT follow symlinks, unlike
/// `DirEntry::metadata()`: using the latter turns a symlinked directory into an
/// apparent real one, and the walk loops forever on any symlink cycle.
/// `DirEntry::ino()` reads d_ino straight from the dirent, so the common path
/// costs no stat at all.
pub fn scan(index: &mut Index, root: &str) -> ScanStats {
    let mut stats = ScanStats { files: 0, dirs: 0, errors: 0 };

    let meta = match fs::symlink_metadata(root) {
        Ok(m) => m,
        Err(_) => {
            stats.errors += 1;
            return stats;
        }
    };
    let root_dev = meta.dev();

    let root_id = index.add(NO_PARENT, root, true, meta.ino());
    index.set_root(root_id);
    stats.dirs += 1;

    let mut queue: Vec<(u32, PathBuf)> = vec![(root_id, PathBuf::from(root))];
    while let Some((parent_id, parent_path)) = queue.pop() {
        let rd = match fs::read_dir(&parent_path) {
            Ok(rd) => rd,
            Err(_) => {
                stats.errors += 1;
                continue;
            }
        };
        for entry in rd {
            let Ok(entry) = entry else {
                stats.errors += 1;
                continue;
            };
            let Ok(ft) = entry.file_type() else {
                stats.errors += 1;
                continue;
            };
            let name = entry.file_name();
            let is_dir = ft.is_dir(); // symlinks report as symlink, not dir
            let id = index.add_new(parent_id, name.as_bytes(), is_dir, entry.ino());

            if is_dir {
                stats.dirs += 1;
                let child_path = parent_path.join(&name);
                // stat only directories, to honour mount boundaries
                match fs::symlink_metadata(&child_path) {
                    Ok(m) if m.dev() == root_dev => queue.push((id, child_path)),
                    Ok(_) => {}
                    Err(_) => stats.errors += 1,
                }
            } else {
                stats.files += 1;
            }
        }
    }
    stats
}

/// Walk a directory that appeared at runtime, so anything already inside it
/// (created before we processed the event) is picked up too.
pub fn scan_subtree(index: &mut Index, parent_id: u32, path: &Path, depth: u32) {
    if depth > 64 {
        return;
    }
    let rd = match fs::read_dir(path) {
        Ok(rd) => rd,
        Err(_) => return,
    };
    for entry in rd.flatten() {
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        let name = entry.file_name();
        let is_dir = ft.is_dir();
        let id = index.add(parent_id, name.as_bytes(), is_dir, entry.ino());
        if is_dir {
            scan_subtree(index, id, &path.join(&name), depth + 1);
        }
    }
}

/// Outcome of walking a mount that fanotify cannot watch.
pub enum ForeignWalk {
    /// Pre-order (parent, name, is_dir); parent indexes an earlier element, or
    /// is NO_PARENT for the mount's own children. Plus the unreadable count.
    Listed(Vec<(u32, Vec<u8>, bool)>, usize),
    /// Not currently a readable mount. The caller keeps what it already has.
    Unavailable(String),
}

/// Walk a separately mounted tree (an rclone FUSE mount of Google Drive, say)
/// into a flat list, without touching the index, so a slow network-backed walk
/// never holds the index lock.
///
/// Refuses unless `path` is currently a mount point: before the mount appears,
/// the directory is just an empty folder on the parent filesystem, and walking
/// that would wipe the index's copy of the drive. Also refuses when the mount
/// cannot be read at all -- a FUSE mount without allow_other is closed to the
/// root daemon -- rather than reporting it as empty.
pub fn walk_foreign(path: &str) -> ForeignWalk {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) => return ForeignWalk::Unavailable(e.to_string()),
    };
    let parent = Path::new(path)
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| "/".into());
    match fs::symlink_metadata(&parent) {
        Ok(pm) if pm.dev() == meta.dev() => {
            return ForeignWalk::Unavailable("not mounted".to_string())
        }
        Err(e) => return ForeignWalk::Unavailable(format!("{}: {}", parent.display(), e)),
        _ => {}
    }
    let dev = meta.dev();
    // A walk that goes to a network API can take hours when the provider
    // throttles (a cold rclone cache against Google Drive's shared quota ran at
    // ~25 entries/s). Say so, rather than going silent until it finishes.
    eprintln!("spoor: rescan {}: walking", path);
    let started = std::time::Instant::now();
    let mut last_report = started;

    let mut out: Vec<(u32, Vec<u8>, bool)> = Vec::new();
    let mut errors = 0usize;
    let mut stack: Vec<(u32, PathBuf)> = vec![(NO_PARENT, PathBuf::from(path))];
    let mut first = true;
    while let Some((parent_idx, dir)) = stack.pop() {
        if last_report.elapsed() >= std::time::Duration::from_secs(30) {
            let secs = started.elapsed().as_secs_f64();
            eprintln!(
                "spoor: rescan {}: {} entries so far ({:.0}/s, {:.0}s in)",
                path,
                out.len(),
                out.len() as f64 / secs,
                secs
            );
            last_report = std::time::Instant::now();
        }
        let rd = match fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(e) if first => return ForeignWalk::Unavailable(e.to_string()),
            Err(_) => {
                errors += 1;
                continue;
            }
        };
        first = false;
        for entry in rd {
            let Ok(entry) = entry else {
                errors += 1;
                continue;
            };
            let Ok(ft) = entry.file_type() else {
                errors += 1;
                continue;
            };
            let name = entry.file_name();
            let is_dir = ft.is_dir();
            let idx = out.len() as u32;
            let child = dir.join(&name);
            out.push((parent_idx, name.as_bytes().to_vec(), is_dir));
            if is_dir {
                // no nested mounts
                match fs::symlink_metadata(&child) {
                    Ok(m) if m.dev() == dev => stack.push((idx, child)),
                    Ok(_) => {}
                    Err(_) => errors += 1,
                }
            }
        }
    }
    ForeignWalk::Listed(out, errors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn names_that_are_not_utf8_are_found_and_open() {
        let base = std::env::temp_dir().join(format!("spoor-scan-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("sub")).unwrap();
        let latin1: &[u8] = b"caf\xe9-latin1.txt";
        fs::write(base.join("sub").join(OsStr::from_bytes(latin1)), b"x").unwrap();
        fs::write(base.join("sub").join("line\nbreak.txt"), b"y").unwrap();

        let mut ix = Index::new();
        let st = scan(&mut ix, base.to_str().unwrap());
        assert_eq!(st.errors, 0);
        ix.build_trigrams();
        for q in ["latin1", "break"] {
            let hits = ix.search_raw(q, 10);
            assert_eq!(hits.len(), 1, "query {:?}", q);
            // the raw path opens the real file; a lossy copy would not
            assert!(fs::metadata(OsStr::from_bytes(&hits[0])).is_ok(), "query {:?}", q);
        }
        fs::remove_dir_all(&base).unwrap();
    }
}
