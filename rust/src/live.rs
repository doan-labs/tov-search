//! The index as it is right now: the immutable base, a bitset of base
//! entries that are gone, and a small overlay of entries added since.
//!
//! Every FSEvents directory event is handled the same way: list that one
//! directory and diff it against what we have. The diff is idempotent, so
//! replaying history, duplicate events, and events that race a compaction
//! are all harmless.

use crate::index::{Index, enc_size};
use crate::walk::{self, FLAG_MOUNT, KIND_DIR, Listing, NONE, RawEnt};
use std::collections::{BTreeMap, HashMap};

#[derive(Clone, Copy)]
pub struct OEnt {
    pub kind: u8,
    pub size: u64,
    pub mtime: u32,
    /// `index::name_mask` of the entry's name, so a search rejects most of
    /// the overlay with one AND.
    pub mask: u64,
    /// Location prior of the nearest folder the base knows (set on insert).
    pub prior: i8,
    /// Its slot in `Live::flat` (set on insert).
    slot: u32,
}

impl OEnt {
    /// `path` may be the whole path or just the name.
    pub fn new(path: &[u8], kind: u8, size: u64, mtime: u32) -> OEnt {
        let name = &path[path.iter().rposition(|&b| b == b'/').map_or(0, |p| p + 1)..];
        OEnt { kind, size, mtime, mask: crate::index::name_mask(name), prior: 0, slot: 0 }
    }
}

/// The overlay laid out for searches: every entry's name mask in one array
/// to scan, its path and value in the same slot. A removed entry's slot is
/// reused; until then its mask is 0, which no name has.
#[derive(Default)]
pub struct Flat {
    pub masks: Vec<u64>,
    pub items: Vec<(Vec<u8>, OEnt)>,
    free: Vec<u32>,
}

impl Flat {
    fn set(&mut self, path: &[u8], e: OEnt) {
        let s = e.slot as usize;
        if s == self.masks.len() {
            self.masks.push(0);
            self.items.push((Vec::new(), e));
        }
        self.masks[s] = e.mask;
        if self.items[s].0 != path {
            self.items[s].0 = path.to_vec();
        }
        self.items[s].1 = e;
    }

    fn clear(&mut self, e: OEnt) {
        self.masks[e.slot as usize] = 0;
        self.items[e.slot as usize].0 = Vec::new();
        self.free.push(e.slot);
    }
}

pub struct Live {
    pub base: Index,
    dead: Vec<u64>,
    pub dead_count: usize,
    pub over: BTreeMap<Vec<u8>, OEnt>,
    /// `over` laid out for searches.
    pub flat: Flat,
    /// Last FSEvents id fully applied.
    pub event_id: u64,
    /// Paths of directories added or removed since last drained; the
    /// content index re-syncs these whole subtrees.
    pub trees: Vec<Vec<u8>>,
    /// The last name search's scored names, reused while you type.
    pub names_cache: crate::query::NameCache,
    /// Pos-empty queries' `ok`/`negbits` name bitsets, reused when the
    /// name-side predicates repeat identically (e.g. a stat-filter-only
    /// edit or a repeated query).
    pub posneg_cache: crate::query::PosNegCache,
    /// Wall-clock second up to which every change is known applied.
    pub synced_at: u32,
    /// Overlay folder -> prior of its nearest base folder.
    priors: HashMap<Vec<u8>, i8>,
    /// Per base entry: dir_prior of its parent + rank_tweaks of its own
    /// name/kind/mtime — everything a query score adds on top of the name
    /// score, baked so a scan reads one array instead of three.
    pub stat: Vec<i32>,
    /// Entries ordered by `stat` (desc), entry index (asc); `ostat[p]` is
    /// `stat[order[p]]`. A query whose score bound is known can top-k down
    /// this order and stop once the bound dips below its k-th best.
    pub order: Vec<u32>,
    pub ostat: Vec<i32>,
    /// `ent_name` in `order` (`o_en[p] == ent_name[order[p]]`), so a walk
    /// down `order` reads name ids sequentially instead of gathering them.
    pub o_en: Vec<u32>,
    /// `name_ents` with each name's list reordered by `stat` desc (same
    /// offsets), and `o_nestat[j] == stat[o_ne[j]]`: a name's best entry
    /// leads its list. `peak[id]` is that first stat (i16 covers it).
    pub o_ne: Vec<u32>,
    pub o_nestat: Vec<i16>,
    pub peak: Vec<i16>,
    /// `peak` minus the name's length penalty: what a name's best entry
    /// scores beyond its query-dependent part, for word-at-a-time pruning.
    pub ubase: Vec<i16>,
    /// Transposed name masks: `char_fit[b]` is the bitset of names whose
    /// `name_mask` carries char-class bit `b` (bits 0..41). A strict token's
    /// exact fit-set is the AND of its bits' bitsets — no mask rescan.
    pub char_fit: Vec<u64>,
    /// `char_fit[b]`'s popcount: how many names hold char class `b` — the
    /// rarity estimate driving multi-token searches (`driven`).
    pub class_counts: Vec<u32>,
    /// Same transpose over the mask's word-start bits (41..64, 23 buckets):
    /// `start_fit[h]` is the bitset of names with a word starting with a
    /// byte hashing to `h`. `ranked` skips every name without one.
    pub start_fit: Vec<u64>,
    /// Like `char_fit`, over each name's first half (`first_half[b]`) and
    /// second half (split at `len / 2`). A token's chars appear in order,
    /// so a name can only match if some prefix of the token fits its first
    /// half and the rest its second (`query::TokenBits`).
    pub first_half: Vec<u64>,
    pub second_half: Vec<u64>,
    /// `double_fit[b]`: names with char class `b` at least twice (a token
    /// with it twice needs it twice); `double_counts` their popcounts.
    pub double_fit: Vec<u64>,
    pub double_counts: Vec<u32>,
    /// First char of each name (past a leading dot): low 6 bits = its
    /// char-class bit index, bit 6 = the name starts with '.' (the `off`
    /// `single_score` uses). With `first_fit`, `stem_pos` and `name_len8`
    /// a one-char query's placed score needs no name text at all.
    pub first_cls: Vec<u8>,
    /// `first_fit[b]`: names whose first char maps to class bit `b`.
    pub first_fit: Vec<u64>,
    /// Last '.' past `off`, else name length (clamped to u16).
    pub stem_pos: Vec<u16>,
    /// Name length clamped to u8 (>=255 means >=255; the score only needs
    /// `min(len,80)`).
    pub name_len8: Vec<u8>,
    /// Folded extension — bytes after the last '.', or "" for a trailing
    /// dot — mapped to the names carrying it, ids ascending. Dotless names
    /// match no extension and appear under no key (`ext_ok`'s rule).
    pub ext_names: HashMap<Box<[u8]>, Vec<u32>>,
    /// Per `OBLK` run of `order`: the size, mtime and kind ranges of its
    /// entries, so a stat-filter walk skips runs no entry of which passes.
    pub oblk: Vec<Blk>,
    /// Names some dir carries: a multi-token table restricted to one
    /// token's cover still scores these, for the dir memo.
    pub dir_names: Vec<u64>,
    /// Per dir: its name id (`ent_name[dir_entry[k]]`), read sequentially.
    pub dir_name: Vec<u32>,
    /// Recent tokens' covers (see `query::Cover`).
    pub cover_cache: crate::query::CoverCache,
}

pub const OBLK: usize = 256;

#[derive(Clone, Copy)]
pub struct Blk {
    pub size: (u64, u64),
    pub mtime: (u32, u32),
    /// Bit `kind & 3` set for each kind present.
    pub kinds: u8,
}

/// The per-entry static part of a query score (see `Live::stat`): each
/// entry's parent dir prior plus the tweaks its own name/kind/mtime earn.
/// Computed once; `now` is the same clock ranking uses.
fn stat_table(idx: &Index) -> Vec<i32> {
    use rayon::prelude::*;
    let now = crate::query::now_secs();
    let (en, kind, mtime, parent, dp) = (idx.ent_name(), idx.kind(), idx.mtime(), idx.parent(), idx.dir_prior());
    // Tweak flags are per distinct name; fold them up once per name.
    let mut flags = vec![0u8; idx.u];
    flags
        .par_iter_mut()
        .enumerate()
        .with_min_len(1 << 14)
        .for_each(|(id, f)| *f = crate::query::name_flags(idx.uname(id as u32)));
    let mut stat = vec![0i32; idx.n];
    stat.par_iter_mut()
        .enumerate()
        .with_min_len(1 << 15)
        .for_each(|(i, s)| *s = dp[parent[i] as usize] as i32 + crate::query::rank_tweaks(flags[en[i] as usize], kind[i], mtime[i], now));
    stat
}

/// `rows` bitsets over the names, `nw` words each, row-major: row `r` holds
/// the names `k` whose `bits(k)` has bit `r`. Built a word of names at a
/// time, then laid out by row.
fn transpose(u: usize, rows: usize, bits: impl Fn(usize) -> u128 + Sync) -> Vec<u64> {
    use rayon::prelude::*;
    let nw = u.div_ceil(64);
    let mut tmp = vec![0u64; nw * rows];
    tmp.par_chunks_mut(rows).enumerate().with_min_len(64).for_each(|(w, t)| {
        for k in w * 64..(w * 64 + 64).min(u) {
            let mut x = bits(k);
            while x != 0 {
                t[x.trailing_zeros() as usize] |= 1 << (k % 64);
                x &= x - 1;
            }
        }
    });
    let mut out = vec![0u64; nw * rows];
    out.par_chunks_mut(nw).enumerate().for_each(|(r, row)| row.iter_mut().enumerate().for_each(|(w, v)| *v = tmp[w * rows + r]));
    out
}

/// `Live::order`/`ostat`: counting sort — stat has a few hundred distinct
/// values, so one pass of counts and one scatter, both cheap.
fn stat_order(stat: &[i32]) -> (Vec<u32>, Vec<i32>) {
    let (mut lo, mut hi) = (i32::MAX, i32::MIN);
    for &s in stat {
        lo = lo.min(s);
        hi = hi.max(s);
    }
    let width = (hi - lo + 1) as usize;
    let mut cnt = vec![0usize; width];
    for &s in stat {
        cnt[(s - lo) as usize] += 1;
    }
    // Bucket starts in the stat-descending layout (idx stays ascending
    // inside a bucket, since the scatter walks i upward).
    let mut off = vec![0usize; width];
    let mut total = 0usize;
    for b in (0..width).rev() {
        off[b] = total;
        total += cnt[b];
    }
    let mut order = vec![0u32; stat.len()];
    let mut at = off;
    for (i, &s) in stat.iter().enumerate() {
        let b = (s - lo) as usize;
        order[at[b]] = i as u32;
        at[b] += 1;
    }
    let ostat: Vec<i32> = order.iter().map(|&i| stat[i as usize]).collect();
    (order, ostat)
}

/// What `Live::fetch` read from disk for one folder update.
pub struct Fetched {
    path: Vec<u8>,
    recursive: bool,
    blocked: bool,
    attrs: Option<OEnt>,
    listing: Option<Listing>,
    scans: HashMap<Vec<u8>, Vec<Listing>>,
}

pub enum Applied {
    Done,
    /// The whole disk needs rescanning (history lost at the root).
    Rebuild,
}

impl Live {
    pub fn new(base: Index) -> Live {
        base.prefault();
        let words = base.n.div_ceil(64);
        let event_id = base.event_id;
        let synced_at = base.synced_at;
        let stat = stat_table(&base);
        let (order, ostat) = stat_order(&stat);
        let en = base.ent_name();
        let mut o_en = vec![0u32; order.len()];
        {
            use rayon::prelude::*;
            o_en.par_iter_mut().enumerate().with_min_len(1 << 15).for_each(|(p, v)| *v = en[order[p] as usize]);
        }
        // Each name's entry list in stat order too: `order` is global
        // stat-desc, so scattering entries into their name's list keeps
        // every list stat-desc.
        let (ne_off, mut o_ne, mut o_nestat) = (base.name_ents_off(), vec![0u32; base.n], vec![0i16; base.n]);
        let mut cur: Vec<u32> = ne_off[..base.u].to_vec();
        for &i in &order {
            let id = en[i as usize] as usize;
            let pos = cur[id] as usize;
            cur[id] += 1;
            o_ne[pos] = i;
            o_nestat[pos] = stat[i as usize] as i16;
        }
        let mut peak = vec![i16::MIN; base.u];
        for id in 0..base.u {
            if ne_off[id] < ne_off[id + 1] {
                peak[id] = o_nestat[ne_off[id] as usize];
            }
        }
        let nw = base.u.div_ceil(64);
        let mask = base.name_mask();
        let char_fit = transpose(base.u, 41, |k| (mask[k] & ((1 << 41) - 1)) as u128);
        let counts = |fit: &[u64]| -> Vec<u32> { fit.chunks(nw).map(|set| set.iter().map(|w| w.count_ones()).sum()).collect() };
        let class_counts = counts(&char_fit);
        let start_fit = transpose(base.u, 23, |k| (mask[k] >> 41) as u128);
        let mut first_half = transpose(base.u, 3 * 41, |k| {
            let name = base.uname(k as u32);
            let (a, b) = name.split_at(name.len() / 2);
            let m = |s: &[u8]| crate::index::char_mask(s) as u128;
            m(a) | m(b) << 41 | (crate::query::doubled(name) as u128) << 82
        });
        let double_fit = first_half.split_off(82 * nw);
        let second_half = first_half.split_off(41 * nw);
        first_half.shrink_to_fit();
        let double_counts = counts(&double_fit);
        let (mut first_cls, mut stem_pos, mut name_len8) =
            (vec![0u8; base.u], vec![0u16; base.u], vec![0u8; base.u]);
        {
            use rayon::prelude::*;
            first_cls
                .par_iter_mut()
                .zip(stem_pos.par_iter_mut())
                .zip(name_len8.par_iter_mut())
                .enumerate()
                .with_min_len(1 << 14)
                .for_each(|(k, ((fc, sp), nl))| {
                    let name = base.uname(k as u32);
                    let off = (name.len() > 1 && name[0] == b'.') as usize;
                    let fb = name.get(off).copied().unwrap_or(0);
                    *fc = crate::index::char_bit(fb).trailing_zeros() as u8 | ((off as u8) << 6);
                    *nl = name.len().min(255) as u8;
                    *sp = memchr::memrchr(b'.', name)
                        .filter(|&p| p > off)
                        .unwrap_or(name.len())
                        .min(u16::MAX as usize) as u16;
                });
        }
        let mut first_fit = vec![0u64; 41 * nw];
        {
            use rayon::prelude::*;
            first_fit.par_chunks_mut(nw).enumerate().for_each(|(b, set)| {
                for (k, &c) in first_cls.iter().enumerate() {
                    if c & 63 == b as u8 {
                        set[k / 64] |= 1 << (k % 64);
                    }
                }
            });
        }
        let ext_names = {
            use rayon::prelude::*;
            (0..base.u.div_ceil(1 << 15))
                .into_par_iter()
                .map(|c| {
                    let mut m: HashMap<Box<[u8]>, Vec<u32>> = HashMap::new();
                    for k in c * (1 << 15)..((c + 1) * (1 << 15)).min(base.u) {
                        let name = base.uname(k as u32);
                        if let Some(dot) = memchr::memrchr(b'.', name) {
                            m.entry(name[dot + 1..].iter().map(|&b| crate::query::fold(b)).collect()).or_default().push(k as u32);
                        }
                    }
                    m
                })
                .reduce(HashMap::new, |mut a, b| {
                    for (k, v) in b {
                        a.entry(k).or_default().extend(v);
                    }
                    a
                })
        };
        let oblk = {
            use rayon::prelude::*;
            let (size, mtime, kind) = (base.size_raw(), base.mtime(), base.kind());
            order
                .par_chunks(OBLK)
                .map(|run| {
                    let mut b = Blk { size: (u64::MAX, 0), mtime: (u32::MAX, 0), kinds: 0 };
                    for &i in run {
                        let (s, m) = (crate::index::dec_size(size[i as usize]), mtime[i as usize]);
                        b.size = (b.size.0.min(s), b.size.1.max(s));
                        b.mtime = (b.mtime.0.min(m), b.mtime.1.max(m));
                        b.kinds |= 1 << (kind[i as usize] & 3);
                    }
                    b
                })
                .collect()
        };
        let dir_name: Vec<u32> = base.dir_entry().iter().map(|&e| en[e as usize]).collect();
        let mut dir_names = vec![0u64; nw];
        for &id in &dir_name[1..] {
            dir_names[id as usize >> 6] |= 1 << (id & 63);
        }
        Live {
            base,
            dead: vec![0; words],
            dead_count: 0,
            over: BTreeMap::new(),
            flat: Flat::default(),
            event_id,
            trees: Vec::new(),
            names_cache: Default::default(),
            posneg_cache: Default::default(),
            synced_at,
            priors: HashMap::new(),
            stat,
            order,
            ostat,
            o_en,
            o_ne,
            o_nestat,
            ubase: peak.iter().zip(&name_len8).map(|(&p, &l)| p.saturating_sub(l.min(80) as i16 / 3)).collect(),
            peak,
            char_fit,
            class_counts,
            start_fit,
            first_half,
            second_half,
            double_fit,
            double_counts,
            first_cls,
            first_fit,
            stem_pos,
            name_len8,
            ext_names,
            oblk,
            dir_names,
            dir_name,
            cover_cache: Default::default(),
        }
    }

    /// Put an entry in the overlay, stamping the prior it ranks with.
    fn put(&mut self, path: Vec<u8>, mut e: OEnt) {
        let cut = path.iter().rposition(|&b| b == b'/').unwrap_or(0);
        e.prior = match self.priors.get(&path[..cut]) {
            Some(&p) => p,
            None => {
                let mut up = &path[..cut];
                let mut prior = 0;
                while !up.is_empty() {
                    if let Some(d) = self.base.lookup(up).and_then(|e| self.base.dir_of(e)) {
                        prior = self.base.dir_prior()[d as usize];
                        break;
                    }
                    up = &up[..up.iter().rposition(|&b| b == b'/').unwrap_or(0)];
                }
                self.priors.insert(path[..cut].to_vec(), prior);
                prior
            }
        };
        e.slot = match self.over.get(&path) {
            Some(old) => old.slot,
            None => self.flat.free.pop().unwrap_or(self.flat.masks.len() as u32),
        };
        self.flat.set(&path, e);
        self.over.insert(path, e);
    }

    #[inline]
    pub fn is_dead(&self, i: u32) -> bool {
        self.dead[i as usize >> 6] & (1 << (i & 63)) != 0
    }

    fn kill(&mut self, i: u32) {
        let w = &mut self.dead[i as usize >> 6];
        if *w & (1 << (i & 63)) == 0 {
            *w |= 1 << (i & 63);
            self.dead_count += 1;
        }
    }

    fn kill_subtree(&mut self, e: u32) {
        self.kill(e);
        if let Some(d) = self.base.dir_of(e) {
            let (a, b) = (self.base.dir_start()[d as usize], self.base.dir_end()[d as usize]);
            for i in a..b {
                self.kill(i);
            }
        }
    }

    fn drop_over_subtree(&mut self, path: &[u8]) {
        if let Some(o) = self.over.remove(path) {
            self.flat.clear(o);
        }
        let (lo, hi) = subtree_bounds(path);
        let keys: Vec<Vec<u8>> = self.over.range(lo..hi).map(|(k, _)| k.clone()).collect();
        for k in keys {
            if let Some(o) = self.over.remove(&k) {
                self.flat.clear(o);
            }
        }
    }

    /// Alive base entry for a path, if the base has one.
    fn base_alive(&self, path: &[u8]) -> Option<u32> {
        self.base.lookup(path).filter(|&e| !self.is_dead(e))
    }

    fn remove_path(&mut self, path: &[u8]) {
        self.trees.push(path.to_vec());
        if let Some(e) = self.base_alive(path) {
            self.kill_subtree(e);
        }
        self.drop_over_subtree(path);
    }

    /// Bring one directory (or, with `recursive`, its whole subtree) in line
    /// with the disk.
    pub fn apply_dir(&mut self, path: &[u8], recursive: bool) -> Applied {
        let f = self.fetch(path, recursive);
        self.apply(f)
    }

    /// The disk half of `apply_dir`: list the folder (or stat it) and scan
    /// any folder that will be new to the index. Needs only `&self`, so the
    /// caller does this under a read lock and searches keep answering.
    pub fn fetch(&self, path: &[u8], recursive: bool) -> Fetched {
        let p = normalize(path);
        let mut f = Fetched { path: p, recursive, blocked: false, attrs: None, listing: None, scans: HashMap::new() };
        // Not even an lstat inside folders we may not touch.
        if walk::blocked(&f.path) {
            f.blocked = true;
            return f;
        }
        if recursive {
            if f.path != b"/" {
                f.attrs = lstat(&f.path);
                if f.attrs.is_some_and(|a| a.kind & 3 == KIND_DIR && a.kind & FLAG_MOUNT == 0) {
                    f.scans.insert(f.path.clone(), walk::scan(&f.path, 4));
                }
            }
            return f;
        }
        let Some(listing) = walk::list_one(&f.path) else {
            f.attrs = lstat(&f.path);
            return f;
        };
        // Children that will be added as folders get their subtree scanned now.
        let cur = self.current_children(&f.path);
        for r in &listing.ents {
            if r.kind & 3 != KIND_DIR || r.kind & FLAG_MOUNT != 0 {
                continue;
            }
            let name = &listing.names[r.name_off as usize..][..r.name_len as usize];
            let was_dir = match cur.get(name) {
                Some(Some(c)) => self.base.kind()[*c as usize] & 3 == KIND_DIR,
                Some(None) => self.over.get(&join(&f.path, name)).is_some_and(|o| o.kind & 3 == KIND_DIR),
                None => false,
            };
            if !was_dir {
                let child = join(&f.path, name);
                let ls = walk::scan(&child, 4);
                f.scans.insert(child, ls);
            }
        }
        f.listing = Some(listing);
        f
    }

    /// Children the index holds for `p`: name -> base entry, or None for an
    /// overlay entry.
    fn current_children(&self, p: &[u8]) -> HashMap<Vec<u8>, Option<u32>> {
        let mut cur: HashMap<Vec<u8>, Option<u32>> = HashMap::new();
        if let Some(d) = self.base_alive(p).and_then(|e| self.base.dir_of(e)) {
            for c in self.base.children(d) {
                if !self.is_dead(c as u32) {
                    cur.insert(self.base.name(c).to_vec(), Some(c as u32));
                }
            }
        }
        let (lo, hi) = subtree_bounds(p);
        let plen = lo.len();
        for k in self.over.range(lo..hi).map(|(k, _)| k) {
            if !k[plen..].contains(&b'/') {
                cur.insert(k[plen..].to_vec(), None);
            }
        }
        cur
    }

    /// The memory half of `apply_dir`: diff what `fetch` read into the index.
    pub fn apply(&mut self, mut f: Fetched) -> Applied {
        if f.blocked {
            return Applied::Done;
        }
        let p = std::mem::take(&mut f.path);
        if f.recursive {
            if p == b"/" {
                return Applied::Rebuild;
            }
            self.remove_path(&p);
            if let Some(attrs) = f.attrs {
                let scan = f.scans.remove(&p);
                self.add_new(p, attrs, scan);
            }
            return Applied::Done;
        }
        let Some(listing) = f.listing else {
            if f.attrs.is_none() {
                self.remove_path(&p);
            }
            return Applied::Done;
        };
        let mut cur = self.current_children(&p);
        for r in &listing.ents {
            let name = &listing.names[r.name_off as usize..][..r.name_len as usize];
            let child = join(&p, name);
            let now = OEnt::new(name, r.kind, r.size, r.mtime);
            match cur.remove(name) {
                None => {
                    let scan = f.scans.remove(&child);
                    self.add_new(child, now, scan)
                }
                Some(Some(c)) => {
                    let c = c as usize;
                    let was_kind = self.base.kind()[c];
                    if was_kind & 3 != now.kind & 3 {
                        self.kill_subtree(c as u32);
                        let scan = f.scans.remove(&child);
                        self.add_new(child, now, scan);
                    } else if now.kind & 3 != KIND_DIR && (self.base.size_raw()[c] != enc_size(now.size) || self.base.mtime()[c] != now.mtime) {
                        self.kill(c as u32);
                        self.put(child, now);
                    }
                }
                Some(None) => {
                    let old = self.over[&child];
                    if old.kind & 3 != now.kind & 3 {
                        self.drop_over_subtree(&child);
                        let scan = f.scans.remove(&child);
                        self.add_new(child, now, scan);
                    } else if (old.kind, old.size, old.mtime) != (now.kind, now.size, now.mtime) {
                        self.put(child, now);
                    }
                }
            }
        }
        for (name, c) in cur {
            let child = join(&p, &name);
            match c {
                Some(c) => {
                    if self.base.kind()[c as usize] & 3 == KIND_DIR {
                        self.trees.push(child);
                    }
                    self.kill_subtree(c)
                }
                None => {
                    if self.over.get(&child).is_some_and(|o| o.kind & 3 == KIND_DIR) {
                        self.trees.push(child.clone());
                    }
                    self.drop_over_subtree(&child)
                }
            }
        }
        Applied::Done
    }

    /// Add an entry and, if it is a directory, its subtree (`scan`, read
    /// ahead by `fetch`; scanned here if missing).
    fn add_new(&mut self, path: Vec<u8>, e: OEnt, scan: Option<Vec<Listing>>) {
        let is_dir = e.kind & 3 == KIND_DIR && e.kind & FLAG_MOUNT == 0;
        self.put(path.clone(), e);
        if !is_dir {
            return;
        }
        self.trees.push(path.clone());
        let ls = scan.unwrap_or_else(|| walk::scan(&path, 4));
        for_each_path(&ls, &path, |p, r| {
            let e = OEnt::new(&p, r.kind, r.size, r.mtime);
            self.put(p, e);
        });
    }

    /// Folders whose listing may have changed since `since` (a wall-clock
    /// second): adding, removing or renaming an entry bumps its folder's
    /// mtime. One lstat per folder, in parallel, under a read lock;
    /// relisting the few that changed is `apply_dir`'s job. This is how lost
    /// FSEvents history is recovered without crawling the whole disk.
    pub fn changed_dirs(&self, since: u32) -> Vec<Vec<u8>> {
        let mut out = self.dirs_where(|p| !walk::blocked(p) && lstat(p).is_some_and(|o| o.mtime >= since));
        if lstat(b"/").is_some_and(|o| o.mtime >= since) {
            out.push(b"/".to_vec());
        }
        out.sort();
        out
    }

    /// Every folder the index descends into except `/`, for which `keep`
    /// holds. `keep` runs in parallel on the stat pool.
    pub fn dirs_where(&self, keep: impl Fn(&[u8]) -> bool + Sync + Send) -> Vec<Vec<u8>> {
        use rayon::prelude::*;
        let idx = &self.base;
        let de = idx.dir_entry();
        let check = &keep;
        let pool = stat_pool();
        let mut out: Vec<Vec<u8>> = pool.install(|| {
            (1..idx.d)
                .into_par_iter()
                .with_min_len(1024)
                .filter(|&k| !self.is_dead(de[k]))
                .map_init(Vec::new, |buf, k| {
                    idx.path(de[k] as usize, buf);
                    check(buf).then(|| buf.clone())
                })
                .flatten()
                .collect()
        });
        out.extend(self.over.iter().filter(|(p, o)| o.kind & 3 == KIND_DIR && o.kind & FLAG_MOUNT == 0 && check(p)).map(|(p, _)| p.clone()));
        out
    }

    /// Everything alive, as listings for Index::build.
    pub fn to_listings(&self) -> Vec<Listing> {
        let mut by_parent: HashMap<&[u8], Vec<(&[u8], OEnt)>> = HashMap::new();
        for (k, v) in &self.over {
            let cut = k.iter().rposition(|&b| b == b'/').unwrap_or(0);
            let parent: &[u8] = if cut == 0 { b"/" } else { &k[..cut] };
            by_parent.entry(parent).or_default().push((&k[cut + 1..], *v));
        }
        let mut out = Vec::new();
        // (base dir id if alive in base, path, listing id)
        let mut stack: Vec<(Option<u32>, Vec<u8>, u32)> = vec![(Some(0), b"/".to_vec(), 0)];
        let mut next = 1u32;
        while let Some((bd, path, id)) = stack.pop() {
            let mut l = Listing { id, names: Vec::new(), ents: Vec::new() };
            let mut push = |l: &mut Listing, name: &[u8], kind: u8, size: u64, mtime: u32, child_path: Option<Vec<u8>>, bdir: Option<u32>| {
                let child = match child_path {
                    Some(cp) => {
                        let c = next;
                        next += 1;
                        stack.push((bdir, cp, c));
                        c
                    }
                    None => NONE,
                };
                l.ents.push(RawEnt { name_off: l.names.len() as u32, name_len: name.len() as u16, kind, size, mtime, child });
                l.names.extend_from_slice(name);
            };
            if let Some(d) = bd {
                for c in self.base.children(d) {
                    if self.is_dead(c as u32) {
                        continue;
                    }
                    let name = self.base.name(c);
                    let k = self.base.kind()[c];
                    let sub = self.base.dir_of(c as u32);
                    let cp = sub.map(|_| join(&path, name));
                    push(&mut l, name, k, self.base.size_of(c), self.base.mtime()[c], cp, sub);
                }
            }
            if let Some(kids) = by_parent.get(path.as_slice()) {
                for &(name, o) in kids {
                    let descend = o.kind & 3 == KIND_DIR && o.kind & FLAG_MOUNT == 0;
                    push(&mut l, name, o.kind, o.size, o.mtime, descend.then(|| join(&path, name)), None);
                }
            }
            out.push(l);
        }
        out
    }
}

/// Threads for bulk lstat: path lookups scale further than opens do.
pub fn stat_pool() -> rayon::ThreadPool {
    rayon::ThreadPoolBuilder::new().num_threads(12).start_handler(|_| crate::no_materialize()).build().unwrap()
}

/// Visit every entry of a scan with its full path.
pub fn for_each_path(ls: &[Listing], root: &[u8], mut f: impl FnMut(Vec<u8>, &RawEnt)) {
    let mut by_id: HashMap<u32, &Listing> = HashMap::with_capacity(ls.len());
    for l in ls {
        by_id.insert(l.id, l);
    }
    let mut stack = vec![(0u32, root.to_vec())];
    while let Some((id, path)) = stack.pop() {
        let Some(l) = by_id.get(&id) else { continue };
        for r in &l.ents {
            let p = join(&path, &l.names[r.name_off as usize..][..r.name_len as usize]);
            if r.child != NONE {
                stack.push((r.child, p.clone()));
            }
            f(p, r);
        }
    }
}

pub(crate) fn normalize(path: &[u8]) -> Vec<u8> {
    let mut p = path.to_vec();
    while p.len() > 1 && p.last() == Some(&b'/') {
        p.pop();
    }
    p
}

pub fn join(dir: &[u8], name: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(dir.len() + 1 + name.len());
    p.extend_from_slice(dir);
    if dir != b"/" {
        p.push(b'/');
    }
    p.extend_from_slice(name);
    p
}

/// Key range holding everything strictly under `path`.
fn subtree_bounds(path: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let lo = join(path, b"");
    let mut hi = lo.clone();
    *hi.last_mut().unwrap() += 1; // '/' + 1 == '0'
    (lo, hi)
}

pub fn lstat(path: &[u8]) -> Option<OEnt> {
    let c = std::ffi::CString::new(path).ok()?;
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::lstat(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let kind = match st.st_mode & libc::S_IFMT {
        libc::S_IFREG => walk::KIND_FILE,
        libc::S_IFDIR => KIND_DIR,
        libc::S_IFLNK => walk::KIND_LINK,
        _ => walk::KIND_OTHER,
    };
    Some(OEnt::new(path, kind, st.st_size as u64, st.st_mtime.clamp(0, u32::MAX as i64) as u32))
}
