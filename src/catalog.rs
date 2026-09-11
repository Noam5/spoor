//! Everything the daemon indexes: one part per configured folder, each with
//! its own index and its own fanotify watcher. Keeping the parts apart means an
//! inode number never has to be told apart from another filesystem's, and the
//! index itself stays single-rooted. Searches run over every part and merge.

use crate::config;
use crate::index::Index;
use crate::watch;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Mutex, RwLock};

/// A rescanned mount's last complete walk, as `scan::walk_foreign` returns it.
pub type Listing = Vec<(u32, Vec<u8>, bool)>;

/// Folders left out of every walk, as absolute paths.
pub type Exclude = HashSet<PathBuf>;

pub struct Part {
    pub root: String,
    pub index: RwLock<Index>,
    /// Events applied while a reconciliation walk of this part runs, kept for
    /// replay onto the fresh index. Pushed and taken only under the index write
    /// lock, so every event is either replayed or applied after the swap:
    /// never lost, never twice.
    pub capture: Mutex<Option<Vec<watch::Event>>>,
    /// Last complete listing of each rescanned mount inside this part, so a
    /// reconciliation can graft it into the fresh index at once.
    pub listings: Mutex<HashMap<String, Listing>>,
}

pub struct Catalog {
    pub parts: Vec<Part>,
    pub exclude: Exclude,
}

impl Catalog {
    pub fn new(parts: Vec<(String, Index)>, exclude: &[String]) -> Catalog {
        Catalog {
            parts: parts
                .into_iter()
                .map(|(root, ix)| Part {
                    root,
                    index: RwLock::new(ix),
                    capture: Mutex::new(None),
                    listings: Mutex::new(HashMap::new()),
                })
                .collect(),
            exclude: exclude.iter().map(PathBuf::from).collect(),
        }
    }

    /// The part whose root contains `path`: the innermost, if one root is a
    /// separate mount inside another.
    pub fn part_for(&self, path: &str) -> Option<&Part> {
        self.parts
            .iter()
            .filter(|p| config::is_within(path, &p.root))
            .max_by_key(|p| p.root.len())
    }

    /// Runs `search` on every part in turn for up to `limit` results in all.
    /// Repeats are dropped: a root that is a mount point inside another root is
    /// listed twice, as a folder of the outer tree and as the inner tree's root.
    pub fn search(
        &self,
        limit: usize,
        search: impl Fn(&Index, usize) -> Result<Vec<Vec<u8>>, String>,
    ) -> Result<Vec<Vec<u8>>, String> {
        let mut out = Vec::new();
        let mut seen: HashSet<Vec<u8>> = HashSet::new();
        for p in &self.parts {
            if out.len() >= limit {
                break;
            }
            let hits = {
                let ix = p.index.read().unwrap();
                search(&ix, limit - out.len())?
            };
            for h in hits {
                if seen.insert(h.clone()) {
                    out.push(h);
                }
            }
        }
        Ok(out)
    }

    /// (live entries, arena slots) over all parts.
    pub fn stats(&self) -> (usize, usize) {
        self.parts.iter().fold((0, 0), |(n, cap), p| {
            let ix = p.index.read().unwrap();
            (n + ix.len(), cap + ix.capacity_used())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::NO_PARENT;

    fn tree(root: &str, names: &[&str]) -> Index {
        let mut ix = Index::new();
        let r = ix.add(NO_PARENT, root, true, 1);
        ix.set_root(r);
        for (i, n) in names.iter().enumerate() {
            ix.add(r, *n, false, 10 + i as u64);
        }
        ix.build_trigrams();
        ix
    }

    #[test]
    fn searches_every_part_and_merges() {
        let mut outer = tree("/", &["report-a.txt"]);
        // the inner root's mount point, as the outer walk lists it
        let root = outer.root_id();
        outer.add(root, "data", true, 99);
        outer.build_trigrams();
        let cat = Catalog::new(
            vec![
                ("/".into(), outer),
                ("/data".into(), tree("/data", &["report-b.txt"])),
            ],
            &[],
        );
        let s = |q: &str, n| {
            cat.search(n, |ix, k| Ok(ix.search_raw(q, k)))
                .unwrap()
                .into_iter()
                .map(|p| String::from_utf8(p).unwrap())
                .collect::<Vec<_>>()
        };
        assert_eq!(s("report", 10), vec!["/report-a.txt", "/data/report-b.txt"]);
        assert_eq!(s("report", 1), vec!["/report-a.txt"]);
        // "/data" is in both parts, but reported once
        assert_eq!(s("data", 10), vec!["/data"]);
        assert_eq!(cat.part_for("/data/x").unwrap().root, "/data");
        assert_eq!(cat.part_for("/datax").unwrap().root, "/");
        assert_eq!(cat.stats().0, 5);
    }
}
