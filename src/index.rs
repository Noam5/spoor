//! In-memory filesystem index.
//!
//! Entries are stored in a flat arena; each holds only its own name plus a
//! parent id, so a path is reconstructed by walking up. That keeps memory
//! proportional to total name bytes rather than to total path bytes.

use std::collections::HashMap;

pub const NO_PARENT: u32 = u32::MAX;

/// Which kinds of entry a search returns (FSearch's filter, Everything's
/// Search menu).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Kind {
    #[default]
    All,
    Files,
    Folders,
}

/// The options the GUI's Search menu and Preferences expose.
///
/// Travels over the socket as a compact flag string: c = match case,
/// r = regex, p = search in path, f = files only, d = folders only,
/// h = hide dotfiles, and "-" for none.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SearchOpts {
    pub match_case: bool,
    pub regex: bool,
    pub in_path: bool,
    pub kind: Kind,
    pub hide_hidden: bool,
}

impl SearchOpts {
    pub fn to_flags(&self) -> String {
        let mut s = String::new();
        if self.match_case {
            s.push('c');
        }
        if self.regex {
            s.push('r');
        }
        if self.in_path {
            s.push('p');
        }
        match self.kind {
            Kind::Files => s.push('f'),
            Kind::Folders => s.push('d'),
            Kind::All => {}
        }
        if self.hide_hidden {
            s.push('h');
        }
        if s.is_empty() {
            s.push('-');
        }
        s
    }

    pub fn from_flags(flags: &str) -> SearchOpts {
        let mut o = SearchOpts::default();
        for ch in flags.chars() {
            match ch {
                'c' => o.match_case = true,
                'r' => o.regex = true,
                'p' => o.in_path = true,
                'f' => o.kind = Kind::Files,
                'd' => o.kind = Kind::Folders,
                'h' => o.hide_hidden = true,
                _ => {}
            }
        }
        o
    }
}

/// Delta + varint encoded posting list.
///
/// Ids arrive in ascending order, so only the gap between consecutive ids is
/// stored, and gaps are small enough that most encode in a single byte. Raw
/// `Vec<u32>` cost ~230MB for this machine's 57.6M postings; note that
/// shrink_to_fit does not help there, because freeing slack does not return it
/// to the OS -- the data has to actually be smaller.
/// One checkpoint every SKIP_INTERVAL postings. At 57.6M postings that is
/// ~900k entries, about 7MB — cheap next to the 210MB the encoding saves.
const SKIP_INTERVAL: u32 = 64;

#[derive(Default)]
pub struct Postings {
    bytes: Vec<u8>,
    last: u32,
    len: u32,
    /// (id, byte offset just past that id's delta). Lets intersection jump
    /// straight to the neighbourhood of a target instead of decoding from the
    /// start — the galloping that delta encoding otherwise takes away.
    skips: Vec<(u32, u32)>,
}

impl Postings {
    #[inline]
    pub fn len(&self) -> usize {
        self.len as usize
    }

    #[inline]
    fn push(&mut self, id: u32) {
        if self.len > 0 && id <= self.last {
            return; // ids ascend; a repeat within one name is a no-op
        }
        let delta = if self.len == 0 { id } else { id - self.last };
        let mut v = delta;
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                self.bytes.push(b);
                break;
            }
            self.bytes.push(b | 0x80);
        }
        self.last = id;
        self.len += 1;
        if self.len % SKIP_INTERVAL == 0 {
            self.skips.push((id, self.bytes.len() as u32));
        }
    }

    fn iter(&self) -> PostingsIter<'_> {
        PostingsIter { bytes: &self.bytes, pos: 0, acc: 0, first: true, pending: None }
    }

    /// Last checkpoint at or before `target`, or None if it precedes the first.
    #[inline]
    fn checkpoint_for(&self, target: u32) -> Option<(u32, u32)> {
        let idx = self.skips.partition_point(|&(id, _)| id <= target);
        if idx == 0 {
            None
        } else {
            Some(self.skips[idx - 1])
        }
    }

    /// Resume decoding from a checkpoint, yielding that checkpoint's id first.
    fn iter_at(&self, id: u32, off: u32) -> PostingsIter<'_> {
        PostingsIter {
            bytes: &self.bytes,
            pos: off as usize,
            acc: id,
            first: false,
            pending: Some(id),
        }
    }

    fn to_vec(&self) -> Vec<u32> {
        let mut v = Vec::with_capacity(self.len as usize);
        v.extend(self.iter());
        v
    }
}

pub struct PostingsIter<'a> {
    bytes: &'a [u8],
    pos: usize,
    acc: u32,
    first: bool,
    pending: Option<u32>,
}

impl PostingsIter<'_> {
    #[inline]
    fn byte_pos(&self) -> usize {
        self.pos
    }
}

impl Iterator for PostingsIter<'_> {
    type Item = u32;
    #[inline]
    fn next(&mut self) -> Option<u32> {
        if let Some(p) = self.pending.take() {
            return Some(p);
        }
        if self.pos >= self.bytes.len() {
            return None;
        }
        let mut delta: u32 = 0;
        let mut shift = 0;
        loop {
            let b = self.bytes[self.pos];
            self.pos += 1;
            delta |= ((b & 0x7f) as u32) << shift;
            if b & 0x80 == 0 {
                break;
            }
            shift += 7;
        }
        if self.first {
            self.first = false;
            self.acc = delta;
        } else {
            self.acc += delta;
        }
        Some(self.acc)
    }
}

pub struct Entry {
    pub parent: u32,
    pub name: Box<str>,
    pub is_dir: bool,
    pub alive: bool,
}

pub struct Index {
    entries: Vec<Entry>,
    /// inode -> entry id, directories only. Used to resolve fanotify events,
    /// which identify the *parent directory* of whatever changed.
    dir_ino: HashMap<u64, u32>,
    /// parent id -> child ids. Directories are few, so this stays cheap.
    children: HashMap<u32, Vec<u32>>,
    root: u32,
    /// trigram -> entry ids, ascending. Postings are append-only: ids are
    /// handed out monotonically, so a new entry always belongs at the end, and
    /// a deleted entry is filtered by the `alive` check at verification time
    /// rather than being spliced out.
    trigrams: HashMap<u32, Postings>,
    trigrams_built: bool,
}

impl Index {
    pub fn new() -> Self {
        Index {
            entries: Vec::new(),
            dir_ino: HashMap::new(),
            children: HashMap::new(),
            root: NO_PARENT,
            trigrams: HashMap::new(),
            trigrams_built: false,
        }
    }

    pub fn len(&self) -> usize {
        self.entries.iter().filter(|e| e.alive).count()
    }

    pub fn capacity_used(&self) -> usize {
        self.entries.len()
    }

    pub fn set_root(&mut self, id: u32) {
        self.root = id;
    }

    /// Insert without checking for an existing sibling of the same name. Only
    /// valid during the initial walk, where readdir guarantees uniqueness;
    /// skipping the check keeps the walk linear instead of quadratic in the
    /// size of each directory.
    pub fn add_new(&mut self, parent: u32, name: &str, is_dir: bool, ino: u64) -> u32 {
        self.insert(parent, name, is_dir, ino)
    }

    /// Insert, or return the existing entry if this (parent, name) is already
    /// present. Live events can legitimately arrive twice: scan_subtree walks a
    /// newly created tree, and the individual FAN_CREATE events for the things
    /// inside it arrive afterwards.
    pub fn add(&mut self, parent: u32, name: &str, is_dir: bool, ino: u64) -> u32 {
        if parent != NO_PARENT {
            if let Some(kids) = self.children.get(&parent) {
                for &k in kids {
                    if &*self.entries[k as usize].name == name {
                        let e = &mut self.entries[k as usize];
                        e.alive = true;
                        e.is_dir = is_dir;
                        if is_dir {
                            self.dir_ino.insert(ino, k);
                        }
                        return k;
                    }
                }
            }
        }
        self.insert(parent, name, is_dir, ino)
    }

    fn insert(&mut self, parent: u32, name: &str, is_dir: bool, ino: u64) -> u32 {
        let id = self.entries.len() as u32;
        if self.trigrams_built {
            for_each_trigram(name, |key| {
                self.trigrams.entry(key).or_default().push(id);
            });
        }
        self.entries.push(Entry {
            parent,
            name: name.into(),
            is_dir,
            alive: true,
        });
        if parent != NO_PARENT {
            self.children.entry(parent).or_default().push(id);
        }
        if is_dir {
            self.dir_ino.insert(ino, id);
            self.children.entry(id).or_default();
        }
        id
    }

    pub fn dir_by_ino(&self, ino: u64) -> Option<u32> {
        self.dir_ino.get(&ino).copied()
    }

    pub fn find_child(&self, parent: u32, name: &str) -> Option<u32> {
        self.children.get(&parent)?.iter().copied().find(|&k| {
            let e = &self.entries[k as usize];
            e.alive && &*e.name == name
        })
    }

    /// Mark an entry dead, recursively for directories.
    pub fn remove(&mut self, id: u32) {
        let mut stack = vec![id];
        while let Some(cur) = stack.pop() {
            let e = &mut self.entries[cur as usize];
            if !e.alive {
                continue;
            }
            e.alive = false;
            if e.is_dir {
                if let Some(kids) = self.children.get(&cur) {
                    stack.extend(kids.iter().copied());
                }
                self.dir_ino.retain(|_, &mut v| v != cur);
            }
        }
    }

    pub fn path_of(&self, id: u32) -> String {
        let mut parts: Vec<&str> = Vec::new();
        let mut cur = id;
        while cur != NO_PARENT {
            let e = &self.entries[cur as usize];
            parts.push(&e.name);
            cur = e.parent;
        }
        parts.reverse();
        let mut s = String::new();
        for (i, p) in parts.iter().enumerate() {
            if i == 0 {
                // root entry carries the full mount path, e.g. "/" or "/home"
                s.push_str(p);
                continue;
            }
            if !s.ends_with('/') {
                s.push('/');
            }
            s.push_str(p);
        }
        if s.is_empty() {
            "/".to_string()
        } else {
            s
        }
    }

    /// Default search: case-insensitive substring, matched against the full
    /// path when the pattern contains '/', otherwise against the name alone.
    pub fn search(&self, pattern: &str, limit: usize) -> Vec<String> {
        self.search_opts(pattern, limit, &SearchOpts::default())
            .unwrap_or_default()
    }

    /// Search with the Search-menu options. Fails only on an invalid regex.
    pub fn search_opts(
        &self,
        pattern: &str,
        limit: usize,
        o: &SearchOpts,
    ) -> Result<Vec<String>, String> {
        if pattern.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        // As in FSearch: a '/' in the query switches to path matching even
        // when "Search in Path" is off, since names cannot contain one.
        let in_path = o.in_path || pattern.contains('/');
        if o.regex {
            return self.search_regex(pattern, limit, o, in_path);
        }
        let terms = split_terms(pattern);
        if terms.len() > 1 {
            // Several words: all must appear, each anywhere in the full path
            // (file name or any folder above it). Quotes keep a phrase whole.
            return Ok(self.search_multi(&terms, limit, o));
        }
        let Some(term) = terms.into_iter().next() else {
            return Ok(Vec::new());
        };
        let in_path = o.in_path || term.contains('/');
        let needle = if o.match_case {
            term
        } else {
            term.to_ascii_lowercase()
        };
        if in_path {
            if let Some(hits) = self.search_path_trigram(&needle, limit, o) {
                return Ok(hits);
            }
            return Ok(self.search_path(&needle, limit, o));
        }
        Ok(self.search_name(&needle, limit, o))
    }

    /// Alive and of the requested kind.
    #[inline]
    fn passes_kind(&self, id: u32, o: &SearchOpts) -> bool {
        let e = &self.entries[id as usize];
        e.alive
            && match o.kind {
                Kind::All => true,
                Kind::Files => !e.is_dir,
                Kind::Folders => e.is_dir,
            }
    }

    /// Every filter: alive, kind, and (if requested) not hidden.
    #[inline]
    fn accept(&self, id: u32, o: &SearchOpts) -> bool {
        self.passes_kind(id, o) && !(o.hide_hidden && self.is_hidden(id))
    }

    /// True if the entry, or any ancestor below the index root, is a dotfile.
    fn is_hidden(&self, id: u32) -> bool {
        let mut cur = id;
        while cur != NO_PARENT && cur != self.root {
            let e = &self.entries[cur as usize];
            if e.name.starts_with('.') {
                return true;
            }
            cur = e.parent;
        }
        false
    }

    /// Build the trigram postings in one pass. Done after the initial walk
    /// rather than during it: bulk insertion avoids millions of incremental
    /// HashMap growth steps.
    pub fn build_trigrams(&mut self) {
        let mut map: HashMap<u32, Postings> = HashMap::with_capacity(1 << 17);
        for (i, e) in self.entries.iter().enumerate() {
            let id = i as u32;
            for_each_trigram(&e.name, |key| {
                map.entry(key).or_default().push(id);
            });
        }
        for v in map.values_mut() {
            v.bytes.shrink_to_fit();
        }
        self.trigrams = map;
        self.trigrams_built = true;
    }

    pub fn trigram_stats(&self) -> (usize, usize) {
        let postings: usize = self.trigrams.values().map(|v| v.len()).sum();
        (self.trigrams.len(), postings)
    }

    /// `needle` is already lowercased unless `o.match_case` is set. Trigram
    /// keys are case-folded, so candidates are a superset of case-sensitive
    /// matches as well; verification decides.
    fn search_name(&self, needle: &str, limit: usize, o: &SearchOpts) -> Vec<String> {
        let cs = o.match_case;
        let mut out = Vec::new();
        if self.trigrams_built && needle.len() >= 3 {
            let Some(cands) = self.trigram_candidates(needle) else {
                return out; // a needle trigram is absent: nothing can match
            };
            for id in cands {
                if contains(&self.entries[id as usize].name, needle, cs) && self.accept(id, o) {
                    out.push(self.path_of(id));
                    if out.len() >= limit {
                        break;
                    }
                }
            }
            return out;
        }
        for (i, e) in self.entries.iter().enumerate() {
            if contains(&e.name, needle, cs) && self.accept(i as u32, o) {
                out.push(self.path_of(i as u32));
                if out.len() >= limit {
                    break;
                }
            }
        }
        out
    }

    /// Intersect the postings of every trigram in the needle. Returns None if
    /// any trigram is unknown, which means no name can contain the needle.
    fn trigram_candidates(&self, needle: &str) -> Option<Vec<u32>> {
        let mut keys: Vec<u32> = Vec::new();
        for_each_trigram(needle, |k| keys.push(k));
        keys.sort_unstable();
        keys.dedup();
        if keys.is_empty() {
            return None;
        }

        let mut lists: Vec<&Postings> = Vec::with_capacity(keys.len());
        for k in &keys {
            lists.push(self.trigrams.get(k)?);
        }
        // Start from the rarest trigram so the working set shrinks fastest.
        lists.sort_unstable_by_key(|l| l.len());

        let mut acc: Vec<u32> = lists[0].to_vec();
        for l in &lists[1..] {
            acc = intersect_stream(&acc, l);
            if acc.is_empty() {
                break;
            }
        }
        Some(acc)
    }

    /// Trigram-accelerated path search.
    ///
    /// Wherever the occurrence lies, it ends inside the component that the
    /// needle's last '/'-separated segment falls in (with no '/' at all, the
    /// whole needle lies within one component). That component's name contains
    /// the last segment, so trigrams on it give candidate anchors. Each is
    /// verified by plain containment against its reconstructed path -- correct
    /// by construction, including for the root entry, whose name is a whole
    /// path -- and every descendant of an anchor matches too.
    ///
    /// Returns None when the last segment is too short for a trigram (or there
    /// is no trigram index); the caller then falls back to the KMP pass.
    fn search_path_trigram(&self, needle: &str, limit: usize, o: &SearchOpts) -> Option<Vec<String>> {
        let (anchors, strict) = self.path_anchors(needle, o.match_case)?;
        let flagged: Vec<(u32, bool)> = anchors.into_iter().map(|a| (a, !strict)).collect();
        Some(self.emit_anchored(&flagged, limit, o, &[]))
    }

    /// Entries at which a path needle's occurrence ends, found through trigrams
    /// on its last segment. `strict` is true when the needle ended in '/': the
    /// matches are then the strict descendants of the anchors, not the anchors'
    /// whole subtrees. None when the last segment is too short for a trigram.
    fn path_anchors(&self, needle: &str, cs: bool) -> Option<(Vec<u32>, bool)> {
        if !self.trigrams_built || self.root == NO_PARENT {
            return None;
        }
        // "…/photos/" asks for what is inside a folder: the strict descendants
        // of every entry whose path ends with the rest of the needle.
        let (core, inside) = match needle.strip_suffix('/') {
            Some(c) if !c.is_empty() && !c.ends_with('/') => (c, true),
            Some(_) => return None, // "//" or a lone "/": leave it to KMP
            None => (needle, false),
        };
        let last = core.rsplit('/').next().unwrap_or(core);
        if last.len() < 3 {
            return None;
        }
        // The root entry is the one name containing '/', so a needle can lie
        // entirely inside it; then everything matches.
        if inside && contains(&self.entries[self.root as usize].name, needle, cs) {
            return Some((vec![self.root], false));
        }
        let cands = match self.trigram_candidates(last) {
            Some(c) => c,
            None => return Some((Vec::new(), inside)),
        };
        let mut anchors: Vec<u32> = Vec::new();
        for id in cands {
            let p = self.path_of(id);
            let hit = if inside {
                ends_with(&p, core, cs)
            } else {
                contains(&p, needle, cs)
            };
            if hit {
                anchors.push(id);
            }
        }
        anchors.sort_unstable();
        Some((anchors, inside))
    }

    /// Emit everything beneath each anchor -- and the anchor itself when its
    /// flag says so -- skipping repeats so nested anchors do not yield
    /// duplicates. Every emitted path must also contain each `verify` needle.
    ///
    /// Anchors must be in ascending id order, so an ancestor is always handled
    /// before its descendants (ids are allocated parent-first).
    fn emit_anchored(
        &self,
        anchors: &[(u32, bool)],
        limit: usize,
        o: &SearchOpts,
        verify: &[&str],
    ) -> Vec<String> {
        let mut out = Vec::new();
        let mut seen = vec![false; self.entries.len()];
        for &(a, include_self) in anchors {
            if seen[a as usize] {
                continue;
            }
            // A hidden anchor hides its whole subtree.
            if o.hide_hidden && self.is_hidden(a) {
                continue;
            }
            let mut stack = vec![a];
            while let Some(cur) = stack.pop() {
                if seen[cur as usize] {
                    continue;
                }
                seen[cur as usize] = true;
                // Below the anchor, prune hidden entries rather than walking
                // into a dot-directory only to reject everything inside it.
                if o.hide_hidden && cur != a && self.entries[cur as usize].name.starts_with('.') {
                    continue;
                }
                if (include_self || cur != a) && self.passes_kind(cur, o) {
                    let p = self.path_of(cur);
                    if verify.iter().all(|t| contains(&p, t, o.match_case)) {
                        out.push(p);
                        if out.len() >= limit {
                            return out;
                        }
                    }
                }
                if let Some(kids) = self.children.get(&cur) {
                    stack.extend(kids.iter().rev().copied());
                }
            }
        }
        out
    }

    /// Several terms, all of which must occur somewhere in the full path.
    ///
    /// Each term long enough for the trigram index yields its hit entries: the
    /// names containing it, or for a term with '/', the entries where it ends.
    /// An entry matches a term exactly when some entry on its chain to the root
    /// is a hit (a strict ancestor, for a term ending in '/'). If an entry
    /// matches every term, the deepest of those hits lies on its chain and
    /// already has every term on its own chain -- so only hits need checking,
    /// and only the subtrees of the hits that pass get walked. Terms too short
    /// for trigrams are checked on each emitted path instead; with no indexable
    /// term at all, a one-pass multi-automaton scan does the job.
    fn search_multi(&self, terms: &[String], limit: usize, o: &SearchOpts) -> Vec<String> {
        let cs = o.match_case;
        let folded: Vec<String> = terms
            .iter()
            .take(64)
            .map(|t| if cs { t.clone() } else { t.to_ascii_lowercase() })
            .collect();
        if !self.trigrams_built || self.root == NO_PARENT {
            return self.search_multi_scan(&folded, limit, o);
        }

        let mut indexed: Vec<MultiTerm> = Vec::new();
        let mut verify: Vec<&str> = Vec::new();
        let mut hits: Vec<u32> = Vec::new();
        for t in &folded {
            if t.contains('/') {
                match self.path_anchors(t, cs) {
                    Some((anchors, strict)) => {
                        if anchors.is_empty() {
                            return Vec::new(); // AND with an unmatched term
                        }
                        hits.extend_from_slice(&anchors);
                        indexed.push(MultiTerm {
                            text: t,
                            strict,
                            set: Some(anchors.into_iter().collect()),
                        });
                    }
                    None => verify.push(t),
                }
            } else if t.len() >= 3 {
                let Some(cands) = self.trigram_candidates(t) else {
                    return Vec::new();
                };
                let before = hits.len();
                hits.extend(
                    cands
                        .into_iter()
                        .filter(|&id| contains(&self.entries[id as usize].name, t, cs)),
                );
                if hits.len() == before {
                    return Vec::new();
                }
                indexed.push(MultiTerm {
                    text: t,
                    strict: false,
                    set: None,
                });
            } else {
                verify.push(t);
            }
        }
        if indexed.is_empty() {
            return self.search_multi_scan(&folded, limit, o);
        }
        hits.sort_unstable();
        hits.dedup();

        let mut anchors: Vec<(u32, bool)> = Vec::new();
        'hits: for &h in &hits {
            let mut all_self = true;
            for term in &indexed {
                let (for_self, for_desc) = self.chain_sat(term, h, cs);
                if !for_desc {
                    continue 'hits;
                }
                all_self &= for_self;
            }
            anchors.push((h, all_self));
        }
        self.emit_anchored(&anchors, limit, o, &verify)
    }

    /// Whether `h`'s chain to the root satisfies `term`: for `h` itself, and
    /// for its strict descendants. The two differ only for a term ending in
    /// '/', which needs a hit strictly above the entry.
    fn chain_sat(&self, term: &MultiTerm, h: u32, cs: bool) -> (bool, bool) {
        let mut cur = h;
        let mut for_desc = false;
        while cur != NO_PARENT {
            let hit = match &term.set {
                Some(set) => set.contains(&cur),
                None => contains(&self.entries[cur as usize].name, term.text, cs),
            };
            if hit {
                if cur != h || !term.strict {
                    return (true, true);
                }
                for_desc = true;
            }
            cur = self.entries[cur as usize].parent;
        }
        (false, for_desc)
    }

    /// Multi-term search without the index: one KMP automaton per term, run
    /// down the tree as `search_path` does for one needle, with a bitmask of
    /// the terms seen so far inherited from parent to child. `terms` are
    /// already folded unless case-sensitive.
    fn search_multi_scan(&self, terms: &[String], limit: usize, o: &SearchOpts) -> Vec<String> {
        let k = terms.len().min(64);
        if k == 0 {
            return Vec::new();
        }
        let fold = !o.match_case;
        let pats: Vec<&[u8]> = terms[..k].iter().map(|t| t.as_bytes()).collect();
        let fails: Vec<Vec<usize>> = pats.iter().map(|p| kmp_failure(p)).collect();
        let full: u64 = if k == 64 { u64::MAX } else { (1u64 << k) - 1 };
        let n = self.entries.len();
        let mut state = vec![0u32; n * k];
        let mut mask = vec![0u64; n];
        let mut ends_slash = vec![false; n];
        let mut out = Vec::new();

        for i in 0..n {
            let e = &self.entries[i];
            let root = e.parent == NO_PARENT;
            let pi = e.parent as usize;
            let mut m = if root { 0 } else { mask[pi] };
            for t in 0..k {
                let (p, f) = (pats[t], &fails[t]);
                let mut st = if root { 0 } else { state[pi * k + t] as usize };
                if !root && !ends_slash[pi] {
                    let (ns, h) = kmp_step(st, b'/', p, f);
                    st = ns;
                    if h {
                        m |= 1 << t;
                    }
                }
                for &b in e.name.as_bytes() {
                    let c = if fold { b.to_ascii_lowercase() } else { b };
                    let (ns, h) = kmp_step(st, c, p, f);
                    st = ns;
                    if h {
                        m |= 1 << t;
                    }
                }
                state[i * k + t] = st as u32;
            }
            mask[i] = m;
            ends_slash[i] = e.name.as_bytes().last() == Some(&b'/');
            if m == full && self.accept(i as u32, o) {
                out.push(self.path_of(i as u32));
                if out.len() >= limit {
                    break;
                }
            }
        }
        out
    }

    /// Path search without ever building a path.
    ///
    /// A KMP automaton is run down the tree: each entry inherits its parent's
    /// matcher state and feeds only its own name, so the total work is one pass
    /// over the name bytes rather than reconstructing 2.5M strings. Because a
    /// match anywhere in an ancestor's path is also a match for every
    /// descendant, `matched` is simply inherited.
    ///
    /// Entries are visited in arena order, which is safe because a child is
    /// always allocated after its parent.
    fn search_path(&self, needle: &str, limit: usize, o: &SearchOpts) -> Vec<String> {
        let pat = needle.as_bytes();
        let fold = !o.match_case;
        let fail = kmp_failure(pat);
        let n = self.entries.len();
        let mut state = vec![0u32; n];
        let mut matched = vec![false; n];
        let mut ends_slash = vec![false; n];
        let mut out = Vec::new();

        for i in 0..n {
            let e = &self.entries[i];
            let (mut k, mut hit) = if e.parent == NO_PARENT {
                (0usize, false)
            } else {
                let pi = e.parent as usize;
                (state[pi] as usize, matched[pi])
            };

            // Mirror path_of(): a separator precedes every component except the
            // root, and is omitted when the parent path already ends in '/'.
            if e.parent != NO_PARENT && !ends_slash[e.parent as usize] {
                let (nk, h) = kmp_step(k, b'/', pat, &fail);
                k = nk;
                hit |= h;
            }
            for &b in e.name.as_bytes() {
                let c = if fold { b.to_ascii_lowercase() } else { b };
                let (nk, h) = kmp_step(k, c, pat, &fail);
                k = nk;
                hit |= h;
            }

            state[i] = k as u32;
            matched[i] = hit;
            ends_slash[i] = e.name.as_bytes().last() == Some(&b'/');

            if hit && self.accept(i as u32, o) {
                out.push(self.path_of(i as u32));
                if out.len() >= limit {
                    break;
                }
            }
        }
        out
    }

    /// Regular-expression search over names, or over full paths in path mode.
    ///
    /// Uses the `regex` crate deliberately: it guarantees linear-time matching
    /// and caps compiled size. That matters here, because any local user can
    /// send a pattern to this root daemon over its world-writable socket, and a
    /// backtracking engine would let one pattern pin the index lock.
    fn search_regex(
        &self,
        pattern: &str,
        limit: usize,
        o: &SearchOpts,
        in_path: bool,
    ) -> Result<Vec<String>, String> {
        let re = regex::RegexBuilder::new(pattern)
            .case_insensitive(!o.match_case)
            .size_limit(1 << 22)
            .build()
            .map_err(regex_error)?;

        let mut out = Vec::new();
        if !in_path {
            for (i, e) in self.entries.iter().enumerate() {
                if re.is_match(&e.name) && self.accept(i as u32, o) {
                    out.push(self.path_of(i as u32));
                    if out.len() >= limit {
                        break;
                    }
                }
            }
            return Ok(out);
        }

        // Full-path matching needs the real path string. Build each
        // directory's path once, in arena order (parents precede children),
        // and extend it per entry in a reused buffer, instead of calling
        // path_of() -- a walk to the root -- for all 2.5M entries.
        let mut dir_paths: HashMap<u32, String> = HashMap::new();
        let mut buf = String::new();
        for (i, e) in self.entries.iter().enumerate() {
            let id = i as u32;
            buf.clear();
            match dir_paths.get(&e.parent) {
                _ if e.parent == NO_PARENT => buf.push_str(&e.name),
                Some(p) => {
                    buf.push_str(p);
                    if !p.ends_with('/') {
                        buf.push('/');
                    }
                    buf.push_str(&e.name);
                }
                None => buf.push_str(&self.path_of(id)),
            }
            if e.is_dir {
                dir_paths.insert(id, buf.clone());
            }
            if re.is_match(&buf) && self.accept(id, o) {
                out.push(buf.clone());
                if out.len() >= limit {
                    break;
                }
            }
        }
        Ok(out)
    }
}

/// A regex error's Display spans several lines (pattern, caret, message); the
/// socket protocol is line-based, so keep only the message.
fn regex_error(e: regex::Error) -> String {
    let msg = e.to_string();
    let last = msg
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    format!("invalid regex: {}", last.trim_start_matches("error: "))
}

/// One indexable term of a multi-term query. `set` holds the hit entries for a
/// term containing '/'; a plain term's hits are just names containing it.
struct MultiTerm<'a> {
    text: &'a str,
    strict: bool,
    set: Option<std::collections::HashSet<u32>>,
}

/// Split a query into AND terms on whitespace. Double quotes keep a phrase,
/// spaces included, together -- as in FSearch and Everything.
pub fn split_terms(q: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for ch in q.chars() {
        match ch {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Suffix test; `suffix` must already be lowercased when case-insensitive.
#[inline]
fn ends_with(haystack: &str, suffix: &str, case_sensitive: bool) -> bool {
    if case_sensitive {
        haystack.ends_with(suffix)
    } else {
        haystack.len() >= suffix.len()
            && haystack.as_bytes()[haystack.len() - suffix.len()..]
                .eq_ignore_ascii_case(suffix.as_bytes())
    }
}

/// Substring test; `needle` must already be lowercased when case-insensitive.
#[inline]
fn contains(haystack: &str, needle: &str, case_sensitive: bool) -> bool {
    if case_sensitive {
        haystack.contains(needle)
    } else {
        contains_ascii_ci(haystack, needle)
    }
}

/// Every 3-byte window of `s`, ASCII-lowercased, packed into a u32.
#[inline]
fn for_each_trigram<F: FnMut(u32)>(s: &str, mut f: F) {
    let b = s.as_bytes();
    if b.len() < 3 {
        return;
    }
    for w in b.windows(3) {
        let key = ((w[0].to_ascii_lowercase() as u32) << 16)
            | ((w[1].to_ascii_lowercase() as u32) << 8)
            | (w[2].to_ascii_lowercase() as u32);
        f(key);
    }
}




/// Intersect a materialised sorted list against a compressed one.
///
/// Streams the compressed side, but when it falls behind the target it jumps to
/// the nearest checkpoint instead of decoding every intervening delta. Without
/// this, delta encoding forces a linear decode and costs roughly 3x on queries.
fn intersect_stream(acc: &[u32], p: &Postings) -> Vec<u32> {
    let mut out = Vec::with_capacity(acc.len().min(p.len()));
    let Some(&acc_max) = acc.last() else {
        return out;
    };
    let mut it = p.iter();
    let mut cur = it.next();
    let mut i = 0usize;
    // Consulting the skip table costs a binary search, which is a loss when
    // streaming would have caught up in a step or two. Only gallop once we have
    // fallen clearly behind.
    let mut behind = 0u32;
    const GALLOP_AFTER: u32 = 8;

    while i < acc.len() {
        let Some(c) = cur else { break };
        if c > acc_max {
            break; // nothing further can match
        }
        match c.cmp(&acc[i]) {
            std::cmp::Ordering::Equal => {
                out.push(c);
                i += 1;
                behind = 0;
                cur = it.next();
            }
            std::cmp::Ordering::Greater => {
                i += 1;
                behind = 0;
            }
            std::cmp::Ordering::Less => {
                behind += 1;
                if behind >= GALLOP_AFTER {
                    behind = 0;
                    if let Some((cp_id, cp_off)) = p.checkpoint_for(acc[i]) {
                        if cp_off as usize > it.byte_pos() {
                            it = p.iter_at(cp_id, cp_off);
                        }
                    }
                }
                cur = it.next();
            }
        }
    }
    out
}

fn kmp_failure(p: &[u8]) -> Vec<usize> {
    let mut f = vec![0usize; p.len()];
    let mut k = 0usize;
    for i in 1..p.len() {
        while k > 0 && p[k] != p[i] {
            k = f[k - 1];
        }
        if p[k] == p[i] {
            k += 1;
        }
        f[i] = k;
    }
    f
}

#[inline]
fn kmp_step(mut k: usize, c: u8, p: &[u8], f: &[usize]) -> (usize, bool) {
    while k > 0 && p[k] != c {
        k = f[k - 1];
    }
    if p[k] == c {
        k += 1;
    }
    if k == p.len() {
        // reset for overlapping matches
        (f[k - 1], true)
    } else {
        (k, false)
    }
}

/// Substring search ignoring ASCII case, without allocating.
fn contains_ascii_ci(haystack: &str, needle_lower: &str) -> bool {
    if needle_lower.is_empty() {
        return true;
    }
    let h = haystack.as_bytes();
    let n = needle_lower.as_bytes();
    if n.len() > h.len() {
        return false;
    }
    let first = n[0];
    for i in 0..=(h.len() - n.len()) {
        if h[i].to_ascii_lowercase() != first {
            continue;
        }
        let mut ok = true;
        for j in 1..n.len() {
            if h[i + j].to_ascii_lowercase() != n[j] {
                ok = false;
                break;
            }
        }
        if ok {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> (Index, u32) {
        let mut ix = Index::new();
        let root = ix.add(NO_PARENT, "/", true, 1);
        ix.set_root(root);
        let home = ix.add(root, "home", true, 2);
        let alice = ix.add(home, "alice", true, 3);
        ix.add(alice, "invoice-followup-T004821.txt", false, 4);
        (ix, alice)
    }

    #[test]
    fn reconstructs_paths() {
        let (ix, alice) = sample();
        assert_eq!(ix.path_of(alice), "/home/alice");
    }

    #[test]
    fn finds_case_insensitively() {
        let (ix, _) = sample();
        // the exact bug that started this: lowercase query, uppercase filename
        let hits = ix.search("invoice-followup-t004821", 10);
        assert_eq!(hits, vec!["/home/alice/invoice-followup-T004821.txt"]);
    }

    #[test]
    fn matches_on_full_path_when_slash_present() {
        let (ix, _) = sample();
        assert_eq!(ix.search("alice/invoice", 10).len(), 1);
        assert_eq!(ix.search("nosuchdir/invoice", 10).len(), 0);
    }

    #[test]
    fn path_query_matches_descendants() {
        // "/home/alice" contains "home/alice", and so does everything beneath it
        let (ix, _) = sample();
        let hits = ix.search("home/alice", 10);
        assert!(hits.iter().any(|h| h.ends_with("invoice-followup-T004821.txt")));
    }

    #[test]
    fn path_query_spans_a_separator() {
        let (mut ix, alice) = sample();
        let proj = ix.add(alice, "project", true, 20);
        ix.add(proj, "notes.md", false, 21);
        assert_eq!(ix.search("alice/project", 10).len(), 2); // dir + child
        assert_eq!(ix.search("alice/proj", 10).len(), 2); // partial last segment
        assert_eq!(ix.search("lice/project", 10).len(), 2); // partial first segment
        assert_eq!(ix.search("alice/zzz", 10).len(), 0);
    }

    #[test]
    fn path_query_is_case_insensitive() {
        let (ix, _) = sample();
        assert_eq!(ix.search("HOME/ALICE", 10).len(), ix.search("home/alice", 10).len());
        assert!(!ix.search("HOME/ALICE", 10).is_empty());
    }

    #[test]
    fn removal_is_recursive() {
        let (mut ix, alice) = sample();
        assert_eq!(ix.search("invoice", 10).len(), 1);
        ix.remove(alice);
        assert_eq!(ix.search("invoice", 10).len(), 0);
    }

    fn varied() -> Index {
        let mut ix = Index::new();
        let root = ix.add(NO_PARENT, "/data", true, 1);
        ix.set_root(root);
        let a = ix.add(root, "reports", true, 2);
        for n in [
            "invoice-followup-T004821.txt",
            "invoice-entity-response.md",
            "INVOICE-Summary.PDF",
            "notes.txt",
            "readme",
            "ab",
            "watch.rs",
            "watcher.rs",
            "unrelated-file.bin",
        ] {
            ix.add(a, n, false, 10);
        }
        ix
    }

    fn tree() -> Index {
        let mut ix = Index::new();
        let root = ix.add(NO_PARENT, "/home", true, 1);
        ix.set_root(root);
        let alice = ix.add(root, "alice", true, 2);
        let ws = ix.add(alice, "workspace", true, 3);
        let garden = ix.add(ws, "garden-analyze", true, 4);
        let research = ix.add(garden, "research", true, 5);
        let h10 = ix.add(research, "H10-seed-catalog", true, 6);
        let data = ix.add(h10, "data", true, 7);
        let suppliers = ix.add(data, "suppliers", true, 8);
        ix.add(suppliers, "invoice-followup-T004821.txt", false, 9);
        ix.add(suppliers, "invoice-entity-response.md", false, 10);
        let src = ix.add(alice, "src", true, 11);
        let evd = ix.add(src, "spoor", true, 12);
        let esrc = ix.add(evd, "src", true, 13);
        for n in ["watch.rs", "index.rs", "gui.rs", "krunner.rs"] {
            ix.add(esrc, n, false, 20);
        }
        let ws2 = ix.add(src, "workspace-notes", true, 30);
        ix.add(ws2, "garden.md", false, 31);
        ix
    }

    #[test]
    fn trigram_path_search_matches_kmp_exactly() {
        let plain = tree();
        let mut tri = tree();
        tri.build_trigrams();
        for q in [
            "workspace/garden",
            "workspace/garden-analyze",
            "src/spoor",
            "spoor/src",
            "alice/src",
            "home/alice",
            "/home/alice",
            "suppliers/invoice",
            "data/suppliers",
            "space/garden",
            "src/watch.rs",
            "src/nothing-here",
            "zzz/aaa",
            "alice/workspace",
            "H10-seed-catalog/data",
        ] {
            let mut a = plain.search(q, 1000);
            let mut b = tri.search(q, 1000);
            a.sort();
            b.sort();
            assert_eq!(a, b, "path query {:?} diverged", q);
        }
    }

    #[test]
    fn postings_round_trip_including_large_gaps() {
        let mut p = Postings::default();
        let ids: Vec<u32> = vec![0, 1, 2, 127, 128, 129, 16_383, 16_384, 1_000_000, 2_573_082];
        for &i in &ids {
            p.push(i);
        }
        assert_eq!(p.len(), ids.len());
        assert_eq!(p.to_vec(), ids);
        // out-of-order or repeated ids are ignored, not corrupting the stream
        p.push(5);
        p.push(2_573_082);
        assert_eq!(p.to_vec(), ids);
    }

    #[test]
    fn seeking_from_a_checkpoint_yields_the_same_tail() {
        let mut p = Postings::default();
        let ids: Vec<u32> = (0..1000u32).map(|i| i * 7).collect();
        for &i in &ids {
            p.push(i);
        }
        assert_eq!(p.to_vec(), ids);
        assert!(!p.skips.is_empty(), "list long enough to have checkpoints");

        for target in [0u32, 1, 6, 7, 8, 349, 350, 351, 6992, 6993, 9999] {
            let expected: Vec<u32> = ids.iter().copied().filter(|&x| x >= target).collect();
            let got: Vec<u32> = match p.checkpoint_for(target) {
                Some((id, off)) => p.iter_at(id, off).filter(|&x| x >= target).collect(),
                None => p.iter().filter(|&x| x >= target).collect(),
            };
            assert_eq!(got, expected, "seek to {}", target);
        }
    }

    #[test]
    fn galloping_intersection_matches_naive() {
        let mut p = Postings::default();
        let ids: Vec<u32> = (0..5000u32).map(|i| i * 3).collect();
        for &i in &ids {
            p.push(i);
        }
        for step in [1u32, 2, 3, 50, 977] {
            let acc: Vec<u32> = (0..400u32).map(|i| i * step).collect();
            let expected: Vec<u32> = acc
                .iter()
                .copied()
                .filter(|x| ids.binary_search(x).is_ok())
                .collect();
            assert_eq!(intersect_stream(&acc, &p), expected, "step {}", step);
        }
        // empty accumulator must not panic
        assert!(intersect_stream(&[], &p).is_empty());
    }

    fn opts() -> SearchOpts {
        SearchOpts::default()
    }

    #[test]
    fn match_case_distinguishes_case() {
        let (ix, _) = sample();
        let cs = SearchOpts { match_case: true, ..opts() };
        assert_eq!(ix.search_opts("T004821", 10, &cs).unwrap().len(), 1);
        assert_eq!(ix.search_opts("t004821", 10, &cs).unwrap().len(), 0);
        assert_eq!(ix.search_opts("t004821", 10, &opts()).unwrap().len(), 1);
    }

    #[test]
    fn match_case_holds_with_trigrams_built() {
        let mut ix = varied();
        ix.build_trigrams();
        let cs = SearchOpts { match_case: true, ..opts() };
        assert_eq!(ix.search_opts("INVOICE", 10, &cs).unwrap().len(), 1);
        assert_eq!(ix.search_opts("invoice", 10, &cs).unwrap().len(), 2);
        assert_eq!(ix.search_opts("invoice", 10, &opts()).unwrap().len(), 3);
    }

    #[test]
    fn search_in_path_matches_descendants_without_a_slash() {
        let mut ix = tree();
        ix.build_trigrams();
        let p = SearchOpts { in_path: true, ..opts() };
        // name mode: only the directory itself
        assert_eq!(ix.search_opts("garden-analyze", 100, &opts()).unwrap().len(), 1);
        // path mode: the directory and everything beneath it
        let hits = ix.search_opts("garden-analyze", 100, &p).unwrap();
        assert!(hits.iter().any(|h| h.ends_with("invoice-followup-T004821.txt")));
        assert_eq!(hits.len(), 7);
    }

    #[test]
    fn path_mode_trigram_matches_kmp_for_every_case_setting() {
        let plain = tree();
        let mut tri = tree();
        tri.build_trigrams();
        for cs in [false, true] {
            let o = SearchOpts { in_path: true, match_case: cs, ..opts() };
            for q in [
                "garden", "suppliers", "alice", "ALICE", "src", "rs", "H10", "h10", "zzz",
                "spoor", "workspace/garden", "SRC/spoor", "home",
            ] {
                let mut a = plain.search_opts(q, 1000, &o).unwrap();
                let mut b = tri.search_opts(q, 1000, &o).unwrap();
                a.sort();
                b.sort();
                assert_eq!(a, b, "query {:?} case_sensitive={}", q, cs);
            }
        }
    }

    #[test]
    fn regex_matches_names_and_paths() {
        let mut ix = tree();
        ix.build_trigrams();
        let r = SearchOpts { regex: true, ..opts() };
        assert_eq!(ix.search_opts(r"^invoice-.*\.txt$", 100, &r).unwrap().len(), 1);
        // case-insensitive unless Match Case is on
        assert_eq!(ix.search_opts(r"^INVOICE-", 100, &r).unwrap().len(), 2);
        let rc = SearchOpts { regex: true, match_case: true, ..opts() };
        assert_eq!(ix.search_opts(r"^INVOICE-", 100, &rc).unwrap().len(), 0);
        // a '/' in the pattern matches against the whole path
        assert_eq!(
            ix.search_opts(r"spoor/src/[a-z]+\.rs$", 100, &r).unwrap().len(),
            4
        );
        let rp = SearchOpts { regex: true, in_path: true, ..opts() };
        assert_eq!(
            ix.search_opts(r"^/home/alice/src$", 100, &rp).unwrap(),
            vec!["/home/alice/src"]
        );
    }

    #[test]
    fn invalid_regex_is_an_error_not_a_panic() {
        let (ix, _) = sample();
        let r = SearchOpts { regex: true, ..opts() };
        let err = ix.search_opts("(unclosed", 10, &r).unwrap_err();
        assert!(err.starts_with("invalid regex"), "{}", err);
        assert!(!err.contains('\n'), "must fit the line protocol: {:?}", err);
    }

    #[test]
    fn files_and_folders_filters() {
        let mut ix = tree();
        ix.build_trigrams();
        let files = SearchOpts { kind: Kind::Files, ..opts() };
        let folders = SearchOpts { kind: Kind::Folders, ..opts() };
        assert_eq!(ix.search_opts("src", 100, &opts()).unwrap().len(), 2);
        assert_eq!(ix.search_opts("src", 100, &files).unwrap().len(), 0);
        assert_eq!(ix.search_opts("src", 100, &folders).unwrap().len(), 2);
        assert_eq!(ix.search_opts(".rs", 100, &files).unwrap().len(), 4);
        assert_eq!(ix.search_opts(".rs", 100, &folders).unwrap().len(), 0);
        // filters apply in path mode too, where descendants are emitted
        let p_files = SearchOpts { kind: Kind::Files, in_path: true, ..opts() };
        assert_eq!(ix.search_opts("spoor", 100, &p_files).unwrap().len(), 4);
    }

    #[test]
    fn hidden_entries_can_be_excluded() {
        let mut ix = tree();
        let root = ix.root_id();
        let alice = ix.find_child(root, "alice").unwrap();
        let cache = ix.add(alice, ".cache", true, 50);
        ix.add(cache, "notes-cache.txt", false, 51);
        ix.add(alice, ".notes-rc", false, 52);
        ix.build_trigrams();
        let hide = SearchOpts { hide_hidden: true, ..opts() };
        assert_eq!(ix.search_opts("notes", 100, &opts()).unwrap().len(), 3);
        assert_eq!(ix.search_opts("notes", 100, &hide).unwrap().len(), 1);
        let path = SearchOpts { in_path: true, ..opts() };
        let hide_path = SearchOpts { in_path: true, hide_hidden: true, ..opts() };
        assert_eq!(ix.search_opts("cache", 100, &path).unwrap().len(), 2);
        assert_eq!(ix.search_opts("cache", 100, &hide_path).unwrap().len(), 0);
        let hide_regex = SearchOpts { regex: true, hide_hidden: true, ..opts() };
        assert_eq!(ix.search_opts("notes", 100, &hide_regex).unwrap().len(), 1);
    }

    #[test]
    fn flags_round_trip() {
        let o = SearchOpts {
            match_case: true,
            regex: true,
            in_path: true,
            kind: Kind::Folders,
            hide_hidden: true,
        };
        assert_eq!(SearchOpts::from_flags(&o.to_flags()), o);
        assert_eq!(SearchOpts::default().to_flags(), "-");
        assert_eq!(SearchOpts::from_flags("-"), SearchOpts::default());
    }

    #[test]
    fn trailing_slash_path_queries_match_kmp() {
        for root_name in ["/home", "/home/alice"] {
            let build = || {
                let mut ix = Index::new();
                let root = ix.add(NO_PARENT, root_name, true, 1);
                ix.set_root(root);
                let src = ix.add(root, "src", true, 2);
                let sp = ix.add(src, "spoor", true, 3);
                ix.add(sp, "main.rs", false, 4);
                let photos = ix.add(root, "photos", true, 5);
                ix.add(photos, "IMG-1.jpg", false, 6);
                let nested = ix.add(photos, "photos-2024", true, 7);
                ix.add(nested, "IMG-2.jpg", false, 8);
                ix
            };
            let plain = build();
            let mut tri = build();
            tri.build_trigrams();
            for q in [
                "home/", "alice/", "/home/", "/home/alice/", "src/", "src/spoor/", "photos/",
                "PHOTOS/", "photos-2024/", "oto/", "zzz/", "/home/alice/photos/", "ome/alice/pho",
            ] {
                for cs in [false, true] {
                    let o = SearchOpts { match_case: cs, ..SearchOpts::default() };
                    let mut a = plain.search_opts(q, 1000, &o).unwrap();
                    let mut b = tri.search_opts(q, 1000, &o).unwrap();
                    a.sort();
                    b.sort();
                    assert_eq!(a, b, "root {:?} query {:?} case_sensitive={}", root_name, q, cs);
                }
            }
        }
    }

    #[test]
    fn graft_replaces_a_mounted_subtree_in_place() {
        let mut ix = tree();
        ix.build_trigrams();
        let root = ix.root_id();
        let alice = ix.find_child(root, "alice").unwrap();
        let drive = ix.add(alice, "GoogleDrive", true, 60);
        let w = |v: &[(u32, &str, bool)]| -> Vec<(u32, String, bool)> {
            v.iter().map(|(p, n, d)| (*p, n.to_string(), *d)).collect()
        };

        let s1 = ix.graft(drive, &w(&[
            (NO_PARENT, "photos", true),
            (0, "IMG-1.jpg", false),
            (0, "IMG-2.jpg", false),
            (NO_PARENT, "notes.txt", false),
        ]));
        assert_eq!((s1.total, s1.added, s1.removed), (4, 4, 0));
        assert_eq!(ix.search("IMG-", 10).len(), 2);
        let arena = ix.raw_len();

        // IMG-2 deleted on the remote, IMG-3 added
        let s2 = ix.graft(drive, &w(&[
            (NO_PARENT, "photos", true),
            (0, "IMG-1.jpg", false),
            (0, "IMG-3.jpg", false),
            (NO_PARENT, "notes.txt", false),
        ]));
        assert_eq!((s2.total, s2.added, s2.removed), (4, 1, 1));
        let mut hits = ix.search("IMG-", 10);
        hits.sort();
        assert_eq!(
            hits,
            vec![
                "/home/alice/GoogleDrive/photos/IMG-1.jpg",
                "/home/alice/GoogleDrive/photos/IMG-3.jpg"
            ]
        );
        assert_eq!(ix.raw_len(), arena + 1, "unchanged entries are revived, not re-created");
        // the query that prompted this: a folder path with a trailing slash
        assert_eq!(ix.search("/home/alice/GoogleDrive/photos/", 10).len(), 2);

        // an empty listing empties the subtree but keeps the mount point
        let s3 = ix.graft(drive, &[]);
        assert_eq!(s3.removed, 4);
        assert_eq!(ix.search("IMG-", 10).len(), 0);
        assert_eq!(ix.search("GoogleDrive", 10).len(), 1);
    }

    #[test]
    fn query_splitting() {
        assert_eq!(split_terms("a b"), vec!["a", "b"]);
        assert_eq!(split_terms("  a   b  "), vec!["a", "b"]);
        assert_eq!(split_terms("\"wedding 2019\" notes"), vec!["wedding 2019", "notes"]);
        assert_eq!(split_terms("\"unterminated phrase"), vec!["unterminated phrase"]);
        assert!(split_terms("\"\"").is_empty());
    }

    fn words_tree(root_name: &str) -> Index {
        let mut ix = Index::new();
        let root = ix.add(NO_PARENT, root_name, true, 1);
        ix.set_root(root);
        let photos = ix.add(root, "photos", true, 2);
        let wedding = ix.add(photos, "wedding 2019", true, 3);
        ix.add(wedding, "IMG-1.jpg", false, 4);
        ix.add(wedding, "notes.txt", false, 5);
        ix.add(photos, "beach.JPG", false, 6);
        let docs = ix.add(root, "documents", true, 7);
        let invoice = ix.add(docs, "invoice", true, 8);
        ix.add(invoice, "followup-T004821.txt", false, 9);
        ix.add(invoice, "statement.pdf", false, 10);
        ix.add(docs, "invoice-summary.txt", false, 11);
        let cache = ix.add(root, ".cache", true, 12);
        ix.add(cache, "wedding-thumb.jpg", false, 13);
        let a = ix.add(root, "a", true, 14);
        ix.add(a, "b.txt", false, 15);
        let beta = ix.add(root, "beta", true, 16);
        ix.add(beta, "alpha.txt", false, 17);
        ix
    }

    /// Brute force: every term in the full path, plus the usual filters.
    fn reference(ix: &Index, q: &str, o: &SearchOpts) -> Vec<String> {
        let terms: Vec<String> = split_terms(q)
            .into_iter()
            .map(|t| if o.match_case { t } else { t.to_ascii_lowercase() })
            .collect();
        let mut out = Vec::new();
        for i in 0..ix.raw_len() as u32 {
            if !ix.accept(i, o) {
                continue;
            }
            let p = ix.path_of(i);
            let hay = if o.match_case { p.clone() } else { p.to_ascii_lowercase() };
            if terms.iter().all(|t| hay.contains(t.as_str())) {
                out.push(p);
            }
        }
        out.sort();
        out
    }

    #[test]
    fn multi_term_and_matches_brute_force_and_scan() {
        let queries = [
            "wedding jpg", "jpg wedding", "invoice txt", "documents invoice", "photos/ jpg",
            "photos/ wedding", "\"wedding 2019\" notes", "wedding 2019", "a b", "a/ b",
            "invoice/ txt", "INVOICE txt", "zzz jpg", "txt invoice documents", "cache jpg",
            "/home/ invoice", "home invoice", "hotos/wed jpg", "alpha beta", "jpg JPG",
        ];
        let optsets = [
            SearchOpts::default(),
            SearchOpts { match_case: true, ..SearchOpts::default() },
            SearchOpts { kind: Kind::Files, ..SearchOpts::default() },
            SearchOpts { kind: Kind::Folders, ..SearchOpts::default() },
            SearchOpts { hide_hidden: true, ..SearchOpts::default() },
            SearchOpts { in_path: true, ..SearchOpts::default() },
        ];
        for root_name in ["/home", "/home/alice"] {
            let plain = words_tree(root_name);
            let mut tri = words_tree(root_name);
            tri.build_trigrams();
            for q in queries {
                for o in &optsets {
                    let want = reference(&plain, q, o);
                    let mut scan = plain.search_opts(q, 1000, o).unwrap();
                    let mut fast = tri.search_opts(q, 1000, o).unwrap();
                    scan.sort();
                    fast.sort();
                    assert_eq!(scan, want, "scan: root {:?} query {:?} {:?}", root_name, q, o);
                    assert_eq!(fast, want, "trigram: root {:?} query {:?} {:?}", root_name, q, o);
                }
            }
        }
    }

    #[test]
    fn words_may_match_folders_as_well_as_the_name() {
        let mut ix = words_tree("/home");
        ix.build_trigrams();
        // "wedding" is only in the folder name of IMG-1.jpg, and both words
        // are in the name of the (hidden) thumbnail
        let mut hits = ix.search("wedding jpg", 10);
        hits.sort();
        assert_eq!(
            hits,
            vec!["/home/.cache/wedding-thumb.jpg", "/home/photos/wedding 2019/IMG-1.jpg"]
        );
        // order does not matter
        let mut rev = ix.search("jpg wedding", 10);
        rev.sort();
        assert_eq!(rev, hits);
        let visible = SearchOpts { hide_hidden: true, ..SearchOpts::default() };
        assert_eq!(
            ix.search_opts("wedding jpg", 10, &visible).unwrap(),
            vec!["/home/photos/wedding 2019/IMG-1.jpg"]
        );
        // quotes keep the space; unquoted, it is two words
        assert_eq!(ix.search("\"beta alpha\"", 10).len(), 0);
        assert_eq!(ix.search("beta alpha", 10), vec!["/home/beta/alpha.txt"]);
        // a single word is still a file-name match
        assert_eq!(ix.search("wedding", 10).len(), 2);
    }

    #[test]
    fn trigram_search_matches_the_scan_exactly() {
        let plain = varied();
        let mut tri = varied();
        tri.build_trigrams();
        for q in [
            "invoice", "INVOICE", "followup", "t004821", "T004821", ".txt", "watch",
            "watcher", "rs", "ab", "zzz", "unrelated", "e-r",
        ] {
            assert_eq!(
                plain.search(q, 100),
                tri.search(q, 100),
                "mismatch for query {:?}",
                q
            );
        }
    }

    #[test]
    fn entries_added_after_build_are_searchable() {
        let mut ix = varied();
        ix.build_trigrams();
        assert_eq!(ix.search("afterwards", 10).len(), 0);
        let root = ix.root_id();
        ix.add(root, "added-afterwards.log", false, 99);
        assert_eq!(ix.search("afterwards", 10).len(), 1);
        assert_eq!(ix.search("AFTERWARDS", 10).len(), 1);
    }

    #[test]
    fn deleted_entries_drop_out_of_trigram_results() {
        let mut ix = varied();
        ix.build_trigrams();
        let hits = ix.search("notes.txt", 10);
        assert_eq!(hits.len(), 1);
        let root = ix.root_id();
        let reports = ix.find_child(root, "reports").unwrap();
        let id = ix.find_child(reports, "notes.txt").unwrap();
        ix.remove(id);
        assert_eq!(ix.search("notes.txt", 10).len(), 0);
    }

    #[test]
    fn repeated_adds_do_not_duplicate() {
        let (mut ix, alice) = sample();
        ix.add(alice, "dup.txt", false, 90);
        ix.add(alice, "dup.txt", false, 90);
        ix.add(alice, "dup.txt", false, 90);
        assert_eq!(ix.search("dup.txt", 10).len(), 1);
    }

    #[test]
    fn overlapping_subtree_scans_do_not_duplicate() {
        // Mirrors the live path: a new tree is walked wholesale, then the
        // individual create events for its contents arrive and re-add them.
        let (mut ix, alice) = sample();
        let mut build = |ix: &mut Index| {
            let a = ix.add(alice, "a", true, 10);
            let b = ix.add(a, "b", true, 11);
            let c = ix.add(b, "c", true, 12);
            ix.add(c, "leaf.txt", false, 13);
        };
        build(&mut ix);
        build(&mut ix);
        build(&mut ix);
        assert_eq!(ix.search("leaf.txt", 10).len(), 1);
    }

    #[test]
    fn new_nested_dirs_are_indexed() {
        // the FSearch failure case, as a unit test
        let (mut ix, alice) = sample();
        let a = ix.add(alice, "a", true, 10);
        let b = ix.add(a, "b", true, 11);
        ix.add(b, "deep-new-file.txt", false, 12);
        assert_eq!(
            ix.search("deep-new-file", 10),
            vec!["/home/alice/a/b/deep-new-file.txt"]
        );
    }
}

impl Index {
    /// Walk the tree by path components. Used when a fanotify event names a
    /// directory whose inode we have not seen yet.
    pub fn resolve_path(&self, path: &str) -> Option<u32> {
        let cur0 = self.root;
        if cur0 == NO_PARENT {
            return None;
        }
        // The root entry stores its full mount path (e.g. "/home"), so strip
        // that prefix before walking components.
        let root_name: &str = &self.entries[cur0 as usize].name;
        let rest = if root_name == "/" {
            path
        } else {
            path.strip_prefix(root_name)?
        };
        let mut cur = cur0;
        for comp in rest.split('/').filter(|c| !c.is_empty()) {
            cur = self.find_child(cur, comp)?;
        }
        Some(cur)
    }

    pub fn root_id(&self) -> u32 {
        self.root
    }

    /// Every arena slot, dead ones included, for snapshotting.
    pub fn raw_entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn raw_len(&self) -> usize {
        self.entries.len()
    }

    /// Rebuild from a snapshot. `children` is derived from parent pointers;
    /// `dir_ino` is intentionally left empty and is repopulated by the
    /// reconciliation scan that runs at startup.
    pub fn from_snapshot(entries: Vec<Entry>, root: u32) -> Index {
        let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
        for (i, e) in entries.iter().enumerate() {
            if e.parent != NO_PARENT {
                children.entry(e.parent).or_default().push(i as u32);
            }
        }
        Index {
            entries,
            dir_ino: HashMap::new(),
            children,
            root,
            trigrams: HashMap::new(),
            trigrams_built: false,
        }
    }
}

/// Outcome of grafting a rescanned mount into the index.
pub struct GraftStats {
    pub total: usize,
    pub added: usize,
    pub removed: usize,
}

impl Index {
    /// Append an entry without registering its inode. Used for mounts that
    /// are rescanned rather than watched: they are a different filesystem, so
    /// their inode numbers would collide with the watched one's in `dir_ino`
    /// and misroute fanotify events.
    fn insert_foreign(&mut self, parent: u32, name: &str, is_dir: bool) -> u32 {
        let id = self.entries.len() as u32;
        if self.trigrams_built {
            for_each_trigram(name, |key| self.trigrams.entry(key).or_default().push(id));
        }
        self.entries.push(Entry {
            parent,
            name: name.into(),
            is_dir,
            alive: true,
        });
        self.children.entry(parent).or_default().push(id);
        id
    }

    /// Replace everything under `mount` with a fresh listing of it.
    ///
    /// For filesystems fanotify cannot watch -- an rclone FUSE mount of Google
    /// Drive, whose remote changes never pass through the kernel -- a periodic
    /// walk is the only source of truth. `walked` is pre-order
    /// (parent, name, is_dir): parent indexes an earlier element, or is
    /// NO_PARENT for the mount's own children.
    ///
    /// Entries still present are revived in place rather than re-created, so
    /// repeated rescans do not grow the arena; vanished ones just stay dead.
    /// `remove()` is deliberately not used: it prunes `dir_ino` per directory,
    /// which is quadratic across 26k directories.
    pub fn graft(&mut self, mount: u32, walked: &[(u32, String, bool)]) -> GraftStats {
        let mut was_alive: std::collections::HashSet<u32> = std::collections::HashSet::new();
        let mut stack: Vec<u32> = self.children.get(&mount).cloned().unwrap_or_default();
        while let Some(id) = stack.pop() {
            let e = &mut self.entries[id as usize];
            if e.alive {
                was_alive.insert(id);
            }
            e.alive = false;
            if let Some(kids) = self.children.get(&id) {
                stack.extend(kids.iter().copied());
            }
        }

        let mut ids: Vec<u32> = Vec::with_capacity(walked.len());
        let mut lookups: HashMap<u32, HashMap<Box<str>, u32>> = HashMap::new();
        let (mut added, mut unchanged) = (0usize, 0usize);
        for (parent_local, name, is_dir) in walked {
            let parent = if *parent_local == NO_PARENT {
                mount
            } else {
                ids[*parent_local as usize]
            };
            let existing = lookups
                .entry(parent)
                .or_insert_with(|| {
                    self.children
                        .get(&parent)
                        .map(|v| {
                            v.iter()
                                .map(|&k| (self.entries[k as usize].name.clone(), k))
                                .collect()
                        })
                        .unwrap_or_default()
                })
                .get(name.as_str())
                .copied();
            let id = match existing {
                Some(k) => {
                    let e = &mut self.entries[k as usize];
                    e.alive = true;
                    e.is_dir = *is_dir;
                    if was_alive.contains(&k) {
                        unchanged += 1;
                    } else {
                        added += 1;
                    }
                    k
                }
                None => {
                    added += 1;
                    self.insert_foreign(parent, name, *is_dir)
                }
            };
            ids.push(id);
        }
        self.entries[mount as usize].alive = true;
        GraftStats {
            total: walked.len(),
            added,
            removed: was_alive.len() - unchanged,
        }
    }
}
