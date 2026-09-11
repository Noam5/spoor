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

use crate::index::{Entry, Index};
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::os::unix::fs::OpenOptionsExt;

const MAGIC: &[u8; 8] = b"EVRYTHD1";

pub fn save(ix: &Index, path: &str) -> io::Result<usize> {
    if let Some(dir) = std::path::Path::new(path).parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = format!("{}.tmp", path);
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
        w.flush()?;
    }
    // Atomic replace: a crash mid-write leaves the previous snapshot intact.
    fs::rename(&tmp, path)?;
    Ok(ix.raw_len())
}

pub fn load(path: &str) -> io::Result<Index> {
    let mut r = BufReader::new(File::open(path)?);

    let mut magic = [0u8; 8];
    r.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "bad snapshot magic"));
    }
    let mut buf8 = [0u8; 8];
    r.read_exact(&mut buf8)?;
    let count = u64::from_le_bytes(buf8) as usize;
    let mut buf4 = [0u8; 4];
    r.read_exact(&mut buf4)?;
    let root = u32::from_le_bytes(buf4);

    let mut entries = Vec::with_capacity(count);
    let mut namebuf = Vec::new();
    for _ in 0..count {
        r.read_exact(&mut buf4)?;
        let parent = u32::from_le_bytes(buf4);
        let mut fb = [0u8; 1];
        r.read_exact(&mut fb)?;
        let mut lb = [0u8; 2];
        r.read_exact(&mut lb)?;
        let len = u16::from_le_bytes(lb) as usize;
        namebuf.resize(len, 0);
        r.read_exact(&mut namebuf)?;
        entries.push(Entry {
            parent,
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
    use crate::index::NO_PARENT;

    #[test]
    fn snapshot_is_private_even_over_a_stale_temp_file() {
        use std::os::unix::fs::PermissionsExt;
        let mut ix = Index::new();
        let r = ix.add(NO_PARENT, "/tmp", true, 1);
        ix.set_root(r);
        let path = format!("/tmp/spoor-perm-test-{}.bin", std::process::id());
        let tmp = format!("{}.tmp", path);
        std::fs::write(&tmp, b"stale").unwrap();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644)).unwrap();
        save(&ix, &path).unwrap();
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
        let path = format!("/tmp/spoor-raw-test-{}.bin", std::process::id());
        save(&ix, &path).unwrap();
        let back = load(&path).unwrap();
        std::fs::remove_file(&path).ok();
        assert_eq!(back.search_raw("caf", 10), vec![b"/tmp/caf\xe9".to_vec()]);
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

        let path = format!("/tmp/spoor-snap-test-{}.bin", std::process::id());
        save(&ix, &path).unwrap();
        let back = load(&path).unwrap();
        std::fs::remove_file(&path).ok();

        assert_eq!(back.len(), ix.len());
        assert_eq!(back.search("file-abc", 10), vec!["/tmp/dir/File-ABC.txt"]);
        // dead entries must stay dead across a reload
        assert_eq!(back.search("deleted.txt", 10).len(), 0);
    }
}
