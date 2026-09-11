//! Initial filesystem walk that seeds the index.

use crate::index::{Index, NO_PARENT};
use std::fs;
use std::os::unix::fs::{DirEntryExt, MetadataExt};

pub struct ScanStats {
    pub files: usize,
    pub dirs: usize,
    pub errors: usize,
}

/// Walk `root`, staying on its device (so /proc, /sys and fuse mounts such as
/// the rclone GoogleDrive mount are skipped, as updatedb's PRUNEFS does).
///
/// Note: `DirEntry::file_type()` does NOT follow symlinks, unlike
/// `DirEntry::metadata()`. Using the latter turns a symlinked directory into an
/// apparent real one and the walk loops forever on any symlink cycle.
/// `DirEntry::ino()` also reads d_ino straight from the dirent, so the common
/// path costs no stat at all.
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

    let mut queue = vec![(root_id, root.to_string())];
    while let Some((parent_id, parent_path)) = queue.pop() {
        let rd = match fs::read_dir(&parent_path) {
            Ok(rd) => rd,
            Err(_) => {
                stats.errors += 1;
                continue;
            }
        };
        for entry in rd {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => {
                    stats.errors += 1;
                    continue;
                }
            };
            let ft = match entry.file_type() {
                Ok(t) => t,
                Err(_) => {
                    stats.errors += 1;
                    continue;
                }
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_dir = ft.is_dir(); // symlinks report as symlink, not dir
            let id = index.add_new(parent_id, &name, is_dir, entry.ino());

            if is_dir {
                stats.dirs += 1;
                let child_path = join(&parent_path, &name);
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
pub fn scan_subtree(index: &mut Index, parent_id: u32, path: &str, depth: u32) {
    if depth > 64 {
        return;
    }
    let rd = match fs::read_dir(path) {
        Ok(rd) => rd,
        Err(_) => return,
    };
    for entry in rd.flatten() {
        let ft = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_dir = ft.is_dir();
        let id = index.add(parent_id, &name, is_dir, entry.ino());
        if is_dir {
            scan_subtree(index, id, &join(path, &name), depth + 1);
        }
    }
}

fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{}{}", dir, name)
    } else {
        format!("{}/{}", dir, name)
    }
}

/// Outcome of walking a mount that fanotify cannot watch.
pub enum ForeignWalk {
    /// Pre-order (parent, name, is_dir); parent indexes an earlier element, or
    /// is NO_PARENT for the mount's own children. Plus the unreadable count.
    Listed(Vec<(u32, String, bool)>, usize),
    /// Not currently a readable mount. The caller keeps what it already has.
    Unavailable(String),
}

/// Walk a separately mounted tree (an rclone FUSE mount of Google Drive, say)
/// into a flat list, without touching the index, so a slow network-backed walk
/// never holds the index lock.
///
/// Refuses unless `path` is currently a mount point: before rclone has mounted
/// it, the directory is just an empty folder on the parent filesystem, and
/// walking that would wipe the index's copy of the drive. Also refuses when the
/// mount cannot be read at all -- a FUSE mount without allow_root is closed to
/// the root daemon -- rather than reporting it as empty.
pub fn walk_foreign(path: &str) -> ForeignWalk {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) => return ForeignWalk::Unavailable(e.to_string()),
    };
    let parent = std::path::Path::new(path)
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

    let mut out: Vec<(u32, String, bool)> = Vec::new();
    let mut errors = 0usize;
    let mut stack: Vec<(u32, String)> = vec![(NO_PARENT, path.to_string())];
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
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_dir = ft.is_dir();
            let idx = out.len() as u32;
            let child = join(&dir, &name);
            out.push((parent_idx, name, is_dir));
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
