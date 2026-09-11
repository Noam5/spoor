//! Snapshot persistence.
//!
//! Deliberately NOT save-on-exit. FSearch writes its database only on a clean
//! quit, so a crash, OOM or logout throws the whole index away. Here the
//! snapshot is rewritten periodically and atomically, so the worst case is
//! losing the last interval rather than everything.
//!
//! The snapshot carries no inode numbers. It exists to answer queries
//! immediately at startup while a full reconciliation scan runs behind it —
//! there is no change journal on ext4, so a restart cannot know what happened
//! while the daemon was down, and only a rescan can.
//!
//! Layout: magic, part count, then per indexed folder its root path and its
//! arena (count, root id, entries). The single-index layout of pre-release
//! builds is still read.

use crate::index::{Entry, Index, NO_PARENT};
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::os::unix::fs::OpenOptionsExt;

const MAGIC: &[u8; 8] = b"SPOOR002";
/// Pre-release: one index, whose root entry names its folder.
const MAGIC_V1: &[u8; 8] = b"EVRYTHD1";
/// More parts than any real configuration; beyond it the file is corrupt.
const MAX_PARTS: u32 = 4096;

/// Writes every part, each with the folder it indexes. Returns the number of
/// arena slots written.
pub fn save(parts: &[(&str, &Index)], path: &str) -> io::Result<usize> {
    if let Some(dir) = std::path::Path::new(path).parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = format!("{}.tmp", path);
    let mut slots = 0;
    {
        let mut w = BufWriter::new({
            // Every indexed file name is in here, so the file is root-only
            // regardless of the umask. Remove any leftover temp file first, so
            // the mode below really applies.
            let _ = fs::remove_file(&tmp);
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)?
        });
        w.write_all(MAGIC)?;
        w.write_all(&(parts.len() as u32).to_le_bytes())?;
        for (root, ix) in parts {
            let rb = root.as_bytes();
            w.write_all(&(rb.len() as u16).to_le_bytes())?;
            w.write_all(rb)?;
            write_index(&mut w, ix)?;
            slots += ix.raw_len();
        }
        w.flush()?;
    }
    // Atomic replace: a crash mid-write leaves the previous snapshot intact.
    fs::rename(&tmp, path)?;
    Ok(slots)
}

fn write_index(w: &mut impl Write, ix: &Index) -> io::Result<()> {
    w.write_all(&(ix.raw_len() as u64).to_le_bytes())?;
    w.write_all(&ix.root_id().to_le_bytes())?;
    for e in ix.raw_entries() {
        w.write_all(&e.parent.to_le_bytes())?;
        let flags = (e.is_dir as u8) | ((e.alive as u8) << 1);
        w.write_all(&[flags])?;
        let nb: &[u8] = &e.name;
        w.write_all(&(nb.len() as u16).to_le_bytes())?;
        w.write_all(nb)?;
    }
    Ok(())
}

/// Every part in the snapshot, with the folder it indexes.
pub fn load(path: &str) -> io::Result<Vec<(String, Index)>> {
    let mut r = BufReader::new(File::open(path)?);
    let mut magic = [0u8; 8];
    r.read_exact(&mut magic)?;
    if &magic == MAGIC_V1 {
        let ix = read_index(&mut r)?;
        let root = match ix.raw_entries().get(ix.root_id() as usize) {
            Some(e) => String::from_utf8_lossy(&e.name).into_owned(),
            None => return Err(corrupt("root out of range")),
        };
        return Ok(vec![(root, ix)]);
    }
    if &magic != MAGIC {
        return Err(corrupt("bad snapshot magic"));
    }
    let n = read_u32(&mut r)?;
    if n > MAX_PARTS {
        return Err(corrupt("implausible part count"));
    }
    let mut parts = Vec::with_capacity(n as usize);
    for _ in 0..n {
        let mut lb = [0u8; 2];
        r.read_exact(&mut lb)?;
        let mut rb = vec![0u8; u16::from_le_bytes(lb) as usize];
        r.read_exact(&mut rb)?;
        let root = String::from_utf8(rb).map_err(|_| corrupt("root path is not UTF-8"))?;
        parts.push((root, read_index(&mut r)?));
    }
    Ok(parts)
}

fn corrupt(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.to_string())
}

fn read_u32(r: &mut impl Read) -> io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

/// One arena. Every parent must precede its child, as the index relies on it:
/// a damaged file is refused here rather than crashing the daemon later, on
/// every restart.
fn read_index(r: &mut impl Read) -> io::Result<Index> {
    let mut b8 = [0u8; 8];
    r.read_exact(&mut b8)?;
    let count = u64::from_le_bytes(b8);
    if count >= NO_PARENT as u64 {
        return Err(corrupt("implausible entry count"));
    }
    let count = count as usize;
    let root = read_u32(r)?;
    if count > 0 && root as usize >= count {
        return Err(corrupt("root out of range"));
    }
    // Capacity from the header is a hint only: a damaged count must not
    // allocate gigabytes before the read fails.
    let mut entries = Vec::with_capacity(count.min(1 << 20));
    let mut namebuf = Vec::new();
    for i in 0..count {
        let parent = read_u32(r)?;
        if parent != NO_PARENT && parent as usize >= i {
            return Err(corrupt("entry precedes its parent"));
        }
        let mut fb = [0u8; 1];
        r.read_exact(&mut fb)?;
        let mut lb = [0u8; 2];
        r.read_exact(&mut lb)?;
        namebuf.resize(u16::from_le_bytes(lb) as usize, 0);
        r.read_exact(&mut namebuf)?;
        entries.push(Entry {
            parent,
            ascii: namebuf.is_ascii(),
            name: namebuf.clone().into_boxed_slice(),
            is_dir: fb[0] & 1 != 0,
            alive: fb[0] & 2 != 0,
        });
    }
    Ok(Index::from_snapshot(entries, root))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> String {
        format!("/tmp/spoor-{}-test-{}.bin", tag, std::process::id())
    }

    fn one(ix: &Index) -> Vec<(&str, &Index)> {
        vec![("/tmp", ix)]
    }

    fn reload(path: &str) -> Index {
        let mut parts = load(path).unwrap();
        std::fs::remove_file(path).ok();
        assert_eq!(parts.len(), 1);
        parts.remove(0).1
    }

    #[test]
    fn snapshot_is_private_even_over_a_stale_temp_file() {
        use std::os::unix::fs::PermissionsExt;
        let mut ix = Index::new();
        let r = ix.add(NO_PARENT, "/tmp", true, 1);
        ix.set_root(r);
        let path = tmp("perm");
        let stale = format!("{}.tmp", path);
        std::fs::write(&stale, b"stale").unwrap();
        std::fs::set_permissions(&stale, std::fs::Permissions::from_mode(0o644)).unwrap();
        save(&one(&ix), &path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        std::fs::remove_file(&path).ok();
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn snapshot_keeps_raw_name_bytes() {
        let mut ix = Index::new();
        let r = ix.add(NO_PARENT, "/tmp", true, 1);
        ix.set_root(r);
        ix.add(r, &b"caf\xe9"[..], false, 2);
        ix.add(r, "ÉTÉ.txt", false, 3);
        let path = tmp("raw");
        save(&one(&ix), &path).unwrap();
        let back = reload(&path);
        assert_eq!(back.search_raw("caf", 10), vec![b"/tmp/caf\xe9".to_vec()]);
        // the ascii flag is recomputed on load, so folding still applies
        assert_eq!(back.search("été", 10), vec!["/tmp/ÉTÉ.txt"]);
    }

    #[test]
    fn snapshot_round_trip() {
        let mut ix = Index::new();
        let root = ix.add(NO_PARENT, "/tmp", true, 1);
        ix.set_root(root);
        let d = ix.add(root, "dir", true, 2);
        ix.add(d, "File-ABC.txt", false, 3);
        let gone = ix.add(d, "deleted.txt", false, 4);
        ix.remove(gone);

        let path = tmp("snap");
        save(&one(&ix), &path).unwrap();
        let back = reload(&path);
        assert_eq!(back.len(), ix.len());
        assert_eq!(back.search("file-abc", 10), vec!["/tmp/dir/File-ABC.txt"]);
        // dead entries must stay dead across a reload
        assert_eq!(back.search("deleted.txt", 10).len(), 0);
    }

    fn tree(root: &str, name: &str) -> Index {
        let mut ix = Index::new();
        let r = ix.add(NO_PARENT, root, true, 1);
        ix.set_root(r);
        ix.add(r, name, false, 2);
        ix
    }

    #[test]
    fn several_parts_keep_their_folders() {
        let (a, b) = (tree("/home", "a.txt"), tree("/srv/data", "b.txt"));
        let path = tmp("parts");
        save(&[("/home", &a), ("/srv/data", &b)], &path).unwrap();
        let parts = load(&path).unwrap();
        std::fs::remove_file(&path).ok();
        let roots: Vec<&str> = parts.iter().map(|(r, _)| r.as_str()).collect();
        assert_eq!(roots, vec!["/home", "/srv/data"]);
        assert_eq!(parts[1].1.search("b.txt", 10), vec!["/srv/data/b.txt"]);
    }

    #[test]
    fn pre_release_snapshots_still_load() {
        let ix = tree("/home", "old.txt");
        let path = tmp("v1");
        {
            let mut w = BufWriter::new(File::create(&path).unwrap());
            w.write_all(MAGIC_V1).unwrap();
            write_index(&mut w, &ix).unwrap();
        }
        let parts = load(&path).unwrap();
        std::fs::remove_file(&path).ok();
        assert_eq!(parts[0].0, "/home");
        assert_eq!(parts[0].1.search("old", 10), vec!["/home/old.txt"]);
    }

    #[test]
    fn damaged_snapshots_are_refused_not_trusted() {
        let path = tmp("bad");
        let mut bytes = MAGIC.to_vec();
        bytes.extend(1u32.to_le_bytes()); // one part
        bytes.extend(5u16.to_le_bytes());
        bytes.extend(b"/home");
        bytes.extend(2u64.to_le_bytes()); // two entries
        bytes.extend(0u32.to_le_bytes()); // root id 0
        for parent in [NO_PARENT, 7] {
            // the second names a parent that does not precede it
            bytes.extend(parent.to_le_bytes());
            bytes.push(1);
            bytes.extend(1u16.to_le_bytes());
            bytes.push(b'x');
        }
        std::fs::write(&path, &bytes).unwrap();
        let err = load(&path).err().expect("must refuse");
        std::fs::remove_file(&path).ok();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
