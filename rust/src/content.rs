//! Content search: a trigram index over the user's text files.
//!
//! Segments are immutable, mmap'd files: a doc table plus, per trigram, the
//! ids of the docs containing it (delta varints). A query becomes an AND/OR
//! of trigrams, the posting lists pick candidate files, and the candidates
//! are read fresh from disk and matched for real. Results therefore never
//! show stale content; only candidate selection can trail a file written
//! in the last couple of seconds.
//!
//! The index is kept in sync by diffing, exactly like the name index: for a
//! directory (or subtree), compare the eligible files the live name index
//! knows about with the docs we hold, reindex what changed, tombstone what
//! went away. The first build is just a sync of $HOME.

use crate::index::{HDR, as_bytes, fields, header, layout, sec};
use crate::live::{Live, join};
use crate::query::{GrepMode, Query, fold};
use crate::walk::KIND_FILE;
use memmap2::Mmap;
use rayon::prelude::*;
use regex::bytes::{Regex, RegexBuilder};
use regex_syntax::hir::{Class, Hir, HirKind};
use std::collections::HashMap;
use std::io::Write;
use std::ops::Range;
use std::path::{Path, PathBuf};

pub const MAX_FILE: u64 = 1 << 20;
/// File bytes per segment build; bounds the build's transient memory.
const SEG_BYTES: u64 = 64 << 20;
/// Largest merge, in posting bytes; bounds the merge's transient memory.
const MERGE_CAP: usize = 96 << 20;
/// Current on-disk format (BF-owned): FSCSEG03's layout plus per-doc bloom
/// filters and end-of-file pair keys.
const MAGIC: &[u8; 8] = b"BFCSEG01";
/// The previous format, still loaded: it has no bloom sections and no
/// end-of-file pair keys. Segments of it answer every probe "maybe" and a
/// pair plan as "every doc", so old segments keep their exact answers while
/// missing only the new shortcuts.
const MAGIC_V3: &[u8; 8] = b"FSCSEG03";
/// tri_off high bit: this trigram's list is a bitset over the segment's docs
/// (cheaper than varints once more than 1 in 8 docs contain it).
const BITSET: u32 = 1 << 31;

/// Directory names whose subtrees are generated, vendored, or caches.
#[rustfmt::skip]
const SKIP_DIRS: &[&[u8]] = &[
    b"node_modules", b".git", b"target", b"DerivedData", b"__pycache__", b".venv", b"venv", b"site-packages", b"Pods",
    b".next", b".turbo", b".cache", b"Library", b"dist", b"build", b".build", b".rustup", b".cargo", b".npm", b".bun",
    b".nvm", b"vendor", b".pnpm-store", b"coverage", b".Trash", b".svn", b".hg", b".gradle", b".m2", b".pyenv",
    b".rbenv", b".gem", b".conda", b"miniconda3", b"anaconda3", b".docker", b".orbstack", b".colima", b".lima",
    b".ollama", b".android", b".expo", b".terraform.d", b".wrangler", b".vscode-server", b"cache", b"Cache", b"caches",
    b"Caches",
];

/// Home-relative trees that are dependencies or app data, not your files.
#[rustfmt::skip]
const SKIP_UNDER_HOME: &[&[u8]] = &[
    b"go/pkg", b".cursor/extensions", b".vscode/extensions", b".local/share", b".local/state", b".config/gcloud", b".codex/.tmp",
];

/// Package/library bundles: their insides are app data.
#[rustfmt::skip]
const SKIP_SUFFIXES: &[&[u8]] = &[
    b".app", b".photoslibrary", b".library", b".lrlibrary", b".musiclibrary", b".tvlibrary", b".imovielibrary",
    b".xcassets", b".framework", b".bundle", b".xcarchive", b".xcresult", b".dSYM", b".salon", b".lrdata",
];

#[rustfmt::skip]
const TEXT_EXTS: &[&[u8]] = &[
    b"rs", b"c", b"h", b"cc", b"cpp", b"cxx", b"hpp", b"hh", b"m", b"mm", b"swift", b"go", b"py", b"pyi", b"js", b"mjs", b"cjs",
    b"ts", b"mts", b"cts", b"tsx", b"jsx", b"java", b"kt", b"kts", b"scala", b"rb", b"php", b"cs", b"fs", b"sh", b"zsh", b"bash",
    b"fish", b"lua", b"sql", b"html", b"htm", b"css", b"scss", b"sass", b"less", b"json", b"jsonc", b"json5", b"yaml", b"yml",
    b"toml", b"xml", b"vue", b"svelte", b"astro", b"zig", b"nim", b"hs", b"ml", b"mli", b"ex", b"exs", b"erl", b"clj", b"dart",
    b"r", b"jl", b"md", b"mdx", b"markdown", b"txt", b"text", b"rst", b"org", b"tex", b"csv", b"tsv", b"ini", b"cfg", b"conf",
    b"env", b"properties", b"plist", b"metal", b"glsl", b"wgsl", b"hlsl", b"proto", b"graphql", b"gql", b"nix", b"tf", b"hcl",
    b"gradle", b"cmake", b"mk", b"make", b"dockerfile", b"log", b"jsonl", b"ndjson", b"diff", b"patch", b"srt", b"vtt", b"rtf",
    b"svg", b"pl", b"pm", b"ps1", b"bat", b"vim", b"el", b"lisp", b"scm", b"rkt", b"elm", b"purs", b"sol", b"v", b"sv", b"vhd",
    b"asm", b"s", b"d", b"cr", b"pas", b"f90", b"cmd", b"service", b"desktop", b"gitignore", b"editorconfig", b"lock", b"sum",
];

pub struct Segment {
    map: Mmap,
    pub id: u64,
    pub ndocs: usize,
    ndocs1: usize,
    ntri: usize,
    ntri1: usize,
    plen: usize,
    paths_len: usize,
    /// `MAGIC` (current) sections are present: per-doc bloom filters and
    /// end-of-file pair keys. Off for `MAGIC_V3` segments.
    fmt: bool,
    /// `nb1` bloom/long offset entries (0 without `fmt`), `blooms_len` and
    /// `longs_len` filter words.
    nb1: usize,
    blooms_len: usize,
    longs_len: usize,
    off: [usize; NS],
    dead: Vec<u64>,
    pub live_docs: usize,
    /// Doc ids best-ranked first: built on the first dense query's walk.
    by_rank: std::sync::OnceLock<Vec<u32>>,
    /// Live docs holding indexable text, as a bitset (built with `by_rank`).
    live_text: std::sync::OnceLock<Vec<u64>>,
}

#[derive(Clone, Copy)]
enum S {
    TriKey,
    TriOff,
    Post,
    PathOff,
    Paths,
    Size,
    Mtime,
    ByPath,
    Rank,
    BloomOff,
    Bloom,
    LongOff,
    Long,
}
const NS: usize = 13;

fn lens(ndocs: usize, ntri: usize, plen: usize, paths_len: usize, fmt: bool, blooms: usize, longs: usize) -> [usize; NS] {
    let n1 = (ndocs + 1) * 4 * fmt as usize;
    [ntri * 4, (ntri + 1) * 4, plen, (ndocs + 1) * 4, paths_len, ndocs * 8, ndocs * 4, ndocs * 4, ndocs, n1, blooms * 8, n1, longs * 8]
}

impl Segment {
    sec!(tri_key, S::TriKey, u32, ntri);
    sec!(tri_off, S::TriOff, u32, ntri1);
    sec!(post, S::Post, u8, plen);
    sec!(path_off, S::PathOff, u32, ndocs1);
    sec!(paths, S::Paths, u8, paths_len);
    sec!(size, S::Size, u64, ndocs);
    sec!(mtime, S::Mtime, u32, ndocs);
    sec!(by_path, S::ByPath, u32, ndocs);
    sec!(rank, S::Rank, i8, ndocs);
    sec!(bloom_off, S::BloomOff, u32, nb1);
    sec!(bloom, S::Bloom, u64, blooms_len);
    sec!(long_off, S::LongOff, u32, nb1);
    sec!(long, S::Long, u64, longs_len);

    /// Doc `d`'s short-gram bloom filter (empty for the old format).
    fn bloom_of(&self, d: u32) -> &[u64] {
        let o = self.bloom_off();
        if o.is_empty() {
            return &[];
        }
        &self.bloom()[o[d as usize] as usize..o[d as usize + 1] as usize]
    }

    /// Doc `d`'s long-gram bloom filter (empty for the old format).
    fn long_of(&self, d: u32) -> &[u64] {
        let o = self.long_off();
        if o.is_empty() {
            return &[];
        }
        &self.long()[o[d as usize] as usize..o[d as usize + 1] as usize]
    }

    /// Can doc `d` hold every gram in `probes`, by its bloom filters? Never
    /// a false negative: a "no" only means the doc provably lacks a gram,
    /// and old-format segments (no filters) always say maybe.
    fn may_contain(&self, d: u32, probes: &Probes) -> bool {
        if !self.fmt {
            return true;
        }
        // A doc with no filter (binary, or merged in from an old segment)
        // can never veto: empty words mean "maybe".
        let has = |words: &[u64], h: u32| words.is_empty() || bloom_bits(h, words.len()).iter().all(|&b| words[b / 64] >> (b % 64) & 1 != 0);
        let (short, long) = (self.bloom_of(d), self.long_of(d));
        probes.short.iter().all(|&h| has(short, h)) && probes.long.iter().all(|&h| has(long, h))
    }

    pub fn path(&self, d: u32) -> &[u8] {
        let o = self.path_off();
        &self.paths()[o[d as usize] as usize..o[d as usize + 1] as usize]
    }

    #[inline]
    pub fn is_dead(&self, d: u32) -> bool {
        self.dead[d as usize >> 6] & (1 << (d & 63)) != 0
    }

    fn kill(&mut self, d: u32) -> bool {
        let w = &mut self.dead[d as usize >> 6];
        if *w & (1 << (d & 63)) != 0 {
            return false;
        }
        *w |= 1 << (d & 63);
        // `live_text` caches the dead set: keep it honest after a kill.
        if let Some(bits) = self.live_text.get_mut() {
            bits[d as usize >> 6] &= !(1 << (d & 63));
        }
        self.live_docs -= 1;
        true
    }

    /// Doc ids best-ranked first: sorted once, on the first dense walk.
    /// Ties settle by doc id, the same order `Ranked` sorts to.
    fn by_rank(&self) -> &[u32] {
        self.by_rank.get_or_init(|| {
            let mut v: Vec<u32> = (0..self.ndocs as u32).collect();
            let (rk, mt) = (self.rank(), self.mtime());
            v.sort_unstable_by_key(|&d| (rank_key(rk[d as usize], mt[d as usize]), d));
            v
        })
    }

    /// The segment's live text docs as a bitset (computed with `by_rank`).
    fn live_text(&self) -> &[u64] {
        self.live_text.get_or_init(|| {
            let mut bits = vec![0u64; self.ndocs.div_ceil(64)];
            for (d, w) in self.dead.iter().enumerate() {
                bits[d] = !w;
            }
            for d in 0..self.ndocs as u32 {
                if self.rank()[d as usize] == NOT_TEXT {
                    bits[d as usize >> 6] &= !(1 << (d & 63));
                }
            }
            if self.ndocs % 64 != 0 {
                *bits.last_mut().unwrap() &= (1 << (self.ndocs % 64)) - 1;
            }
            bits
        })
    }

    /// Live docs whose path starts with `prefix`, via the sorted permutation.
    fn with_prefix<'a>(&'a self, prefix: &'a [u8]) -> impl Iterator<Item = u32> + 'a {
        let bp = self.by_path();
        let start = bp.partition_point(|&d| self.path(d) < prefix);
        bp[start..].iter().copied().take_while(move |&d| self.path(d).starts_with(prefix)).filter(move |&d| !self.is_dead(d))
    }

    /// Doc-id range of the paths under `prefix`, and whether it covers the
    /// whole segment (every path starts with it).
    fn doc_range(&self, prefix: &[u8]) -> (std::ops::Range<u32>, bool) {
        let bp = self.by_path();
        let (Some(&first), Some(&last)) = (bp.first(), bp.last()) else { return (0..0, false) };
        let (first, last) = (self.path(first), self.path(last));
        if last < prefix || (first > prefix && !first.starts_with(prefix)) {
            return (0..0, false);
        }
        if first.starts_with(prefix) && last.starts_with(prefix) {
            return (0..self.ndocs as u32, true);
        }
        let a = bp.partition_point(|&d| self.path(d) < prefix);
        let z = a + bp[a..].partition_point(|&d| self.path(d).starts_with(prefix));
        if (z - a) * 2 > bp.len() {
            return (0..self.ndocs as u32, false);
        }
        let (lo, hi) = bp[a..z].iter().fold((u32::MAX, 0), |(lo, hi), &d| (lo.min(d), hi.max(d + 1)));
        (lo.min(hi)..hi, false)
    }

    /// Live docs directly inside `prefix` (which ends in '/'), skipping each
    /// subdirectory's run of paths with one binary search.
    fn direct_children(&self, prefix: &[u8]) -> Vec<u32> {
        let bp = self.by_path();
        let mut out = Vec::new();
        let mut i = bp.partition_point(|&d| self.path(d) < prefix);
        while i < bp.len() {
            let p = self.path(bp[i]);
            if !p.starts_with(prefix) {
                break;
            }
            match p[prefix.len()..].iter().position(|&b| b == b'/') {
                None => {
                    if !self.is_dead(bp[i]) {
                        out.push(bp[i]);
                    }
                    i += 1;
                }
                Some(k) => {
                    // Jump past "prefix/sub/..." : first path >= "prefix/sub0".
                    let mut hi = p[..prefix.len() + k + 1].to_vec();
                    *hi.last_mut().unwrap() = b'/' + 1;
                    i += bp[i..].partition_point(|&d| self.path(d) < hi.as_slice());
                }
            }
        }
        out
    }

    /// The posting list of trigram `tri` as stored (bitset bytes or delta
    /// varints): it is never decoded until something asks for its docs.
    fn list(&self, tri: u32) -> Option<List<'_>> {
        let k = self.tri_key().binary_search(&tri).ok()?;
        let o = self.tri_off();
        let bytes = &self.post()[(o[k] & !BITSET) as usize..(o[k + 1] & !BITSET) as usize];
        Some(if o[k] & BITSET != 0 { List::Bits(bytes) } else { List::Var(Var { deltas: bytes }) })
    }

    fn list_into(&self, k: usize, out: &mut Vec<u32>) {
        let o = self.tri_off();
        let mut bytes = &self.post()[(o[k] & !BITSET) as usize..(o[k + 1] & !BITSET) as usize];
        if o[k] & BITSET != 0 {
            for (w, &b) in bytes.iter().enumerate() {
                let mut b = b;
                while b != 0 {
                    out.push(w as u32 * 8 + b.trailing_zeros());
                    b &= b - 1;
                }
            }
            return;
        }
        let mut last = 0u32;
        while !bytes.is_empty() {
            let (v, n) = varint(bytes);
            bytes = &bytes[n..];
            last += v;
            out.push(last);
        }
    }

    fn load(dir: &Path, id: u64) -> Option<Segment> {
        let f = std::fs::File::open(seg_path(dir, id)).ok()?;
        let map = unsafe { Mmap::map(&f) }.ok()?;
        // Both formats keep the first nine sections identical; the current
        // one appends bloom offsets and words. An old file gets empty
        // sections and `fmt` off.
        let (f4, [ndocs, ntri, plen, paths_len, blooms, longs]) = if let Some(f) = fields::<6>(&map, MAGIC) {
            (true, f.map(|v| v as usize))
        } else {
            let [n, t, p, l] = fields(&map, MAGIC_V3)?;
            (false, [n, t, p, l, 0, 0].map(|v| v as usize))
        };
        let (off, total) = layout(&lens(ndocs, ntri, plen, paths_len, f4, blooms, longs));
        if map.len() < total {
            return None;
        }
        let mut dead = vec![0u64; ndocs.div_ceil(64)];
        if let Ok(b) = std::fs::read(dead_path(dir, id)) {
            for (w, c) in dead.iter_mut().zip(b.chunks_exact(8)) {
                *w = u64::from_le_bytes(c.try_into().unwrap());
            }
        }
        let live_docs = ndocs - dead.iter().map(|w| w.count_ones() as usize).sum::<usize>();
        Some(Segment {
            map,
            id,
            ndocs,
            ndocs1: ndocs + 1,
            ntri,
            ntri1: ntri + 1,
            plen,
            paths_len,
            fmt: f4,
            nb1: (ndocs + 1) * f4 as usize,
            blooms_len: blooms,
            longs_len: longs,
            off,
            dead,
            live_docs,
            by_rank: std::sync::OnceLock::new(),
            live_text: std::sync::OnceLock::new(),
        })
    }

    fn save_dead(&self, dir: &Path) {
        let bytes: Vec<u8> = self.dead.iter().flat_map(|w| w.to_le_bytes()).collect();
        let tmp = dead_path(dir, self.id).with_extension("tmp");
        if std::fs::write(&tmp, bytes).is_ok() {
            let _ = std::fs::rename(tmp, dead_path(dir, self.id));
        }
    }
}

fn seg_path(dir: &Path, id: u64) -> PathBuf {
    dir.join(format!("seg-{id:06}.fsc"))
}
fn dead_path(dir: &Path, id: u64) -> PathBuf {
    dir.join(format!("seg-{id:06}.dead"))
}

#[inline]
fn varint(b: &[u8]) -> (u32, usize) {
    let mut v = 0u32;
    for (i, &x) in b.iter().enumerate().take(5) {
        v |= ((x & 0x7f) as u32) << (7 * i);
        if x & 0x80 == 0 {
            return (v, i + 1);
        }
    }
    (v, b.len().min(5))
}

fn put_varint(out: &mut Vec<u8>, mut v: u32) {
    while v >= 0x80 {
        out.push(v as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// The posting key for "the doc's last two bytes are a,b": the pair with a
/// zero third byte, which no real trigram can produce (a three-byte window
/// never ends in byte 0 of the pair space — the eof marker only collides
/// with a genuine trailing-NUL trigram, and the posting lists are the same
/// set anyway).
fn eof_key(a: u8, b: u8) -> u32 {
    (fold(a) as u32) << 16 | (fold(b) as u32) << 8
}

/// Append the sorted distinct case-folded trigrams of a buffer to `out`,
/// plus the end-of-file pair key for buffers of two bytes or more (so a
/// 2-byte literal can use the index too). `seen` is a 2 MiB scratch bitset,
/// all zero on entry and exit.
fn trigrams(buf: &[u8], seen: &mut [u64], out: &mut Vec<u32>) {
    let start = out.len();
    if buf.len() >= 2 {
        let e = eof_key(buf[buf.len() - 2], buf[buf.len() - 1]);
        seen[(e >> 6) as usize] |= 1 << (e & 63);
        out.push(e);
    }
    if buf.len() >= 3 {
        let mut t = (fold(buf[0]) as u32) << 8 | fold(buf[1]) as u32;
        for &b in &buf[2..] {
            t = ((t << 8) | fold(b) as u32) & 0xFF_FFFF;
            let (w, bit) = ((t >> 6) as usize, 1u64 << (t & 63));
            if seen[w] & bit == 0 {
                seen[w] |= bit;
                out.push(t);
            }
        }
    }
    for &t in &out[start..] {
        seen[(t >> 6) as usize] = 0;
    }
    out[start..].sort_unstable();
}

/// Trigrams of a short string (query side), no scratch needed.
fn trigrams_small(s: &[u8]) -> Vec<u32> {
    let mut t: Vec<u32> = s.windows(3).map(|w| (fold(w[0]) as u32) << 16 | (fold(w[1]) as u32) << 8 | fold(w[2]) as u32).collect();
    t.sort_unstable();
    t.dedup();
    t
}

/// Folded-gram lengths the bloom filters index: a SHORT-byte probe lives in
/// the short filter, a LONG-byte one in the long.
const SHORT: usize = 5;
const LONG: usize = 7;

/// A gram's 24-bit hash (its folded bytes, big-endian in the low 40 bits).
#[inline]
fn gram_hash(g: u64) -> u32 {
    (g.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 40) as u32
}

/// The two bits a gram hash sets in a bloom filter of `words` u64s.
#[inline]
fn bloom_bits(h: u32, words: usize) -> [usize; 2] {
    let at = |h: u32| ((h as u64 * words as u64 * 64) >> 24) as usize;
    [at(h), at(h.wrapping_mul(0x9E37_79B1) & 0xFF_FFFF)]
}

/// Append a bloom filter of these gram hashes to `out`: `bits` bits set per
/// gram, two positions each, in whole words. A gram the doc lacks still
/// passes ~24% of the time at three bits, ~40% at two.
fn bloom(hashes: &[u32], bits: usize, out: &mut Vec<u64>) {
    let words = (hashes.len() * bits).div_ceil(64);
    let at = out.len();
    out.resize(at + words, 0);
    for &h in hashes {
        for b in bloom_bits(h, words) {
            out[at + b / 64] |= 1 << (b % 64);
        }
    }
}

/// The sorted distinct folded n-gram hashes of `buf`, for the bloom filters.
fn gram_hashes(buf: &[u8], n: usize) -> Vec<u32> {
    let mut out: Vec<u32> = buf.windows(n).map(|w| gram_hash(w.iter().fold(0u64, |g, &b| g << 8 | fold(b) as u64))).collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// The gram hashes a doc holding `pattern` must contain, per filter: empty
/// for queries whose matches needn't carry the pattern's folded grams.
#[derive(Default)]
struct Probes {
    short: Vec<u32>,
    long: Vec<u32>,
}

/// Keywords a definition starts with (`sym:` search).
const DEFINES: &str = "fn|func|function|def|class|struct|enum|trait|interface|type|typealias|impl|let|const|var|val|static|module|mod|protocol|extension|macro_rules!|define|typedef|union|object|record|namespace|actor";

/// The posting key for "this doc defines `name`": a hash above the 24-bit
/// trigram space, so definitions live in the same key table as trigrams.
fn symbol_key(name: &[u8]) -> u32 {
    let h = name.iter().fold(0x811c_9dc5u32, |h, &b| (h ^ b as u32).wrapping_mul(0x0100_0193));
    h | 0x8000_0000
}

/// Is `name` something the definition index records (a plain identifier)?
fn plain_identifier(name: &[u8]) -> bool {
    name.first().is_some_and(|&b| b.is_ascii_alphabetic() || b == b'_') && name.iter().all(|&b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Append the sorted distinct definition keys of a buffer to `out`: every
/// identifier right after a declaring keyword, as `sym:` matches it.
fn symbols(buf: &[u8], out: &mut Vec<u32>) {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        RegexBuilder::new(&format!(r"(?-u:\b)(?:{DEFINES})(?:<[^>\n]*>)?[ \t*&]+(?:mut[ \t]+)?[A-Za-z_][A-Za-z0-9_]*"))
            .unicode(false)
            .build()
            .unwrap()
    });
    let mut keys = Vec::new();
    let mut at = 0;
    while let Some(m) = re.find_at(buf, at) {
        let end = m.end();
        let begin = buf[..end].iter().rposition(|&b| !(b.is_ascii_alphanumeric() || b == b'_')).map_or(0, |p| p + 1);
        keys.push(symbol_key(&buf[begin..end]));
        // The name may itself be a keyword ("static func main"): look again
        // from it, not past it.
        at = begin;
    }
    keys.sort_unstable();
    keys.dedup();
    out.extend(keys);
}

/// Paths with size and mtime, all in one buffer: a full sync holds ~500k
/// of them, and one allocation (mmap-backed, returned on drop) beats 500k.
#[derive(Default)]
pub struct Docs {
    buf: Vec<u8>,
    items: Vec<(u32, u32, u64, u32)>,
}

impl Docs {
    fn push(&mut self, path: &[u8], size: u64, mtime: u32) {
        self.items.push((self.buf.len() as u32, path.len() as u32, size, mtime));
        self.buf.extend_from_slice(path);
    }
    fn path(&self, i: usize) -> &[u8] {
        let (o, l, _, _) = self.items[i];
        &self.buf[o as usize..(o + l) as usize]
    }
    pub fn len(&self) -> usize {
        self.items.len()
    }
    fn sort(&mut self) {
        let buf = &self.buf;
        self.items.sort_by(|a, b| buf[a.0 as usize..(a.0 + a.1) as usize].cmp(&buf[b.0 as usize..(b.0 + b.1) as usize]));
        self.items.dedup_by(|a, b| buf[a.0 as usize..(a.0 + a.1) as usize] == buf[b.0 as usize..(b.0 + b.1) as usize]);
    }
    fn find(&self, path: &[u8]) -> Option<usize> {
        let i = self.items.partition_point(|&(o, l, _, _)| &self.buf[o as usize..(o + l) as usize] < path);
        (i < self.items.len() && self.path(i) == path).then_some(i)
    }

    /// Index ranges of about SEG_BYTES of file data each.
    pub fn batches(&self) -> Vec<std::ops::Range<usize>> {
        let (mut out, mut start, mut bytes) = (Vec::new(), 0, 0u64);
        for (i, it) in self.items.iter().enumerate() {
            if bytes >= SEG_BYTES {
                out.push(start..i);
                (start, bytes) = (i, 0);
            }
            bytes += it.2;
        }
        if start < self.items.len() {
            out.push(start..self.items.len());
        }
        out
    }
}

/// Rank of a doc that turned out not to be text: kept so diffs know we
/// looked at it, never a candidate.
const NOT_TEXT: i8 = i8::MIN;

struct DocMeta<'a> {
    path: &'a [u8],
    size: u64,
    mtime: u32,
    rank: i8,
    bloom: &'a [u64],
    long: &'a [u64],
}

/// One rayon split's output for its run of docs: trigram and definition
/// keys bucketed by first byte (so postings build in parallel over key
/// runs), per-doc bloom words, and each doc's slices of them.
struct Split {
    pairs: Vec<Vec<u64>>,
    syms: Vec<u32>,
    sym_off: Vec<(u32, u32)>,
    blooms: Vec<u64>,
    longs: Vec<u64>,
    bloom_off: Vec<(u32, u32)>,
    long_off: Vec<(u32, u32)>,
    docs: Vec<(usize, bool)>, // (doc index in `docs`, is_text)
}

/// The key space in runs that postings are built in, in parallel: the
/// trigrams 64k at a time, then the definition keys (above 2^24).
fn key_runs() -> impl rayon::iter::IndexedParallelIterator<Item = std::ops::Range<u64>> {
    (0..257usize).into_par_iter().map(|r| if r < 256 { (r as u64) << 16..(r as u64 + 1) << 16 } else { 1 << 24..1 << 32 })
}

/// Posting lists for a run of keys, encoded: the keys (ascending), where
/// each list starts in `post` (with its flags), and the bytes.
#[derive(Default)]
struct Postings {
    keys: Vec<u32>,
    offs: Vec<u32>,
    post: Vec<u8>,
}

impl Postings {
    /// Add `key`'s docs (ascending) in a segment of `n`: a bitset when
    /// dense, delta varints otherwise.
    fn push(&mut self, n: usize, key: u32, list: &[u32]) {
        self.keys.push(key);
        if list.len() * 8 > n {
            self.offs.push(self.post.len() as u32 | BITSET);
            let at = self.post.len();
            self.post.resize(at + n.div_ceil(8), 0);
            for &d in list {
                self.post[at + d as usize / 8] |= 1 << (d % 8);
            }
        } else {
            self.offs.push(self.post.len() as u32);
            let mut last = 0u32;
            for &d in list {
                put_varint(&mut self.post, d - last);
                last = d;
            }
        }
    }
}

/// Build one segment file from docs (any order). Files that turn out not
/// to be text are recorded with no trigrams.
pub fn build_segment(dir: &Path, id: u64, docs: &Docs, range: std::ops::Range<usize>) -> Option<Segment> {
    use std::io::Read;
    // Docs in runs of 256, a run on one thread with its next 8 files open
    // and their reads started (`open_ahead`): on a cold cache those reads
    // overlap the work on earlier files. A read buffer and a 2 MiB seen-set
    // go back to a pool after each run, so a build holds one per thread.
    const RUN: usize = 256;
    const AHEAD: usize = 8;
    let scratch: std::sync::Mutex<Vec<(Vec<u64>, Vec<u8>)>> = std::sync::Mutex::new(Vec::new());
    let runs: Vec<std::ops::Range<usize>> = range.clone().step_by(RUN).map(|s| s..(s + RUN).min(range.end)).collect();
    let splits: Vec<Split> = runs
        .into_par_iter()
        .map(|run| {
            let (mut seen, mut buf) = scratch.lock().unwrap().pop().unwrap_or_else(|| (vec![0u64; (1 << 24) / 64], Vec::new()));
            let mut sp = Split {
                pairs: vec![Vec::new(); 256],
                syms: Vec::new(),
                sym_off: Vec::new(),
                blooms: Vec::new(),
                longs: Vec::new(),
                bloom_off: Vec::new(),
                long_off: Vec::new(),
                docs: Vec::new(),
            };
            let mut ahead = std::collections::VecDeque::new();
            for i in run.clone() {
                while ahead.len() <= AHEAD && i + ahead.len() < run.end {
                    ahead.push_back(open_ahead(docs.path(i + ahead.len())));
                }
                buf.clear();
                let ok = (ahead.pop_front().flatten())
                    .and_then(|f| f.take(MAX_FILE + 1).read_to_end(&mut buf).ok())
                    .is_some_and(|n| n as u64 <= MAX_FILE && memchr::memchr(0, &buf[..n.min(8192)]).is_none());
                if ok {
                    let (bs, ls, ss) = (sp.blooms.len() as u32, sp.longs.len() as u32, sp.syms.len() as u32);
                    // Three bits a short gram; two a long one, three in big
                    // files (the costliest to read for nothing).
                    bloom(&gram_hashes(&buf, SHORT), 3, &mut sp.blooms);
                    bloom(&gram_hashes(&buf, LONG), if buf.len() >= 16 << 10 { 3 } else { 2 }, &mut sp.longs);
                    let mut tri = Vec::new();
                    trigrams(&buf, &mut seen, &mut tri);
                    for &t in &tri {
                        sp.pairs[(t >> 16) as usize].push((t as u64) << 32 | (i - range.start) as u64);
                    }
                    symbols(&buf, &mut sp.syms);
                    sp.sym_off.push((ss, sp.syms.len() as u32 - ss));
                    sp.bloom_off.push((bs, sp.blooms.len() as u32 - bs));
                    sp.long_off.push((ls, sp.longs.len() as u32 - ls));
                } else {
                    sp.sym_off.push((0, 0));
                    sp.bloom_off.push((0, 0));
                    sp.long_off.push((0, 0));
                }
                sp.docs.push((i, ok));
            }
            scratch.lock().unwrap().push((seen, buf));
            sp
        })
        .collect();
    // Docs in path order: splits cover contiguous runs, in order.
    let docs_in_order: Vec<(usize, usize)> = splits.iter().enumerate().flat_map(|(si, sp)| (0..sp.docs.len()).map(move |k| (si, k))).collect();
    let meta: Vec<DocMeta> = docs_in_order
        .iter()
        .map(|&(si, k)| {
            let (i, text) = splits[si].docs[k];
            let (_, _, size, mtime) = docs.items[i];
            let (bo, bl) = splits[si].bloom_off[k];
            let (lo, ll) = splits[si].long_off[k];
            DocMeta {
                path: docs.path(i),
                size,
                mtime,
                rank: if text { doc_rank(docs.path(i)) } else { NOT_TEXT },
                bloom: &splits[si].blooms[bo as usize..(bo + bl) as usize],
                long: &splits[si].longs[lo as usize..(lo + ll) as usize],
            }
        })
        .collect();
    // Postings, built in parallel over runs of keys: a run of trigrams
    // takes its bucket of every split, the definition keys every doc's.
    let parts = key_runs()
        .map(|keys| {
            let mut pairs: Vec<u64> = Vec::new();
            if keys.start < 1 << 24 {
                for sp in &splits {
                    pairs.extend_from_slice(&sp.pairs[(keys.start >> 16) as usize]);
                }
            } else {
                for (si, sp) in splits.iter().enumerate() {
                    let d0: u32 = splits[..si].iter().map(|s| s.docs.len() as u32).sum();
                    for (k, &(sst, slen)) in sp.sym_off.iter().enumerate() {
                        pairs.extend(sp.syms[sst as usize..(sst + slen) as usize].iter().map(|&x| (x as u64) << 32 | (d0 + k as u32) as u64));
                    }
                }
            }
            pairs.sort_unstable();
            let (mut part, mut list) = (Postings::default(), Vec::new());
            for run in pairs.chunk_by(|a, b| a >> 32 == b >> 32) {
                list.clear();
                list.extend(run.iter().map(|&p| p as u32));
                part.push(meta.len(), (run[0] >> 32) as u32, &list);
            }
            part
        })
        .collect();
    write_segment(dir, id, &meta, parts)
}

/// Merge segments into one, dropping tombstoned docs. Postings stay in
/// order because docs are renumbered segment by segment; bloom filters
/// carry over as they are.
pub fn merge(dir: &Path, id: u64, segs: &[&Segment]) -> Option<Segment> {
    let mut meta = Vec::new();
    let mut remap: Vec<Vec<u32>> = Vec::with_capacity(segs.len());
    for s in segs {
        let mut r = vec![u32::MAX; s.ndocs];
        for d in 0..s.ndocs as u32 {
            if !s.is_dead(d) {
                r[d as usize] = meta.len() as u32;
                meta.push(DocMeta {
                    path: s.path(d),
                    size: s.size()[d as usize],
                    mtime: s.mtime()[d as usize],
                    rank: s.rank()[d as usize],
                    bloom: s.bloom_of(d),
                    long: s.long_of(d),
                });
            }
        }
        remap.push(r);
    }
    // Postings, merged in parallel over runs of keys.
    let parts = key_runs()
        .map(|keys| {
            let k = |s: &Segment, x: u64| s.tri_key().partition_point(|&t| (t as u64) < x);
            let mut pos: Vec<std::ops::Range<usize>> = segs.iter().map(|s| k(s, keys.start)..k(s, keys.end)).collect();
            let (mut out, mut list, mut part) = (Postings::default(), Vec::new(), Vec::new());
            while let Some(t) = segs.iter().zip(&pos).filter(|(_, p)| !p.is_empty()).map(|(s, p)| s.tri_key()[p.start]).min() {
                list.clear();
                for (si, s) in segs.iter().enumerate() {
                    if !pos[si].is_empty() && s.tri_key()[pos[si].start] == t {
                        part.clear();
                        s.list_into(pos[si].start, &mut part);
                        list.extend(part.iter().map(|&d| remap[si][d as usize]).filter(|&d| d != u32::MAX));
                        pos[si].start += 1;
                    }
                }
                if !list.is_empty() {
                    out.push(meta.len(), t, &list);
                }
            }
            out
        })
        .collect();
    write_segment(dir, id, &meta, parts)
}

/// Write the segment file: docs and their postings, built in runs of keys
/// (ascending). `parts` covers every key in order.
fn write_segment(dir: &Path, id: u64, docs: &[DocMeta<'_>], parts: Vec<Postings>) -> Option<Segment> {
    if docs.is_empty() {
        return None;
    }
    let ndocs = docs.len();
    let (mut keys, mut tri_off, mut post) = (Vec::new(), Vec::new(), Vec::new());
    for p in &parts {
        let base = post.len() as u32;
        keys.extend_from_slice(&p.keys);
        tri_off.extend(p.offs.iter().map(|&o| ((o & !BITSET) + base) | (o & BITSET)));
        post.extend_from_slice(&p.post);
    }
    drop(parts);
    tri_off.push(post.len() as u32);
    let mut paths = Vec::new();
    let mut path_off = vec![0u32];
    for d in docs {
        paths.extend_from_slice(d.path);
        path_off.push(paths.len() as u32);
    }
    let size: Vec<u64> = docs.iter().map(|d| d.size).collect();
    let mtime: Vec<u32> = docs.iter().map(|d| d.mtime).collect();
    let rank: Vec<i8> = docs.iter().map(|d| d.rank).collect();
    let mut by_path: Vec<u32> = (0..ndocs as u32).collect();
    by_path.sort_by(|&a, &b| docs[a as usize].path.cmp(&docs[b as usize].path));
    let (mut bloom_off, mut blooms, mut long_off, mut longs) = (vec![0u32], Vec::new(), vec![0u32], Vec::new());
    for d in docs {
        blooms.extend_from_slice(d.bloom);
        bloom_off.push(blooms.len() as u32);
        longs.extend_from_slice(d.long);
        long_off.push(longs.len() as u32);
    }

    let (off, _) = layout(&lens(ndocs, keys.len(), post.len(), paths.len(), true, blooms.len(), longs.len()));
    let fields = [ndocs, keys.len(), post.len(), paths.len(), blooms.len(), longs.len()].map(|v| v as u64);
    let hdr = header(MAGIC, &fields);
    let sections: [&[u8]; NS] = [
        as_bytes(&keys),
        as_bytes(&tri_off),
        &post,
        as_bytes(&path_off),
        &paths,
        as_bytes(&size),
        as_bytes(&mtime),
        as_bytes(&by_path),
        as_bytes(&rank),
        as_bytes(&bloom_off),
        as_bytes(&blooms),
        as_bytes(&long_off),
        as_bytes(&longs),
    ];
    let p = seg_path(dir, id);
    let tmp = p.with_extension("tmp");
    let write = || -> std::io::Result<()> {
        let mut f = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
        f.write_all(&hdr)?;
        let mut at = HDR;
        for (k, sec) in sections.iter().enumerate() {
            f.write_all(&vec![0u8; off[k] - at])?;
            f.write_all(sec)?;
            at = off[k] + sec.len();
        }
        f.write_all(&vec![0u8; ((at + 63) & !63) - at])?;
        f.flush()
    };
    write().ok()?;
    std::fs::rename(&tmp, &p).ok()?;
    Segment::load(dir, id)
}

/// Should this file be in the content index?
pub fn eligible(path: &[u8], size: u64, home: &[u8]) -> bool {
    let name = &path[path.iter().rposition(|&b| b == b'/').map_or(0, |p| p + 1)..];
    name_ok(name, size) && in_scope(path, home)
}

/// The name/size half of eligibility, checkable before building a path.
fn name_ok(name: &[u8], size: u64) -> bool {
    if size > MAX_FILE {
        return false;
    }
    match name.iter().rposition(|&b| b == b'.').filter(|&p| p > 0) {
        Some(dot) => {
            let ext = &name[dot + 1..];
            TEXT_EXTS.iter().any(|x| x.eq_ignore_ascii_case(ext)) && !name.ends_with(b".min.js") && name != b"package-lock.json"
        }
        None => size <= 256 << 10,
    }
}

/// Is this path (file or directory) inside the indexed area?
pub fn in_scope(path: &[u8], home: &[u8]) -> bool {
    let Some(rest) = path.strip_prefix(home) else { return false };
    if !rest.is_empty() && rest[0] != b'/' {
        return false;
    }
    let rel = rest.strip_prefix(b"/").unwrap_or(rest);
    if SKIP_UNDER_HOME.iter().any(|p| rel.starts_with(p) && rel.get(p.len()).is_none_or(|&b| b == b'/')) {
        return false;
    }
    !rel.split(|&b| b == b'/').any(|c| SKIP_DIRS.contains(&c) || SKIP_SUFFIXES.iter().any(|x| c.len() > x.len() && c.ends_with(x)))
}

/// Candidate order tier: your files first, then dot-dirs, logs and transcripts.
fn doc_rank(path: &[u8]) -> i8 {
    let mut r = 0i8;
    if path.split(|&b| b == b'/').any(|c| c.first() == Some(&b'.')) {
        r -= 2;
    }
    let name = &path[path.iter().rposition(|&b| b == b'/').map_or(0, |p| p + 1)..];
    if [&b".jsonl"[..], b".ndjson", b".log", b".lock", b".sum"].iter().any(|x| name.ends_with(x)) {
        r -= 1;
    }
    r
}

pub struct Content {
    pub dir: PathBuf,
    pub segs: Vec<Segment>,
    next_id: u64,
}

impl Content {
    /// Open the segments the manifest lists, without touching anything: the
    /// view of an engine that follows another process's index.
    pub fn open_shared(dir: PathBuf) -> Content {
        let manifest: Vec<u64> =
            std::fs::read_to_string(dir.join("manifest")).unwrap_or_default().split_whitespace().filter_map(|s| s.parse().ok()).collect();
        let segs: Vec<Segment> = manifest.iter().filter_map(|&id| Segment::load(&dir, id)).collect();
        let next_id = segs.iter().map(|s| s.id + 1).max().unwrap_or(1);
        Content { dir, segs, next_id }
    }

    /// Open as the owner: also delete what the manifest doesn't list.
    pub fn open(dir: PathBuf) -> Content {
        std::fs::create_dir_all(&dir).ok();
        let Content { dir, segs, next_id } = Content::open_shared(dir);
        // Anything not loaded (old format, crashed build) is garbage.
        let keep: Vec<String> = segs.iter().flat_map(|s| [format!("seg-{:06}.fsc", s.id), format!("seg-{:06}.dead", s.id)]).collect();
        for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with("seg-") && !keep.contains(&name) {
                let _ = std::fs::remove_file(e.path());
            }
        }
        Content { dir, segs, next_id }
    }

    fn save_manifest(&self) {
        let s: String = self.segs.iter().map(|s| format!("{}\n", s.id)).collect();
        let tmp = self.dir.join("manifest.tmp");
        if std::fs::write(&tmp, s).is_ok() {
            let _ = std::fs::rename(tmp, self.dir.join("manifest"));
        }
    }

    pub fn docs(&self) -> usize {
        self.segs.iter().map(|s| s.live_docs).sum()
    }

    pub fn bytes(&self) -> usize {
        self.segs.iter().map(|s| s.map.len()).sum()
    }

    /// Diff what the name index wants (from `wanted`, per dir) against the
    /// docs we hold: tombstone what changed or went away, return what needs
    /// (re)indexing. Cheap; the caller builds segments off-lock.
    pub fn diff(&mut self, wants: Vec<(Vec<u8>, bool, Docs)>) -> Docs {
        let mut todo = Docs::default();
        let mut touched = vec![false; self.segs.len()];
        for (dir, recursive, want) in wants {
            let mut held = vec![false; want.len()];
            let lo = join(&dir, b"");
            for (si, s) in self.segs.iter_mut().enumerate() {
                let ids = if recursive { s.with_prefix(&lo).collect::<Vec<_>>() } else { s.direct_children(&lo) };
                for d in ids {
                    match want.find(s.path(d)) {
                        Some(i) if want.items[i].2 == s.size()[d as usize] && want.items[i].3 == s.mtime()[d as usize] => held[i] = true,
                        _ => touched[si] |= s.kill(d),
                    }
                }
            }
            for (i, h) in held.iter().enumerate() {
                if !h {
                    let (_, _, size, mtime) = want.items[i];
                    todo.push(want.path(i), size, mtime);
                }
            }
        }
        for (s, t) in self.segs.iter().zip(&touched) {
            if *t {
                s.save_dead(&self.dir);
            }
        }
        todo.sort();
        todo
    }

    /// Folders holding an indexed file that changed (or went away) since
    /// `since`: an edit in place leaves its folder's mtime alone, so lost
    /// FSEvents history is recovered for the content index by an lstat per
    /// indexed file.
    pub fn changed_dirs(&self, since: u32) -> Vec<Vec<u8>> {
        let pool = crate::live::stat_pool();
        let mut out: Vec<Vec<u8>> = pool.install(|| {
            self.segs
                .par_iter()
                .flat_map_iter(|s| (0..s.ndocs as u32).filter(|&d| !s.is_dead(d)).map(move |d| (s, d)))
                .filter(|&(s, d)| crate::live::lstat(s.path(d)).is_none_or(|o| o.mtime >= since || o.size != s.size()[d as usize]))
                .map(|(s, d)| {
                    let p = s.path(d);
                    p[..p.iter().rposition(|&b| b == b'/').unwrap_or(0).max(1)].to_vec()
                })
                .collect()
        });
        out.sort();
        out.dedup();
        out
    }

    pub fn alloc_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id - 1
    }

    pub fn push(&mut self, seg: Segment) {
        self.segs.push(seg);
        self.save_manifest();
    }

    /// Tiered merging: 8 segments of the same size tier become one, so
    /// incremental updates never pile up thousands of tiny segments. Returns
    /// the group to merge (ids), capped so a merge's postings stay small.
    pub fn merge_plan(&self) -> Option<Vec<u64>> {
        let tier = |s: &Segment| (s.plen.max(1) as f64).log(4.0) as u32;
        let mut by_tier: HashMap<u32, Vec<&Segment>> = HashMap::new();
        for s in &self.segs {
            by_tier.entry(tier(s)).or_default().push(s);
        }
        let mut tiers: Vec<_> = by_tier.into_iter().filter(|(_, v)| v.len() >= 8).collect();
        tiers.sort_by_key(|(t, _)| *t);
        for (_, v) in tiers {
            let group: Vec<u64> = v.iter().take(8).map(|s| s.id).collect();
            let bytes: usize = v.iter().take(8).map(|s| s.plen).sum();
            if bytes <= MERGE_CAP {
                return Some(group);
            }
        }
        None
    }

    pub fn segments(&self, ids: &[u64]) -> Vec<&Segment> {
        self.segs.iter().filter(|s| ids.contains(&s.id)).collect()
    }

    /// Swap merged segments for their replacement. Only the content worker
    /// writes, so nothing was tombstoned while the merge ran.
    pub fn replace(&mut self, ids: &[u64], seg: Segment) {
        self.segs.retain(|s| !ids.contains(&s.id));
        for id in ids {
            let _ = std::fs::remove_file(seg_path(&self.dir, *id));
            let _ = std::fs::remove_file(dead_path(&self.dir, *id));
        }
        self.segs.push(seg);
        self.save_manifest();
    }

    /// Candidate docs for a pattern, filtered by the name query: per
    /// segment the ranked hits (best-first by rank/mtime) plus the rest.
    fn candidates(&self, plan: &TQ, filt: &Query) -> Found {
        // A scope narrows each segment to the doc range holding its paths;
        // when the name query checks nothing beyond it (and the range covers
        // the whole segment), the per-doc path filter is skipped entirely.
        let prefix = filt.scope.as_ref().map(|s| [s.as_slice(), b"/"].concat());
        let only = only_scope(filt);
        let scope: Vec<Option<(std::ops::Range<u32>, bool)>> = self
            .segs
            .iter()
            .map(|s| {
                let (docs, whole) = prefix.as_ref().map_or((0..s.ndocs as u32, true), |p| s.doc_range(p));
                (s.live_docs > 0 && !docs.is_empty()).then_some((docs, !(only && whole)))
            })
            .collect();
        let per: Vec<Cands> = self
            .segs
            .par_iter()
            .enumerate()
            .filter_map(|(si, s)| {
                let (docs, check) = scope[si].clone()?;
                let pass = |d: u32| {
                    !s.is_dead(d)
                        && s.rank()[d as usize] != NOT_TEXT
                        && (!check || filt.match_path(s.path(d), KIND_FILE, s.size()[d as usize], s.mtime()[d as usize]).is_some())
                };
                // An AND of stored bitsets, all docs in scope: AND them
                // straight to a bitset, no id list in between.
                let mut dense_bits = if !check { and_dense(s, plan) } else { None };
                let mut bits = if let Some(mut bits) = dense_bits.take() {
                    for (w, t) in bits.iter_mut().zip(s.live_text().iter()) {
                        *w &= *t;
                    }
                    bits
                } else {
                    let ids = eval(s, plan, docs.clone()).unwrap_or_else(|| docs.collect());
                    // A candidate set covering much of the segment: mark it in
                    // a bitset instead of ranking every candidate.
                    if ids.len() < FIRST || ids.len() * 8 <= s.ndocs as usize {
                        let v: Vec<Ranked> = ids
                            .into_iter()
                            .filter(|&d| pass(d))
                            .map(|d| (rank_key(s.rank()[d as usize], s.mtime()[d as usize]), si as u32, d))
                            .collect();
                        return Some(Cands::list(v));
                    }
                    let mut bits = vec![0u64; s.ndocs as usize / 64 + 1];
                    for &d in &ids {
                        if pass(d) {
                            bits[d as usize >> 6] |= 1 << (d & 63);
                        }
                    }
                    bits
                };
                // Dense enough to walk best-ranked first: take the FIRST
                // best; the rest stay set, so `rest` still sees them.
                let k: usize = bits.iter().map(|w| w.count_ones() as usize).sum();
                if k >= FIRST && k * 8 > s.ndocs as usize {
                    let mut top = Vec::with_capacity(FIRST);
                    for &d in s.by_rank() {
                        if top.len() == FIRST {
                            break;
                        }
                        let w = &mut bits[d as usize >> 6];
                        if *w >> (d & 63) & 1 != 0 {
                            *w &= !(1 << (d & 63));
                            top.push((rank_key(s.rank()[d as usize], s.mtime()[d as usize]), si as u32, d));
                        }
                    }
                    return Some(Cands::Walk { si, bits, top });
                }
                Some(Cands::list(
                    set_bits(&bits).into_iter().map(|d| (rank_key(s.rank()[d as usize], s.mtime()[d as usize]), si as u32, d)).collect(),
                ))
            })
            .collect();
        Found { total: per.iter().map(Cands::len).sum(), per }
    }

    pub fn search(&self, g: &Grep, filt: &Query) -> GrepResult {
        let Found { mut per, total } = self.candidates(&g.plan(), filt);
        // A candidate whose doc's bloom filter lacks one of the pattern's
        // grams can't match: skip it without opening the file.
        let probes = g.probes();
        let path = |&(_, si, d): &Ranked| {
            let s = &self.segs[si as usize];
            s.may_contain(d, &probes).then(|| s.path(d))
        };
        // Most searches are done within the best few hundred candidates:
        // rank those first, and the rest only if reading gets that far.
        let t = std::time::Instant::now();
        let best = take_best(&mut per);
        let (mut r, done) = verify_from(g, best.len(), filt.limit, READERS, t, |i| path(&best[i]));
        if r.files.len() < filt.limit && done == best.len() && best.len() < total {
            // Then the next best few thousand, and the others only if
            // reading gets past those too.
            let mut rest: Vec<Ranked> = par_claim(&per, per.len().min(8), |c| c.rest(&self.segs)).concat();
            let next = rest.len().min(8 * FIRST);
            if next < rest.len() {
                rest.select_nth_unstable(next);
            }
            rest[..next].sort_unstable();
            let mut at = 0;
            for end in [next, rest.len()] {
                if at == end {
                    continue;
                }
                if at > 0 {
                    rest[at..].par_sort_unstable();
                }
                let (more, done) = verify_from(g, end - at, filt.limit - r.files.len(), READERS, t, |i| path(&rest[at + i]));
                r.files.extend(more.files);
                r.read += more.read;
                r.complete = more.complete;
                if r.files.len() >= filt.limit || done < end - at {
                    break;
                }
                at = end;
            }
        }
        r.candidates = total;
        r
    }
}

/// A candidate's place in the ranking, packed so sorting compares ints: your
/// files before dot-dirs/logs, then most recently modified first; ties in
/// segment, then doc order. Then its segment and doc.
type Ranked = (u64, u32, u32);

/// `filt` checks nothing a doc's scope membership can't settle: no tokens,
/// kinds that files pass, no ext/size/mtime/regex filters.
fn only_scope(q: &Query) -> bool {
    q.tokens.is_empty()
        && q.exts.is_empty()
        && q.size == (0, u64::MAX)
        && q.mtime == (0, u32::MAX)
        && q.name_re.is_none()
        && q.path_re.is_none()
        && q.kind.is_none_or(|k| KIND_FILE & 3 == k)
}

/// The first part of `Ranked`.
fn rank_key(rank: i8, mtime: u32) -> u64 {
    (((127 - rank as i32) as u64) << 32) | (u32::MAX - mtime) as u64
}

/// Candidates ranked in the first round.
const FIRST: usize = 512;

/// A group of candidates: all of them, the `FIRST` best in front
/// (unordered).
enum Cands {
    List(Vec<Ranked>),
    /// The `FIRST` best (unordered), and the others.
    Two(Vec<Ranked>, Vec<Ranked>),
    /// A segment's dense candidate set: `top`, its `FIRST` best (in rank
    /// order), and the rest, still set in `bits`.
    Walk { si: usize, bits: Vec<u64>, top: Vec<Ranked> },
}

impl Cands {
    /// A list, its `FIRST` best in front.
    fn list(mut v: Vec<Ranked>) -> Cands {
        if v.len() > FIRST {
            v.select_nth_unstable(FIRST);
            let rest = v.split_off(FIRST);
            return Cands::Two(v, rest);
        }
        Cands::List(v)
    }

    fn len(&self) -> usize {
        match self {
            Cands::List(v) => v.len(),
            Cands::Two(b, r) => b.len() + r.len(),
            Cands::Walk { bits, top, .. } => top.len() + bits.iter().map(|w| w.count_ones() as usize).sum::<usize>(),
        }
    }

    /// What `take_best` left, unordered.
    fn rest(&self, segs: &[Segment]) -> Vec<Ranked> {
        match self {
            Cands::List(v) => v.clone(),
            Cands::Two(_, rest) => rest.clone(),
            Cands::Walk { si, bits, .. } => set_bits(bits)
                .into_iter()
                .map(|d| {
                    let s = &segs[*si];
                    (rank_key(s.rank()[d as usize], s.mtime()[d as usize]), *si as u32, d)
                })
                .collect(),
        }
    }
}

/// The docs set in `bits`, ascending.
fn set_bits(bits: &[u64]) -> Vec<u32> {
    let mut out = Vec::new();
    for (w, &x) in bits.iter().enumerate() {
        let mut x = x;
        while x != 0 {
            out.push((w * 64 + x.trailing_zeros() as usize) as u32);
            x &= x - 1;
        }
    }
    out
}

/// A search's candidates: per segment, and how many in all.
struct Found {
    per: Vec<Cands>,
    total: usize,
}

/// Take the `FIRST` best-ranked candidates out of the segments', in order.
fn take_best(per: &mut Vec<Cands>) -> Vec<Ranked> {
    let mut top = Vec::new();
    for c in per.iter_mut() {
        match c {
            Cands::List(v) => top.extend(v.drain(..v.len().min(FIRST))),
            Cands::Two(best, _) => top.append(best),
            Cands::Walk { top: t, .. } => top.append(t),
        }
    }
    if top.len() > FIRST {
        top.select_nth_unstable(FIRST);
        per.push(Cands::List(top.split_off(FIRST)));
    }
    top.sort_unstable();
    top
}

/// `items.iter().map(f).collect()` on `lanes` lanes (see `run_lanes`): each
/// claims the next item.
fn par_claim<T: Sync, R: Send>(items: &[T], lanes: usize, f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
    let next = AtomicUsize::new(0);
    let out = std::sync::Mutex::new(Vec::with_capacity(items.len()));
    run_lanes(lanes, &|| {
        let mut mine = Vec::new();
        loop {
            let i = next.fetch_add(1, Relaxed);
            let Some(x) = items.get(i) else { break };
            mine.push((i, f(x)));
        }
        out.lock().unwrap().extend(mine);
    });
    let mut out = out.into_inner().unwrap();
    out.sort_unstable_by_key(|r| r.0);
    out.into_iter().map(|r| r.1).collect()
}

/// One search's lanes, shared with the helper threads.
struct Lanes {
    n: usize,
    next: std::sync::atomic::AtomicUsize,
    done: std::sync::atomic::AtomicUsize,
    panicked: std::sync::atomic::AtomicBool,
    /// The search's job, its lifetime erased: it is only run for a lane
    /// claimed below `n`, and `run_lanes` returns after all of those end.
    job: *const (dyn Fn() + Sync),
}

// Safety: `job` is Sync and outlives every call (see the field).
unsafe impl Send for Lanes {}
unsafe impl Sync for Lanes {}

impl Lanes {
    fn work(&self) {
        use std::sync::atomic::Ordering::*;
        while self.next.fetch_add(1, Relaxed) < self.n {
            let job = std::panic::AssertUnwindSafe(|| unsafe { (*self.job)() });
            if std::panic::catch_unwind(job).is_err() {
                self.panicked.store(true, Relaxed);
            }
            self.done.fetch_add(1, Release);
        }
    }
}

/// Threads that help searches: the latest search's lanes, and a bell.
struct Helpers {
    latest: std::sync::Mutex<(u64, Option<std::sync::Arc<Lanes>>)>,
    bell: std::sync::Condvar,
}

fn helpers() -> &'static Helpers {
    static H: std::sync::OnceLock<&'static Helpers> = std::sync::OnceLock::new();
    H.get_or_init(|| {
        let h: &'static Helpers = Box::leak(Box::new(Helpers { latest: Default::default(), bell: Default::default() }));
        let n = std::thread::available_parallelism().map_or(1, |n| n.get()).max(SCAN_READERS);
        for i in 1..n {
            let spawned = std::thread::Builder::new().name(format!("tov-search-help-{i}")).spawn(move || {
                // Someone is waiting: keep off the slow cores and out of the
                // throttled IO tiers, and never download iCloud placeholders.
                #[cfg(target_os = "macos")]
                unsafe {
                    libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INITIATED, 0)
                };
                crate::no_materialize();
                let mut seen = 0;
                loop {
                    let lanes = {
                        let mut g = h.latest.lock().unwrap();
                        while g.0 == seen {
                            g = h.bell.wait(g).unwrap();
                        }
                        seen = g.0;
                        g.1.clone()
                    };
                    if let Some(l) = lanes {
                        l.work();
                    }
                }
            });
            if spawned.is_err() {
                break;
            }
        }
        h
    })
}

/// Run `job` on `n` lanes at once, this thread taking lanes too, and return
/// when all are done. A lane goes to whichever thread claims it first, so
/// this never waits on a helper still waking up (~0.1 ms when they sleep):
/// one that wakes late finds no lane left.
fn run_lanes(n: usize, job: &(dyn Fn() + Sync)) {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::*};
    if n <= 1 {
        return job();
    }
    let h = helpers();
    let job: *const (dyn Fn() + Sync + '_) = job;
    // Safety: see `Lanes::job`; the wait below is what makes it hold.
    let job: *const (dyn Fn() + Sync + 'static) = unsafe { std::mem::transmute(job) };
    let lanes = std::sync::Arc::new(Lanes { n, next: AtomicUsize::new(0), done: AtomicUsize::new(0), panicked: AtomicBool::new(false), job });
    {
        let mut g = h.latest.lock().unwrap();
        g.0 += 1;
        g.1 = Some(lanes.clone());
    }
    for _ in 1..n {
        h.bell.notify_one();
    }
    lanes.work();
    while lanes.done.load(Acquire) < n {
        std::thread::yield_now();
    }
    assert!(!lanes.panicked.load(Relaxed), "a search lane panicked");
}

/// What the name index says should be indexed under `dir` (direct
/// children only unless `recursive`).
pub fn wanted(live: &Live, home: &[u8], dir: &[u8], recursive: bool) -> Docs {
    let mut want = Docs::default();
    if !in_scope(dir, home) {
        return want;
    }
    let idx = &live.base;
    let mut p = Vec::new();
    if let Some(d) = idx.lookup(dir).filter(|&e| !live.is_dead(e)).and_then(|e| idx.dir_of(e)) {
        let range = if recursive { idx.dir_start()[d as usize] as usize..idx.dir_end()[d as usize] as usize } else { idx.children(d) };
        for i in range {
            if idx.kind()[i] & 3 != KIND_FILE || live.is_dead(i as u32) || !name_ok(idx.name(i), idx.size_of(i)) {
                continue;
            }
            if recursive {
                idx.path(i, &mut p);
            } else {
                p = join(dir, idx.name(i));
            }
            if in_scope(&p, home) {
                want.push(&p, idx.size_of(i), idx.mtime()[i]);
            }
        }
    }
    let lo = join(dir, b"");
    for (k, o) in live.over.range(lo.clone()..).take_while(|(k, _)| k.starts_with(&lo)) {
        if o.kind & 3 == KIND_FILE && (recursive || !k[lo.len()..].contains(&b'/')) && eligible(k, o.size, home) {
            want.push(k, o.size, o.mtime);
        }
    }
    want
}

/// The dirs/trees from one batch of changes, each with what should be
/// indexed there. Done under the name-index read lock only.
pub fn wants(live: &Live, home: &[u8], dirs: &[Vec<u8>], trees: &[Vec<u8>]) -> Vec<(Vec<u8>, bool, Docs)> {
    let mut out: Vec<(Vec<u8>, bool)> = Vec::new();
    for (d, r) in dirs.iter().map(|d| (d, false)).chain(trees.iter().map(|d| (d, true))) {
        if in_scope(d, home) {
            out.push((d.clone(), r));
        } else if r && home.starts_with(d) {
            // A subtree containing home (e.g. "/" rescanned): sync all of home.
            out.push((home.to_vec(), true));
        }
    }
    out.sort();
    out.dedup();
    out.into_iter()
        .map(|(d, r)| {
            let mut w = wanted(live, home, &d, r);
            w.sort();
            (d, r, w)
        })
        .collect()
}

pub struct Grep {
    pub pattern: String,
    pub mode: GrepMode,
    pub max_per_file: usize,
    /// Stop reading candidates after this long (None = read them all).
    pub budget: Option<std::time::Duration>,
    /// The matcher folds case (smart-case literal, or a `re:` flag).
    case_insensitive: bool,
    re: std::sync::OnceLock<Regex>,
    src: String,
    /// An ASCII pattern short enough to match without a regex (a `sym:`
    /// name or a literal): then the regex is never even compiled, and most
    /// files never see it — the literal mostly isn't: `literal` finds it).
    literal: Option<Literal>,
}

impl Grep {
    pub fn new(pattern: &str, mode: GrepMode) -> Result<Grep, String> {
        let smart_ci = !pattern.chars().any(|c| c.is_uppercase());
        let src = match mode {
            GrepMode::Literal => regex::escape(pattern),
            GrepMode::Regex => pattern.to_string(),
            // A definition: a declaring keyword, optional generics/modifiers,
            // then the name. ASCII word boundaries keep the regex on the fast
            // DFA path even in files with non-ASCII text.
            GrepMode::Symbol => format!(r"(?-u:\b)(?:{DEFINES})(?:<[^>\n]*>)?[ \t*&]+(?:mut[ \t]+)?{}(?-u:\b)", regex::escape(pattern)),
        };
        let case_insensitive = smart_ci && mode != GrepMode::Symbol;
        let short = (1..=4096).contains(&pattern.len());
        let literal = match mode {
            GrepMode::Symbol if short => Some(Literal::Definition(memchr::memmem::Finder::new(pattern.as_bytes()).into_owned())),
            GrepMode::Literal if short && pattern.is_ascii() => Some(match case_insensitive {
                true => Literal::folded(pattern.as_bytes()),
                false => Literal::Exact(memchr::memmem::Finder::new(pattern.as_bytes()).into_owned()),
            }),
            _ => None,
        };
        let g = Grep {
            pattern: pattern.to_string(),
            mode,
            max_per_file: 5,
            budget: Some(std::time::Duration::from_millis(250)),
            case_insensitive,
            re: std::sync::OnceLock::new(),
            src,
            literal,
        };
        if g.literal.is_none() {
            g.re.set(g.build()?).ok();
        }
        Ok(g)
    }

    fn build(&self) -> Result<Regex, String> {
        RegexBuilder::new(&self.src).case_insensitive(self.case_insensitive).multi_line(true).size_limit(1 << 26).build().map_err(|e| e.to_string())
    }

    fn re(&self) -> &Regex {
        // A literal's regex always compiles (it was 4096 bytes at most).
        self.re.get_or_init(|| self.build().expect("literal regex"))
    }

    /// Gram hashes every matching doc must hold (see `SHORT`), or none when
    /// the match needn't carry the pattern's folded bytes: regexes match
    /// arbitrary things, and a case-insensitive pattern holding 'k', 's' or
    /// non-ASCII can match on Unicode folds (K, ſ) whose folded grams are
    /// not the pattern's — probing those would drop real hits.
    fn probes(&self) -> Probes {
        let grams = |n: usize| {
            let mut out: Vec<u32> = if self.mode == GrepMode::Regex
                || (self.case_insensitive && self.pattern.bytes().any(|b| !b.is_ascii() || matches!(b, b'k' | b'K' | b's' | b'S')))
            {
                Vec::new()
            } else {
                self.pattern.as_bytes().windows(n).map(|w| gram_hash(w.iter().fold(0, |g, &b| g << 8 | fold(b) as u64))).collect()
            };
            out.sort_unstable();
            out.dedup();
            out
        };
        Probes { short: grams(SHORT), long: grams(LONG) }
    }

    fn plan(&self) -> TQ {
        match self.mode {
            // Exactly the docs that define it (plus rare hash collisions,
            // which reading the file weeds out).
            GrepMode::Symbol if plain_identifier(self.pattern.as_bytes()) => TQ::Tri(symbol_key(self.pattern.as_bytes())),
            GrepMode::Literal | GrepMode::Symbol => literal_plan(self.pattern.as_bytes()),
            GrepMode::Regex => regex_syntax::Parser::new().parse(&self.pattern).map_or(TQ::All, |h| regex_plan(&h)),
        }
    }
}

#[derive(Default)]
pub struct GrepResult {
    pub files: Vec<FileMatches>,
    pub candidates: usize,
    pub read: usize,
    /// False if the time budget ran out before every candidate was read.
    pub complete: bool,
}

/// Threads reading candidates (the searching one and helpers): file opens
/// on this Mac stop scaling past ~4 (Endpoint Security clients tax every
/// open; measured on hot files: 7.5 us/file at 4 threads, 9 at 8, 14 at 12).
const READERS: usize = 4;
/// Folders outside the index (`in:/etc`) are read with more threads: their
/// files are mostly small and not in the page cache, so reads wait on the
/// disk more than on the open() tax.
const SCAN_READERS: usize = 8;

/// Run `f` with iCloud placeholders turned off (they fail fast instead of
/// materializing), then restore the caller's I/O policy.
fn without_materializing<T>(f: impl FnOnce() -> T) -> T {
    #[cfg(target_os = "macos")]
    unsafe extern "C" {
        fn getiopolicy_np(iotype: i32, scope: i32) -> i32;
        fn setiopolicy_np(iotype: i32, scope: i32, policy: i32) -> i32;
    }
    // IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES, IOPOL_SCOPE_THREAD
    #[cfg(target_os = "macos")]
    let prior = unsafe { getiopolicy_np(3, 1) };
    crate::no_materialize();
    let r = f();
    #[cfg(target_os = "macos")]
    if prior >= 0 {
        unsafe { setiopolicy_np(3, 1, prior) };
    }
    r
}

pub struct FileMatches {
    pub path: Vec<u8>,
    pub lines: Vec<(usize, String)>,
}

/// Read candidates in order until `limit` files have matched or the time
/// budget is spent (best-ranked results first, so a cut-short search still
/// returns the ones you most likely wanted). Each read thread claims the
/// next unread candidate, so the files read are always a prefix of the
/// order and reading stops as soon as the `limit`th match is in.
pub fn verify(g: &Grep, paths: &[impl AsRef<[u8]> + Sync], limit: usize) -> GrepResult {
    verify_from(g, paths.len(), limit, SCAN_READERS, std::time::Instant::now(), |i| Some(paths[i].as_ref())).0
}

/// `verify` over `n` candidates with `readers` threads, `path(i)` giving
/// the i-th, or None if the index already rules it out (the bloom filters
/// did); the budget counts from `t`. Also returns how many candidates it
/// got through.
fn verify_from<'a>(
    g: &Grep,
    n: usize,
    limit: usize,
    readers: usize,
    t: std::time::Instant,
    path: impl Fn(usize) -> Option<&'a [u8]> + Sync,
) -> (GrepResult, usize) {
    use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
    let (next, found, read) = (AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0));
    let hits = std::sync::Mutex::new(Vec::new());
    let work = || {
        while found.load(Relaxed) < limit && g.budget.is_none_or(|b| t.elapsed() <= b) {
            let i = next.fetch_add(1, Relaxed);
            if i >= n {
                break;
            }
            let Some(p) = path(i) else { continue };
            read.fetch_add(1, Relaxed);
            if let Some(m) = match_file(g, p) {
                found.fetch_add(1, Relaxed);
                hits.lock().unwrap().push((i, m));
            }
        }
        // Don't sit on a big file's worth of buffer between searches.
        READ_BUF.with_borrow_mut(|b| {
            if b.capacity() > 256 << 10 {
                *b = Vec::new();
            }
        });
    };
    without_materializing(|| run_lanes(readers, &work));
    let mut hits = hits.into_inner().unwrap();
    hits.sort_unstable_by_key(|h| h.0);
    hits.truncate(limit);
    let done = next.into_inner().min(n);
    let complete = done == n || hits.len() >= limit;
    (GrepResult { files: hits.into_iter().map(|h| h.1).collect(), candidates: n, read: read.into_inner(), complete }, done)
}

thread_local! {
    /// One read buffer per read-pool thread: a fresh ~1 MB Vec per file
    /// costs page faults and an munmap every time.
    static READ_BUF: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn match_file(g: &Grep, path: &[u8]) -> Option<FileMatches> {
    READ_BUF.with_borrow_mut(|buf| {
        use std::io::Read;
        buf.clear();
        open_regular(path)?.take(MAX_FILE * 4).read_to_end(buf).ok()?;
        if memchr::memchr(0, &buf[..buf.len().min(8192)]).is_some() {
            return None;
        }
        let lines = match g.literal.as_ref().filter(|l| !l.needs_regex(buf)) {
            Some(l) => lines_at(g, buf, l.starts(buf)),
            None => lines_at(g, buf, g.re().find_iter(buf).map(|m| m.start())),
        };
        (!lines.is_empty()).then(|| FileMatches { path: path.to_vec(), lines })
    })
}

/// The first `max_per_file` lines holding a match (by where matches start),
/// numbered, as shown.
fn lines_at(g: &Grep, buf: &[u8], starts: impl Iterator<Item = usize>) -> Vec<(usize, String)> {
    let mut lines = Vec::new();
    let (mut line_no, mut counted) = (1usize, 0usize);
    let mut last_line_start = usize::MAX;
    for start in starts {
        line_no += memchr::memchr_iter(b'\n', &buf[counted..start]).count();
        counted = start;
        let ls = memchr::memrchr(b'\n', &buf[..start]).map_or(0, |p| p + 1);
        if ls == last_line_start {
            continue;
        }
        last_line_start = ls;
        let le = memchr::memchr(b'\n', &buf[start..]).map_or(buf.len(), |p| start + p);
        lines.push((line_no, String::from_utf8_lossy(&buf[ls..le.min(ls + 400)]).trim_end().to_string()));
        if lines.len() >= g.max_per_file {
            break;
        }
    }
    lines
}

/// An ASCII literal pattern, found without a regex: its exact bytes (it has
/// an upper-case letter), or else its letters in either case. That is what
/// the case-insensitive regex matches too, but for two non-ASCII letters it
/// folds onto ASCII ones (ſ for s, the Kelvin sign for k): a file holding
/// one of those goes to the regex.
enum Literal {
    Exact(memchr::memmem::Finder<'static>),
    /// A `sym:` name: as a whole word after a declaring keyword on its line
    /// (the definition regex, with its keyword part compiled once).
    Definition(memchr::memmem::Finder<'static>),
    Folded {
        needle: Vec<u8>,
        pair: (usize, usize),
        odd: Vec<memchr::memmem::Finder<'static>>,
    },
}

impl Literal {
    fn folded(pattern: &[u8]) -> Literal {
        let needle: Vec<u8> = pattern.iter().map(|&b| fold(b)).collect();
        // Scan for the two rarest bytes first.
        let common = |b: u8| match b {
            b' ' | b'e' | b't' | b'a' | b'o' | b'i' | b'n' | b's' | b'r' => 3,
            b'h' | b'l' | b'd' | b'c' | b'u' | b'm' | b'\n' | b'\t' => 2,
            b'f' | b'p' | b'g' | b'w' | b'y' | b'b' | b'.' | b',' | b'_' | b'(' | b')' | b'"' | b'=' | b'0'..=b'9' => 1,
            _ => 0,
        };
        let mut by_rarity: Vec<usize> = (0..needle.len()).collect();
        by_rarity.sort_by_key(|&i| (common(needle[i]), i));
        let (a, b) = (by_rarity[0], *by_rarity.get(1).unwrap_or(&by_rarity[0]));
        let odd = [(b's', "\u{17F}"), (b'k', "\u{212A}")].iter().filter(|(c, _)| needle.contains(c));
        let odd = odd.map(|(_, u)| memchr::memmem::Finder::new(u.as_bytes()).into_owned()).collect();
        Literal::Folded { needle, pair: (a.min(b), a.max(b)), odd }
    }

    fn needs_regex(&self, hay: &[u8]) -> bool {
        matches!(self, Literal::Folded { odd, .. } if odd.iter().any(|f| f.find(hay).is_some()))
    }

    /// Where its matches start, left to right.
    fn starts<'a>(&'a self, hay: &'a [u8]) -> impl Iterator<Item = usize> + 'a {
        let len = match self {
            Literal::Exact(f) | Literal::Definition(f) => f.needle().len(),
            Literal::Folded { needle, .. } => needle.len(),
        };
        let mut at = 0;
        std::iter::from_fn(move || {
            loop {
                let p = match self {
                    Literal::Exact(f) | Literal::Definition(f) => f.find(hay.get(at..)?).map(|p| at + p),
                    Literal::Folded { needle, pair, .. } => find_folded(hay, at, needle, *pair),
                }?;
                if !matches!(self, Literal::Definition(_)) {
                    at = p + len;
                    return Some(p);
                }
                at = p + 1;
                if defined_at(hay, p, len) {
                    return Some(p);
                }
            }
        })
    }
}

/// Is the name at `hay[p..p + len]` defined there: an ASCII word boundary
/// after it, and before it on its line a declaring keyword, maybe generics,
/// spaces or `*&`, maybe `mut` (what `sym:`'s regex asks, split at the name)?
fn defined_at(hay: &[u8], p: usize, len: usize) -> bool {
    static KEYWORD: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let word = |b: Option<&u8>| b.is_some_and(|&b| b.is_ascii_alphanumeric() || b == b'_');
    if word(hay[..p + len].last()) == word(hay.get(p + len)) {
        return false;
    }
    let re = KEYWORD.get_or_init(|| Regex::new(&format!(r"(?-u:\b)(?:{DEFINES})(?:<[^>\n]*>)?[ \t*&]+(?:mut[ \t]+)?\z")).unwrap());
    let line = memchr::memrchr(b'\n', &hay[..p]).map_or(0, |i| i + 1);
    re.is_match(&hay[line..p])
}

/// The first place at or after `from` where `hay` holds `needle` (lower
/// case) in either case; `pair` are two positions of rare bytes in it, the
/// ones checked first.
fn find_folded(hay: &[u8], from: usize, needle: &[u8], pair: (usize, usize)) -> Option<usize> {
    let k = needle.len();
    let last = hay.len().checked_sub(k)?;
    let at = |c: usize| hay[c..c + k].iter().zip(needle).all(|(&x, &y)| fold(x) == y);
    let mut i = from;
    #[cfg(target_arch = "aarch64")]
    // Safety: every load reads 16 bytes at i + pair.1 at most, and the loop
    // keeps i + 16 <= last + 1, so they end by hay.len().
    unsafe {
        use std::arch::aarch64::*;
        let (c1, c2) = (needle[pair.0], needle[pair.1]);
        let case = |c: u8| vdupq_n_u8(if c.is_ascii_lowercase() { 0x20 } else { 0 });
        let (v1, v2, m1, m2) = (vdupq_n_u8(c1), vdupq_n_u8(c2), case(c1), case(c2));
        while i + 16 <= last + 1 {
            let p = hay.as_ptr().add(i);
            let eq = vandq_u8(vceqq_u8(vorrq_u8(vld1q_u8(p.add(pair.0)), m1), v1), vceqq_u8(vorrq_u8(vld1q_u8(p.add(pair.1)), m2), v2));
            // Four bits per byte lane.
            let mut bits = vget_lane_u64::<0>(vreinterpret_u64_u8(vshrn_n_u16::<4>(vreinterpretq_u16_u8(eq))));
            while bits != 0 {
                let c = i + (bits.trailing_zeros() / 4) as usize;
                if at(c) {
                    return Some(c);
                }
                bits &= !(0xF << ((c - i) * 4));
            }
            i += 16;
        }
    }
    (i..=last).find(|&c| at(c))
}

/// A trigram query: which docs could possibly match.
#[derive(Debug, Clone)]
pub enum TQ {
    All,
    Tri(u32),
    /// Every key p..=p|0xFF: the trigrams starting with a byte pair, plus
    /// the pair itself (low byte 0, the end-of-file marker).
    Pair(u32),
    And(Vec<TQ>),
    Or(Vec<TQ>),
}

fn literal_plan(s: &[u8]) -> TQ {
    match s.len() {
        0 | 1 => TQ::All,
        // First two bytes: every doc holding them has a trigram starting
        // with them, or — a two-byte file — just the end-of-file pair key.
        2 => TQ::Pair(eof_key(s[0], s[1])),
        _ => TQ::And(trigrams_small(s).into_iter().map(TQ::Tri).collect()),
    }
}

/// Docs matching `q` in a segment; None means "every doc".
/// A stored posting list, decoded only when asked: either the bitset bytes
/// (one bit per doc, set = present) or the delta-varint bytes.
enum List<'a> {
    Bits(&'a [u8]),
    Var(Var<'a>),
}

/// A delta-varint list: its docs come from summing; seeking to n means
/// decoding and discarding n entries.
struct Var<'a> {
    deltas: &'a [u8],
}

/// Doc ids of a varint list, ascending.
struct Varints<'a> {
    b: std::slice::Iter<'a, u8>,
    acc: u32,
}

impl Iterator for Varints<'_> {
    type Item = u32;
    fn next(&mut self) -> Option<u32> {
        let mut v = 0u32;
        let mut s = 0;
        loop {
            let &x = self.b.next()?;
            v |= ((x & 0x7f) as u32) << s;
            if x < 0x80 {
                break;
            }
            s += 7;
        }
        self.acc += v;
        Some(self.acc)
    }
}

impl<'a> Var<'a> {
    /// The list's docs, ascending.
    fn iter(&self) -> Varints<'a> {
        Varints { b: self.deltas.iter(), acc: 0 }
    }
}

/// A read position on a varint list, for `seek` membership tests.
struct Cursor<'a>(std::iter::Peekable<Varints<'a>>);

impl Cursor<'_> {
    /// Whether `d` is on the list, advancing past it.
    fn seek(&mut self, d: u32) -> bool {
        while self.0.peek().is_some_and(|&x| x < d) {
            self.0.next();
        }
        self.0.peek() == Some(&d)
    }
}

impl<'a> List<'a> {
    /// Decode it, keeping only docs inside `docs`, into `out`.
    fn decode(&self, docs: Range<u32>, out: &mut Vec<u32>) {
        match self {
            List::Bits(b) => {
                for (w, &byte) in b[docs.start as usize / 8..].iter().enumerate() {
                    let base = w as u32 * 8 + docs.start & !7;
                    if base >= docs.end {
                        break;
                    }
                    let mut byte = byte;
                    while byte != 0 {
                        let i = byte.trailing_zeros();
                        byte &= byte - 1;
                        let d = base + i;
                        if d < docs.end && d >= docs.start {
                            out.push(d);
                        }
                    }
                }
            }
            List::Var(v) => out.extend(v.iter().filter(|d| docs.contains(d))),
        }
    }

    /// Keep only docs on this list: when `v` is much shorter than the list,
    /// seek into it rather than decoding it whole.
    fn retain(&self, v: &mut Vec<u32>) {
        match self {
            List::Bits(b) => v.retain(|&d| b.get(d as usize / 8).is_some_and(|&x| x >> (d % 8) & 1 != 0)),
            List::Var(d) => {
                if v.len() * 64 < d.deltas.len() {
                    let mut c = Cursor(d.iter().peekable());
                    v.retain(|&d| c.seek(d));
                } else {
                    let all: Vec<u32> = d.iter().collect();
                    *v = intersect(v, &all);
                }
            }
        }
    }
}

/// Docs set on every one of the bitsets, within `docs`, into `out`.
/// `docs` need not start on a word boundary (a scoped range is where its
/// paths sort), so this works in bytes and bounds each id by the range.
fn and_bits(lists: &[&[u8]], docs: Range<u32>, out: &mut Vec<u32>) {
    let end = (docs.end as usize).div_ceil(8).min(lists.iter().map(|l| l.len()).min().unwrap_or(0));
    // A list's 64 docs from byte `i` (none past its end).
    let word = |l: &[u8], i: usize| match l.get(i..i + 8) {
        Some(b) => u64::from_le_bytes(b.try_into().unwrap()),
        None => l[i..].iter().rev().fold(0, |w, &b| w << 8 | b as u64),
    };
    let mut i = docs.start as usize / 8;
    while i < end {
        let mut w = u64::MAX;
        for l in lists {
            w &= word(l, i);
            if w == 0 {
                break;
            }
        }
        while w != 0 {
            let d = i as u32 * 8 + w.trailing_zeros();
            if docs.contains(&d) {
                out.push(d);
            }
            w &= w - 1;
        }
        i += 8;
    }
}

/// An AND whose members are all trigrams stored as bitsets: their AND, as
/// a bitset — no id list in between.
fn and_dense(s: &Segment, q: &TQ) -> Option<Vec<u64>> {
    let TQ::And(qs) = q else { return None };
    let lists: Vec<&[u8]> = qs
        .iter()
        .map(|q| match q {
            TQ::Tri(t) => s.list(*t).and_then(|l| if let List::Bits(b) = l { Some(b) } else { None }),
            _ => None,
        })
        .collect::<Option<_>>()?;
    if lists.is_empty() {
        return None;
    }
    let mut bits = vec![!0u64; s.ndocs.div_ceil(64)];
    if s.ndocs % 64 != 0 {
        *bits.last_mut().unwrap() &= (1 << (s.ndocs % 64)) - 1;
    }
    for l in lists {
        for (w, b) in bits.iter_mut().zip(l.chunks(8)) {
            *w &= match <[u8; 8]>::try_from(b) {
                Ok(b) => u64::from_le_bytes(b),
                Err(_) => b.iter().rev().fold(0, |x, &y| x << 8 | y as u64),
            };
        }
    }
    Some(bits)
}

/// Docs within `docs` matching `q` in a segment, ascending; None means
/// every doc. An AND decodes only its shortest varint list to the end;
/// longer lists and bitsets are tests per candidate, not decodes per doc.
fn eval(s: &Segment, q: &TQ, docs: Range<u32>) -> Option<Vec<u32>> {
    match q {
        TQ::All => None,
        TQ::Tri(t) => {
            let mut out = Vec::new();
            if let Some(l) = s.list(*t) {
                l.decode(docs, &mut out);
            }
            Some(out)
        }
        TQ::Pair(p) => {
            if !s.fmt {
                // Old segments index no end-of-file pairs: a two-byte file
                // would be missed, so fall back to every doc.
                return None;
            }
            // Every key p..=p|0xFF: pair-completing trigrams plus the pair
            // itself (low byte 0).
            let keys = s.tri_key();
            let a = keys.partition_point(|&k| k < *p);
            let b = keys.partition_point(|&k| k <= p | 0xFF);
            let mut bits = vec![0u64; s.ndocs.div_ceil(64)];
            for &k in &keys[a..b] {
                match s.list(k) {
                    Some(List::Bits(x)) => {
                        for (w, c) in bits.iter_mut().zip(x.chunks(8)) {
                            let mut x8 = [0u8; 8];
                            x8[..c.len()].copy_from_slice(c);
                            *w |= u64::from_le_bytes(x8);
                        }
                    }
                    Some(List::Var(v)) => v.iter().for_each(|d| bits[d as usize / 64] |= 1 << (d % 64)),
                    None => {}
                }
            }
            let mut out = Vec::new();
            for (w, &word) in bits.iter().enumerate() {
                let d0 = w as u32 * 64;
                if d0 >= docs.end {
                    break;
                }
                let mut word = word;
                while word != 0 {
                    let i = word.trailing_zeros();
                    word &= word - 1;
                    let d = d0 + i;
                    if d >= docs.start && d < docs.end {
                        out.push(d);
                    }
                }
            }
            Some(out)
        }
        TQ::And(qs) => {
            let (mut bits, mut vars, mut rest) = (Vec::new(), Vec::new(), Vec::new());
            for q in qs {
                match q {
                    TQ::Tri(t) => match s.list(*t) {
                        Some(List::Bits(b)) => bits.push(b),
                        Some(List::Var(v)) => vars.push(v.deltas),
                        None => return Some(Vec::new()),
                    },
                    TQ::All => {}
                    q => rest.push(q),
                }
            }
            vars.sort_by_key(|v| v.len());
            let mut acc = None;
            if let Some((&first, vs)) = vars.split_first() {
                let mut v = Vec::new();
                List::Var(Var { deltas: first }).decode(docs.clone(), &mut v);
                for &b in &bits {
                    List::Bits(b).retain(&mut v);
                }
                for &x in vs {
                    if v.is_empty() {
                        break;
                    }
                    List::Var(Var { deltas: x }).retain(&mut v);
                }
                acc = Some(v);
            } else if !bits.is_empty() {
                let mut v = Vec::new();
                and_bits(&bits, docs.clone(), &mut v);
                acc = Some(v);
            }
            for q in rest {
                if acc.as_ref().is_some_and(Vec::is_empty) {
                    break;
                }
                if let Some(r) = eval(s, q, docs.clone()) {
                    acc = Some(match acc {
                        Some(a) => intersect(&a, &r),
                        None => r,
                    });
                }
            }
            acc
        }
        TQ::Or(qs) => {
            let mut acc: Vec<u32> = Vec::new();
            for q in qs {
                acc.extend(eval(s, q, docs.clone())?);
            }
            acc.sort_unstable();
            acc.dedup();
            Some(acc)
        }
    }
}

fn intersect(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut out = Vec::with_capacity(a.len().min(b.len()));
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                out.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    out
}

/// What a regex fragment tells us about its matches, as small sets of
/// (case-folded) strings: every match is one of `exact`, starts with one of
/// `prefix` and ends with one of `suffix` (None: no small such set); and
/// every doc holding a match satisfies `q`.
struct Info {
    exact: Option<Set>,
    prefix: Option<Set>,
    suffix: Option<Set>,
    q: TQ,
}

type Set = Vec<Vec<u8>>;

const MAX_EXACT: usize = 16;

impl Info {
    fn exact(set: Set) -> Info {
        Info { exact: Some(set.clone()), prefix: Some(set.clone()), suffix: Some(set), q: TQ::All }
    }

    fn any() -> Info {
        Info { exact: None, prefix: None, suffix: None, q: TQ::All }
    }

    /// All it says about a doc, as one trigram query.
    fn query(self) -> TQ {
        match self.exact {
            Some(_) => and(self.q, exact_query(self.exact)),
            None => and(and(self.q, exact_query(self.prefix)), exact_query(self.suffix)),
        }
    }
}

/// Every string of `a` followed by one of `b`, if that's few enough.
fn cross(a: &Option<Set>, b: &Option<Set>) -> Option<Set> {
    let (a, b) = (a.as_ref()?, b.as_ref()?);
    if a.len() * b.len() > MAX_EXACT {
        return None;
    }
    let mut set: Set = a.iter().flat_map(|x| b.iter().map(move |y| [x.as_slice(), y].concat())).collect();
    set.sort();
    set.dedup();
    Some(set)
}

/// What a set of strings requires of a doc: one of them (All when there is
/// no set, it is empty, or a string in it is too short to have a trigram).
/// Trigrams all of them share are required once, not per string.
fn exact_query(set: Option<Set>) -> TQ {
    let Some(set) = set.filter(|set| !set.is_empty() && set.iter().all(|s| s.len() >= 2)) else { return TQ::All };
    if !set.iter().all(|s| s.len() >= 3) {
        return TQ::Or(set.iter().map(|s| literal_plan(s)).collect());
    }
    let tris: Vec<Vec<u32>> = set.iter().map(|s| trigrams_small(s)).collect();
    let common: Vec<u32> = tris[0].iter().copied().filter(|t| tris.iter().all(|ts| ts.binary_search(t).is_ok())).collect();
    let rest: Vec<TQ> = tris.iter().map(|ts| TQ::And(ts.iter().filter(|t| !common.contains(t)).map(|&t| TQ::Tri(t)).collect())).collect();
    let mut q = TQ::And(common.into_iter().map(TQ::Tri).collect());
    if rest.iter().all(|r| !matches!(r, TQ::And(v) if v.is_empty())) {
        q = and(q, TQ::Or(rest));
    }
    q
}

fn and(a: TQ, b: TQ) -> TQ {
    match (a, b) {
        (TQ::All, x) | (x, TQ::All) => x,
        (TQ::And(mut x), TQ::And(y)) => {
            x.extend(y);
            TQ::And(x)
        }
        (TQ::And(mut x), y) | (y, TQ::And(mut x)) => {
            x.push(y);
            TQ::And(x)
        }
        (x, y) => TQ::And(vec![x, y]),
    }
}

/// Up to 8 members are each an exact string; with more, their first bytes,
/// if few (`\s`: tab to CR, space, and four UTF-8 lead bytes), start it.
fn class(c: &Class) -> Info {
    let mut set: Set = match c {
        Class::Unicode(u) => {
            u.ranges().iter().flat_map(|r| r.start()..=r.end()).take(9).map(|ch| ch.to_string().bytes().map(fold).collect()).collect()
        }
        Class::Bytes(b) => b.ranges().iter().flat_map(|r| r.start()..=r.end()).take(9).map(|x| vec![fold(x)]).collect(),
    };
    if set.len() <= 8 {
        set.sort();
        set.dedup();
        return Info::exact(set);
    }
    let lead = |c: char| c.to_string().as_bytes()[0];
    let mut first: Vec<u8> = match c {
        Class::Unicode(u) => u.ranges().iter().flat_map(|r| lead(r.start())..=lead(r.end())).map(fold).collect(),
        Class::Bytes(b) => b.ranges().iter().flat_map(|r| r.start()..=r.end()).map(fold).collect(),
    };
    first.sort();
    first.dedup();
    Info { prefix: (first.len() <= MAX_EXACT).then(|| first.into_iter().map(|b| vec![b]).collect()), ..Info::any() }
}

/// `a` then `b`. A match holds a's suffix right before b's prefix; that
/// pairing is carried up in the prefix or suffix when one side is exact,
/// else required here.
fn concat(a: Info, b: Info) -> Info {
    let exact = cross(&a.exact, &b.exact);
    let ab_prefix = a.exact.as_ref().and(cross(&a.exact, &b.prefix));
    let ab_suffix = b.exact.as_ref().and(cross(&a.suffix, &b.exact));
    let mut q = and(a.q, b.q);
    if exact.is_none() && ab_prefix.is_none() && ab_suffix.is_none() {
        q = match cross(&a.suffix, &b.prefix) {
            Some(j) => and(q, exact_query(Some(j))),
            None => {
                let sa = if a.exact.is_none() { exact_query(a.suffix.clone()) } else { TQ::All };
                let pb = if b.exact.is_none() { exact_query(b.prefix.clone()) } else { TQ::All };
                and(and(q, sa), pb)
            }
        };
    }
    let prefix = if a.exact.is_some() { ab_prefix.or(a.exact) } else { a.prefix };
    let suffix = if b.exact.is_some() { ab_suffix.or(b.exact) } else { b.suffix };
    Info { exact, prefix, suffix, q }
}

fn info(h: &Hir) -> Info {
    match h.kind() {
        HirKind::Empty | HirKind::Look(_) => Info::exact(vec![Vec::new()]),
        HirKind::Literal(l) => Info::exact(vec![l.0.iter().map(|&b| fold(b)).collect()]),
        HirKind::Class(c) => class(c),
        HirKind::Capture(c) => info(&c.sub),
        HirKind::Repetition(r) => {
            if r.min == 0 {
                return Info::any();
            }
            let i = info(&r.sub);
            if r.min == 1 && r.max == Some(1) {
                return i;
            }
            // At least one copy, which starts it; one ends it.
            Info { q: and(i.q, exact_query(i.exact)), exact: None, prefix: i.prefix, suffix: i.suffix }
        }
        HirKind::Concat(hs) => hs.iter().map(info).fold(Info::exact(vec![Vec::new()]), concat),
        HirKind::Alternation(hs) => {
            let parts: Vec<Info> = hs.iter().map(info).collect();
            let union = |f: fn(&Info) -> &Option<Set>| {
                let mut set = Set::new();
                for p in &parts {
                    set.extend(f(p).clone()?);
                }
                set.sort();
                set.dedup();
                (set.len() <= MAX_EXACT).then_some(set)
            };
            let (exact, prefix, suffix) = (union(|p| &p.exact), union(|p| &p.prefix), union(|p| &p.suffix));
            if exact.is_some() {
                return Info { exact, prefix, suffix, q: TQ::All };
            }
            let ors: Vec<TQ> = parts.into_iter().map(Info::query).collect();
            let q = if ors.iter().any(|q| matches!(q, TQ::All)) { TQ::All } else { TQ::Or(ors) };
            Info { exact, prefix, suffix, q }
        }
    }
}

fn regex_plan(h: &Hir) -> TQ {
    info(h).query()
}
/// Files to grep where the content index does not reach (e.g. `in:/etc`),
/// picked from the name index instead of crawling, newest first. `q` is the
/// name query restricted to the files worth reading.
pub fn scan_paths(live: &Live, mut q: Query) -> Vec<Vec<u8>> {
    q.kind = Some(KIND_FILE);
    q.limit = 200_000;
    q.size = (q.size.0, q.size.1.min(MAX_FILE));
    let mut files: Vec<(Vec<u8>, u32)> = (crate::query::Searcher { live }.search(&q).into_iter())
        .map(|h| match h.over {
            Some(path) => {
                let m = live.over[&path].mtime;
                (path, m)
            }
            None => {
                let mut p = Vec::new();
                live.base.path(h.idx as usize, &mut p);
                (p, live.base.mtime()[h.idx as usize])
            }
        })
        .collect();
    files.sort_by_key(|(_, m)| std::cmp::Reverse(*m));
    files.into_iter().map(|(p, _)| p).collect()
}

/// Open a path for reading only if it is a regular file, never blocking:
/// O_NONBLOCK keeps a FIFO from hanging open(), and the read threads'
/// "don't materialize dataless files" policy keeps iCloud placeholders from
/// being downloaded just because we searched.
pub fn open_regular(path: &[u8]) -> Option<std::fs::File> {
    open_sized(path).map(|(f, _)| f)
}

/// `open_regular`, and the file's size.
fn open_sized(path: &[u8]) -> Option<(std::fs::File, u64)> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::OpenOptionsExt;
    let f = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW).open(std::ffi::OsStr::from_bytes(path)).ok()?;
    let m = f.metadata().ok()?;
    m.is_file().then_some((f, m.len()))
}

/// `open_regular` for a file about to be indexed, telling the kernel to
/// start reading it now (as much as indexing reads).
fn open_ahead(path: &[u8]) -> Option<std::fs::File> {
    use std::os::unix::io::AsRawFd;
    let (f, len) = open_sized(path)?;
    #[cfg(target_os = "macos")]
    unsafe {
        let ra = libc::radvisory { ra_offset: 0, ra_count: len.min(MAX_FILE + 1) as libc::c_int };
        libc::fcntl(f.as_raw_fd(), libc::F_RDADVISE, &ra);
    }
    #[cfg(target_os = "linux")]
    unsafe {
        libc::posix_fadvise(f.as_raw_fd(), 0, len.min(MAX_FILE + 1) as i64, libc::POSIX_FADV_SEQUENTIAL);
    }
    Some(f)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::Query;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tov-search-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn mkdocs(dir: &Path, files: &[(&str, &[u8])]) -> Docs {
        let mut docs = Docs::default();
        for (n, body) in files {
            if let Some(p) = dir.join(n).parent() {
                std::fs::create_dir_all(p).unwrap();
            }
            std::fs::write(dir.join(n), body).unwrap();
            docs.push(dir.join(n).as_os_str().as_bytes(), body.len() as u64, 7);
        }
        docs
    }

    #[test]
    fn bloom_never_rejects_a_hit() {
        let dir = tmpdir("bloom");
        let content = dir.join("content");
        std::fs::create_dir_all(&content).unwrap();
        let docs = mkdocs(&dir, &[("a.txt", b"the quick brown fox jumps over"), ("b.txt", b"unrelated filler")]);
        let seg = build_segment(&content, 1, &docs, 0..2).unwrap();
        assert!(seg.fmt);
        // A gram present in a doc always survives its filter.
        let g = Grep::new("quick brown", GrepMode::Literal).unwrap();
        let probes = g.probes();
        assert!(seg.may_contain(0, &probes));
        // A doc without grams (binary, old-format) answers "maybe".
        let mut docs = mkdocs(&dir, &[("bin.dat", b"\x00\x01binary\x00")]);
        docs.items.push((docs.buf.len() as u32, 3, 4, 0));
        docs.buf.extend_from_slice(b"oth");
        let seg2 = build_segment(&content, 2, &docs, 0..docs.len()).unwrap();
        for d in 0..seg2.ndocs as u32 {
            assert!(seg2.bloom_of(d).is_empty() || seg2.may_contain(d, &probes));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_filter_means_maybe() {
        // may_contain must never let an empty filter veto (merged docs from
        // FSCSEG03 segments carry no bloom data).
        let dir = tmpdir("empty-filter");
        let content = dir.join("content");
        std::fs::create_dir_all(&content).unwrap();
        let docs = mkdocs(&dir, &[("bin.dat", b"\x00\x01\x02\x03nul"), ("a.txt", b"some text body")]);
        let seg = build_segment(&content, 1, &docs, 0..2).unwrap();
        let probes = Probes { short: vec![u32::MAX, 1, 2], long: vec![u32::MAX] };
        let binary = (0..2u32).find(|&d| seg.bloom_of(d).is_empty()).expect("a doc without a filter");
        assert!(seg.may_contain(binary, &probes));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bloom_skips_reads_for_absent_grams() {
        // A long enough filter that a foreign gram almost surely misses:
        // the file is never opened.
        let dir = tmpdir("bloom-skip");
        let content = dir.join("content");
        std::fs::create_dir_all(&content).unwrap();
        let body: Vec<u8> = (0..60_000).map(|i| b'a' + (i % 23) as u8).collect();
        let docs = mkdocs(&dir, &[("big.txt", &body)]);
        let seg = build_segment(&content, 1, &docs, 0..1).unwrap();
        let g = Grep::new("{{{{{{{{", GrepMode::Literal).unwrap();
        let probes = g.probes();
        assert!(!probes.short.is_empty());
        assert!(!seg.may_contain(0, &probes));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_byte_literal_uses_pair_key() {
        let dir = tmpdir("pair");
        let content = dir.join("content");
        std::fs::create_dir_all(&content).unwrap();
        let docs = mkdocs(&dir, &[("a.txt", b"line with ab inside"), ("b.txt", b"nothing alike")]);
        let seg = build_segment(&content, 1, &docs, 0..2).unwrap();
        match literal_plan(b"ab") {
            TQ::Pair(p) => {
                let mut docs_hit = Vec::new();
                if let Some(list) = eval(&seg, &TQ::Pair(p), 0..seg.ndocs as u32) {
                    docs_hit = list;
                }
                assert_eq!(docs_hit, vec![0]);
            }
            other => panic!("expected Pair, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn segment_save_load_round_trip() {
        let dir = tmpdir("roundtrip");
        let content = dir.join("content");
        std::fs::create_dir_all(&content).unwrap();
        let files: Vec<(&str, &[u8])> =
            vec![("alpha.rs", b"fn main() { println!(\"hi\"); }"), ("beta/b.py", b"import os\nprint(os.name)\n"), ("bin.dat", b"\x00\x01\x02")];
        let docs = mkdocs(&dir, &files);
        let seg = build_segment(&content, 1, &docs, 0..docs.len()).unwrap();
        drop(seg);
        let seg = Segment::load(&content, 1).unwrap();
        assert_eq!(seg.ndocs, files.len());
        let mut paths: Vec<&[u8]> = (0..seg.ndocs as u32).map(|d| seg.path(d)).collect();
        paths.sort();
        let mut want: Vec<Vec<u8>> = files.iter().map(|(n, _)| dir.join(n).as_os_str().as_bytes().to_vec()).collect();
        want.sort();
        assert_eq!(paths, want.iter().map(|v| &v[..]).collect::<Vec<_>>());
        assert!(seg.fmt);
        // Trigram postings identical to a rebuild.
        let seg2 = build_segment(&content, 2, &docs, 0..docs.len()).unwrap();
        assert_eq!(seg.tri_key(), seg2.tri_key());
        let (mut a, mut b) = (Vec::new(), Vec::new());
        for t in 0..seg.tri_key().len().min(200) {
            a.clear();
            b.clear();
            seg.list_into(t, &mut a);
            seg2.list_into(t, &mut b);
            assert_eq!(a, b);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn merge_preserves_bloom_and_paths() {
        let dir = tmpdir("merge");
        let content = dir.join("content");
        std::fs::create_dir_all(&content).unwrap();
        let docs = mkdocs(&dir, &[("a.txt", b"alpha body"), ("b.txt", b"beta body"), ("c.txt", b"gamma body")]);
        let s1 = build_segment(&content, 1, &docs, 0..2).unwrap();
        let s2 = build_segment(&content, 2, &docs, 2..3).unwrap();
        let m = merge(&content, 3, &[&s1, &s2]).unwrap();
        assert_eq!(m.ndocs, 3);
        // Every doc carries its filter through the merge.
        let g = Grep::new("alpha", GrepMode::Literal).unwrap();
        let probes = g.probes();
        let hits: Vec<u32> = (0..3u32).filter(|&d| m.may_contain(d, &probes)).collect();
        assert_eq!(hits, vec![0]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn probes_disabled_for_regex_and_ambiguous_case() {
        assert!(Grep::new("ab.*", GrepMode::Regex).unwrap().probes().short.is_empty());
        // ASCII-foldable bytes (K/S fold to two codepoints) and non-ASCII
        // bytes can't be probed on the folded index.
        assert!(Grep::new("kontakt", GrepMode::Literal).unwrap().probes().short.is_empty());
        assert!(Grep::new("m\u{00fc}nchen", GrepMode::Literal).unwrap().probes().short.is_empty());
        assert!(!Grep::new("plain literal", GrepMode::Literal).unwrap().probes().short.is_empty());
    }

    #[test]
    fn grep_search_reads_only_probe_survivors() {
        let dir = tmpdir("grep-reads");
        let content = dir.join("content");
        std::fs::create_dir_all(&content).unwrap();
        let hay: Vec<u8> = (0..80_000).map(|i| b'a' + (i % 17) as u8).collect();
        let docs = mkdocs(&dir, &[("hit.txt", b"prefix needle-123 suffix"), ("miss.txt", &hay)]);
        let seg = build_segment(&content, 1, &docs, 0..2).unwrap();
        let mut c = Content::open_shared(content.clone());
        c.push(seg);
        let g = Grep::new("needle-123", GrepMode::Literal).unwrap();
        let r = c.search(&g, &Query::parse("", "/").unwrap());
        assert_eq!(r.files.len(), 1);
        assert_eq!(r.files[0].path.as_slice(), dir.join("hit.txt").as_os_str().as_bytes());
        assert_eq!(r.read, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
