//! Name search: parse a query, scan the index in parallel, rank, top-k.

use crate::index::{Index, char_bit, start_bit};
use crate::live::{Blk, Live, OBLK};
use crate::walk::{FLAG_HIDDEN, KIND_DIR, KIND_FILE, KIND_LINK};
use rayon::prelude::*;
use std::collections::HashMap;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Mode {
    Fuzzy,
    Exact,
    Prefix,
    Suffix,
}

#[derive(Clone, Debug)]
pub struct Token {
    pub text: Vec<u8>,
    pub mask: u64,
    pub mode: Mode,
    pub negate: bool,
    /// Char classes a typo may leave out of a matching name: all but the
    /// first letter's, or none when the token takes no typos.
    pub loose: u64,
    /// `index::start_bit` of the first letter when the token takes typos.
    pub start: u64,
}

impl Token {
    /// Can a name with mask `m` (`index::name_mask`) match? Cleanly it has
    /// every char class; with a typo, a word in it starts with this token's
    /// first letter and at most one `loose` class is missing. Branchless, so
    /// the scan over every name stays vectorized.
    #[inline(always)]
    fn fits(&self, m: u64) -> bool {
        let miss = self.mask & !m;
        (miss == 0) | ((((miss & !self.loose) | (miss & miss.wrapping_sub(1))) == 0) & (m & self.start != 0))
    }

    /// For negation: does `fits(m)` already prove `token_matches`? Only when
    /// matching reduces to char presence — a one-byte Exact or Fuzzy token
    /// whose char has a unique mask bit (letters, digits, '.', ' ').
    fn mask_covers(&self) -> bool {
        self.text.len() == 1
            && matches!(self.mode, Mode::Exact | Mode::Fuzzy)
            && (self.text[0].is_ascii_alphanumeric() || self.text[0] == b'.' || self.text[0] == b' ')
    }
}

/// Fuzzy words this long forgive one typo (see `typo_score`).
const TYPO_MIN_LEN: usize = 5;
/// What a typo costs, so clean matches of the same quality rank first.
const TYPO_COST: i32 = 60;

fn takes_typos(text: &[u8], mode: Mode) -> bool {
    mode == Mode::Fuzzy && text.len() >= TYPO_MIN_LEN
}

#[derive(Clone, Default)]
pub struct Query {
    pub tokens: Vec<Token>,
    pub kind: Option<u8>,
    pub exts: Vec<Vec<u8>>,
    pub scope: Option<Vec<u8>>,
    pub size: (u64, u64),
    pub mtime: (u32, u32),
    pub name_re: Option<regex::bytes::Regex>,
    pub path_re: Option<regex::bytes::Regex>,
    pub limit: usize,
    /// Content search: handled by the content layer, carried here so one
    /// query string can say everything.
    pub grep: Option<String>,
    pub grep_mode: GrepMode,
}

#[derive(Clone, Copy, Default, PartialEq, Debug)]
pub enum GrepMode {
    #[default]
    Literal,
    Regex,
    Symbol,
}

pub struct Hit {
    pub score: i32,
    /// Base entry, or u32::MAX for an overlay entry (then `over` is its path).
    pub idx: u32,
    pub over: Option<Vec<u8>>,
}

#[rustfmt::skip]
const TYPES: &[(&str, &[&str])] = &[
    ("image", &["png", "jpg", "jpeg", "gif", "heic", "heif", "webp", "tiff", "tif", "bmp", "svg", "raw", "cr2", "cr3", "nef", "arw", "dng", "psd", "ico", "icns", "avif", "jxl"]),
    ("video", &["mp4", "mov", "m4v", "mkv", "avi", "webm", "wmv", "flv", "mpg", "mpeg", "3gp", "hevc"]),
    ("audio", &["mp3", "m4a", "aac", "wav", "flac", "aiff", "aif", "ogg", "opus", "alac", "caf", "mid", "midi", "m4r"]),
    ("doc", &["pdf", "doc", "docx", "pages", "txt", "md", "rtf", "odt", "key", "ppt", "pptx", "numbers", "xls", "xlsx", "csv", "epub", "tex"]),
    ("code", &["rs", "c", "h", "cc", "cpp", "hpp", "m", "mm", "swift", "go", "py", "js", "mjs", "cjs", "ts", "tsx", "jsx", "java", "kt", "rb", "php", "cs", "sh", "zsh", "bash", "fish", "lua", "sql", "html", "css", "scss", "json", "yaml", "yml", "toml", "xml", "vue", "svelte", "zig", "nim", "hs", "ml", "ex", "exs", "erl", "clj", "dart", "r", "jl", "metal", "glsl", "wgsl", "proto", "graphql", "nix"]),
    ("archive", &["zip", "tar", "gz", "tgz", "bz2", "xz", "7z", "rar", "dmg", "pkg", "iso", "zst", "lz4", "xip"]),
    ("font", &["ttf", "otf", "woff", "woff2", "ttc", "dfont"]),
];

impl Query {
    /// Parse the human query language. Plain words are fuzzy tokens;
    /// `'x` exact, `^x` prefix, `x$` suffix, `!x` negate; filters are
    /// `ext: type: kind: in: size: mtime: re: path: limit: grep: regex: sym:`.
    pub fn parse(s: &str, home: &str) -> Result<Query, String> {
        let mut q = Query { size: (0, u64::MAX), mtime: (0, u32::MAX), limit: 50, ..Default::default() };
        for word in split_words(s) {
            if let Some((k, v)) = word.split_once(':')
                && q.filter(k, v, home)?
            {
                continue;
            }
            for piece in word.split('/').filter(|p| !p.is_empty()) {
                q.push_token(piece);
            }
        }
        Ok(q)
    }

    pub fn push_token(&mut self, w: &str) {
        let (mut t, mut negate, mut mode) = (w, false, Mode::Fuzzy);
        if let Some(r) = t.strip_prefix('!') {
            (t, negate, mode) = (r, true, Mode::Exact);
        }
        if let Some(r) = t.strip_prefix('\'') {
            (t, mode) = (r, Mode::Exact);
        } else if let Some(r) = t.strip_prefix('^') {
            (t, mode) = (r, Mode::Prefix);
        } else if let Some(r) = t.strip_suffix('$') {
            (t, mode) = (r, Mode::Suffix);
        }
        // Positive tokens are tracked in a u8 bitset.
        if t.is_empty() || (!negate && self.tokens.iter().filter(|t| !t.negate).count() >= 8) {
            return;
        }
        let text: Vec<u8> = t.bytes().map(|b| b.to_ascii_lowercase()).collect();
        let mask = text.iter().fold(0, |m, &b| m | char_bit(b));
        let (loose, start) = if takes_typos(&text, mode) { (mask & !char_bit(text[0]), start_bit(text[0])) } else { (0, 0) };
        self.tokens.push(Token { text, mask, mode, negate, loose, start });
    }

    /// Apply filter `k:v`; false if `k` is not a filter name.
    pub fn filter(&mut self, k: &str, v: &str, home: &str) -> Result<bool, String> {
        match k {
            "ext" => self.exts.extend(v.split(',').map(|e| e.trim_start_matches('.').to_ascii_lowercase().into_bytes())),
            "type" => {
                for t in v.split(',') {
                    if t == "app" {
                        self.kind = Some(KIND_DIR);
                        self.exts.push(b"app".to_vec());
                        continue;
                    }
                    let (_, exts) = TYPES.iter().find(|(n, _)| *n == t).ok_or(format!("unknown type {t}"))?;
                    self.exts.extend(exts.iter().map(|e| e.as_bytes().to_vec()));
                }
            }
            "kind" => {
                self.kind = Some(match v {
                    "file" | "f" => KIND_FILE,
                    "dir" | "folder" | "d" => KIND_DIR,
                    "link" | "symlink" | "l" => KIND_LINK,
                    _ => return Err(format!("unknown kind {v}")),
                })
            }
            "in" => {
                let p = v.strip_prefix('~').map_or(v.to_string(), |r| format!("{home}{r}"));
                // The index holds real paths: /etc is /private/etc.
                let p = std::fs::canonicalize(&p).map_or(p, |c| c.to_string_lossy().into_owned());
                self.scope = Some(p.trim_end_matches('/').as_bytes().to_vec());
            }
            "size" => self.size = range(v, parse_size)?,
            "mtime" | "modified" => {
                // mtime:<7d means "modified within the last 7 days".
                let now = now_secs();
                let (lo, hi) = range(v, parse_age)?;
                self.mtime = (now.saturating_sub(hi.min(now as u64) as u32), now.saturating_sub(lo as u32));
                if hi == u64::MAX {
                    self.mtime.0 = 0;
                }
            }
            "re" => self.name_re = Some(regex::bytes::Regex::new(&format!("(?i){v}")).map_err(|e| e.to_string())?),
            "path" => self.path_re = Some(regex::bytes::Regex::new(&format!("(?i){v}")).map_err(|e| e.to_string())?),
            "limit" => self.limit = v.parse().map_err(|_| "bad limit")?,
            "grep" | "content" => (self.grep, self.grep_mode) = (Some(v.to_string()), GrepMode::Literal),
            "regex" => (self.grep, self.grep_mode) = (Some(v.to_string()), GrepMode::Regex),
            "sym" | "symbol" => (self.grep, self.grep_mode) = (Some(v.to_string()), GrepMode::Symbol),
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// The parts of a query that pick files for a content scan.
    pub fn clone_for_scan(&self) -> Query {
        Query { grep: None, ..self.clone() }
    }

    /// Does a full path pass every filter and token? Returns the match score.
    /// Used where there is no dir memo: the overlay and content-search docs.
    pub fn match_path(&self, path: &[u8], kind: u8, size: u64, mtime: u32) -> Option<i32> {
        self.match_path_with(path, kind, size, mtime, |dirs| self.dir_match(dirs))
    }

    /// `match_path`, with the folder half (`dir_match` of the path's folder
    /// part) supplied by the caller, who can memoize it per folder.
    pub fn match_path_with(&self, path: &[u8], kind: u8, size: u64, mtime: u32, dirs: impl FnOnce(&[u8]) -> DirMatch) -> Option<i32> {
        if let Some(s) = &self.scope {
            if !(path.starts_with(s) && path.get(s.len()) == Some(&b'/')) {
                return None;
            }
        }
        let cut = path.iter().rposition(|&b| b == b'/').unwrap_or(0);
        let name = &path[cut + 1..];
        if name.is_empty()
            || self.kind.is_some_and(|k| kind & 3 != k)
            || (!self.exts.is_empty() && !ext_ok(name, &self.exts))
            || size < self.size.0
            || size > self.size.1
            || mtime < self.mtime.0
            || mtime > self.mtime.1
        {
            return None;
        }
        // A hit needs some token in its own name; most paths fail here,
        // before the folders are looked at.
        let mut pos = self.tokens.iter().filter(|t| !t.negate).peekable();
        let m = crate::index::name_mask(name);
        if pos.peek().is_some() && !pos.any(|t| t.fits(m) && token_score(name, m, t).is_some()) {
            return None;
        }
        if self.tokens.iter().any(|t| t.negate && token_matches(name, t)) {
            return None;
        }
        let d = dirs(&path[..cut]);
        if d.negated {
            return None;
        }
        let pos: Vec<&Token> = self.tokens.iter().filter(|t| !t.negate).collect();
        let all = (1u32 << pos.len()) - 1;
        let (mut got, mut inherited, mut score) = (0u32, 0u32, 0i32);
        for (t, tok) in pos.iter().enumerate() {
            if let Some(s) = token_score(name, m, tok) {
                got |= 1 << t;
                score += s;
            } else if let Some(s) = d.best[t] {
                inherited |= 1 << t;
                score += s * 3 / 4;
            }
        }
        if !pos.is_empty() && (got == 0 || (got | inherited) != all) {
            return None;
        }
        if self.name_re.as_ref().is_some_and(|re| !re.is_match(name)) || self.path_re.as_ref().is_some_and(|re| !re.is_match(path)) {
            return None;
        }
        Some(score)
    }

    /// How the folders of a path (`/a/b` for `/a/b/name`) match the tokens:
    /// the best score per positive token, and whether a negated one hits.
    pub fn dir_match(&self, dirs: &[u8]) -> DirMatch {
        let comps: Vec<&[u8]> = dirs.split(|&b| b == b'/').filter(|c| !c.is_empty()).collect();
        let mut d = DirMatch { negated: false, best: [None; 8] };
        d.negated = self.tokens.iter().any(|t| t.negate && comps.iter().any(|c| token_matches(c, t)));
        for (t, tok) in self.tokens.iter().filter(|t| !t.negate).enumerate() {
            d.best[t] = comps.iter().filter_map(|c| token_score(c, !0, tok)).max();
        }
        d
    }
}

/// See `Query::dir_match`.
#[derive(Clone, Copy)]
pub struct DirMatch {
    negated: bool,
    best: [Option<i32>; 8],
}

/// Split on spaces, keeping "double quoted" runs together.
fn split_words(s: &str) -> Vec<String> {
    let (mut out, mut cur, mut quoted) = (Vec::new(), String::new(), false);
    for c in s.chars() {
        match c {
            '"' => quoted = !quoted,
            ' ' if !quoted => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn range(v: &str, p: fn(&str) -> Option<u64>) -> Result<(u64, u64), String> {
    let bad = || format!("bad range {v}");
    if let Some(r) = v.strip_prefix(">=").or(v.strip_prefix('>')) {
        return Ok((p(r).ok_or_else(bad)?, u64::MAX));
    }
    if let Some(r) = v.strip_prefix("<=").or(v.strip_prefix('<')) {
        return Ok((0, p(r).ok_or_else(bad)?));
    }
    if let Some((a, b)) = v.split_once("..") {
        return Ok((p(a).ok_or_else(bad)?, p(b).ok_or_else(bad)?));
    }
    let x = p(v).ok_or_else(bad)?;
    Ok((x, x))
}

fn split_unit(s: &str) -> (f64, String) {
    let i = s.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(s.len());
    (s[..i].parse().unwrap_or(f64::NAN), s[i..].to_ascii_lowercase())
}

fn parse_size(s: &str) -> Option<u64> {
    let (n, u) = split_unit(s);
    let m = match u.as_str() {
        "" | "b" => 1.0,
        "k" | "kb" => 1e3,
        "m" | "mb" => 1e6,
        "g" | "gb" => 1e9,
        "t" | "tb" => 1e12,
        _ => return None,
    };
    (!n.is_nan()).then(|| (n * m) as u64)
}

fn parse_age(s: &str) -> Option<u64> {
    let (n, u) = split_unit(s);
    let m = match u.as_str() {
        "s" => 1.0,
        "m" | "min" => 60.0,
        "h" => 3600.0,
        "" | "d" => 86400.0,
        "w" => 604800.0,
        "mo" => 2592000.0,
        "y" => 31536000.0,
        _ => return None,
    };
    (!n.is_nan()).then(|| (n * m) as u64)
}

/// Pins `now_secs` (0: real time), so ranking by age is reproducible in
/// benchmarks.
pub static NOW_OVERRIDE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

pub fn now_secs() -> u32 {
    let pinned = NOW_OVERRIDE.load(std::sync::atomic::Ordering::Relaxed);
    if pinned != 0 {
        return pinned;
    }
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as u32)
}

#[inline(always)]
pub(crate) fn fold(b: u8) -> u8 {
    b | (((b.wrapping_sub(b'A') < 26) as u8) << 5)
}

#[inline(always)]
fn is_subseq(name: &[u8], q: &[u8]) -> bool {
    let mut j = 0;
    for &b in name {
        if fold(b) == q[j] {
            j += 1;
            if j == q.len() {
                return true;
            }
        }
    }
    false
}

fn find_ci(name: &[u8], q: &[u8]) -> Option<usize> {
    if q.len() > name.len() {
        return None;
    }
    if q.is_empty() {
        return Some(0);
    }
    // Jump between positions where the first byte folds (memchr is SIMD);
    // most names have none. Same first match as scanning every window.
    let mut at = 0;
    while let Some(i) = find_folded(&name[at..], q[0]) {
        let s = at + i;
        if s + q.len() > name.len() {
            return None;
        }
        if name[s + 1..s + q.len()].iter().zip(&q[1..]).all(|(&a, &b)| fold(a) == b) {
            return Some(s);
        }
        at = s + 1;
    }
    None
}

#[derive(Clone, Copy, PartialEq)]
enum Class {
    Lower,
    Upper,
    Digit,
    Delim,
    Other,
}

const fn class_of(b: u8) -> Class {
    match b {
        b'a'..=b'z' => Class::Lower,
        b'A'..=b'Z' => Class::Upper,
        b'0'..=b'9' => Class::Digit,
        b' ' | b'_' | b'-' | b'.' | b'/' | b'(' | b')' | b'[' | b']' | b',' | b'+' | b'@' => Class::Delim,
        _ => Class::Other,
    }
}

/// `class_of` for every byte: one load instead of a chain of range tests.
static CLASS: [Class; 256] = {
    let mut t = [Class::Other; 256];
    let mut b = 0;
    while b < 256 {
        t[b] = class_of(b as u8);
        b += 1;
    }
    t
};

#[inline(always)]
fn class(b: u8) -> Class {
    CLASS[b as usize]
}

const SCORE_MATCH: i32 = 16;
const GAP_START: i32 = -3;
const GAP_EXT: i32 = -1;
const BONUS_BOUNDARY: i32 = 8;
const BONUS_CAMEL: i32 = 7;
const BONUS_CONSEC: i32 = 4;

#[inline(always)]
fn bonus(prev: Class, cur: Class) -> i32 {
    match (prev, cur) {
        (Class::Delim, c) if c != Class::Delim => BONUS_BOUNDARY,
        (Class::Lower, Class::Upper) | (Class::Lower | Class::Upper, Class::Digit) => BONUS_CAMEL,
        _ => 0,
    }
}

/// fzf-v1 style: leftmost-ending match, shrunk from the right, then scored
/// with boundary/camel/consecutive bonuses. Returns None when no match.
pub fn fuzzy_score(name: &[u8], q: &[u8]) -> Option<i32> {
    fuzzy_score_capped(name, q, 100)
}

/// NEON name→folded-bytes blocks (`NC` 16-byte loads, up to 128 bytes):
/// per-char equality reduced to u64 position bitmasks, so the forward,
/// shrink and scoring passes share each char's mask instead of rescanning.
/// Bytes past `name.len()` are whatever follows it in memory (or 0 in the
/// copy); every consumer bounds matches below `name.len()`.
#[cfg(target_arch = "aarch64")]
mod mask64 {
    use core::arch::aarch64::*;

    pub(crate) struct Fold<const NC: usize> {
        v: [uint8x16_t; NC],
    }

    pub(crate) type Fold64 = Fold<4>;

    #[inline(always)]
    unsafe fn fold_v(v: uint8x16_t) -> uint8x16_t {
        unsafe {
            let up = vandq_u8(vcgeq_u8(v, vdupq_n_u8(b'A')), vcleq_u8(v, vdupq_n_u8(b'Z')));
            vorrq_u8(v, vandq_u8(up, vdupq_n_u8(0x20)))
        }
    }

    /// Four byte-lane predicates (0xFF/0x00) -> one u64, bit 16j+i = lane i
    /// of the j-th: weight each lane, then three pairwise-add rounds.
    #[inline(always)]
    unsafe fn mask4(a: uint8x16_t, b: uint8x16_t, c: uint8x16_t, d: uint8x16_t) -> u64 {
        const W: [u8; 16] = [1, 2, 4, 8, 16, 32, 64, 128, 1, 2, 4, 8, 16, 32, 64, 128];
        unsafe {
            let w = vld1q_u8(W.as_ptr());
            let s = vpaddq_u8(vpaddq_u8(vandq_u8(a, w), vandq_u8(b, w)), vpaddq_u8(vandq_u8(c, w), vandq_u8(d, w)));
            vgetq_lane_u64(vreinterpretq_u64_u8(vpaddq_u8(s, s)), 0)
        }
    }

    impl<const NC: usize> Fold<NC> {
        /// `name.len() <= NC * 16`, `NC` 4 or 8.
        #[inline(always)]
        pub(crate) fn new(name: &[u8]) -> Self {
            let fold = |p: *const u8| unsafe { core::array::from_fn(|j| fold_v(vld1q_u8(p.add(16 * j)))) };
            // NC * 16 bytes from the name cannot fault if they stay on its
            // page (an empty slice's pointer may dangle, so it takes the copy).
            let v = if !name.is_empty() && (name.as_ptr() as usize & 4095) <= 4096 - NC * 16 {
                fold(name.as_ptr())
            } else {
                let mut buf = [0u8; 128];
                buf[..name.len()].copy_from_slice(name);
                fold(buf.as_ptr())
            };
            Self { v }
        }

        /// Positions `64 * w ..` where folded(name[i]) == c.
        #[inline(always)]
        pub(crate) fn eq_at(&self, c: u8, w: usize) -> u64 {
            unsafe {
                let vc = vdupq_n_u8(c);
                let e = |j: usize| vceqq_u8(self.v[4 * w + j], vc);
                mask4(e(0), e(1), e(2), e(3))
            }
        }

        /// Positions where folded(name[i]) == c.
        #[inline(always)]
        pub(crate) fn eq(&self, c: u8) -> u64 {
            self.eq_at(c, 0)
        }
    }
}

/// AVX2 twin of the NEON `Fold64`: the name is folded in two 32-byte
/// vectors and each query char's equality mask is a compare + movemask.
/// Bytes past `name.len()` are whatever follows it in memory (or 0 in the
/// copy); `fuzzy_mask64` bounds every match below `name.len()`.
#[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
mod mask64 {
    use core::arch::x86_64::*;

    pub(crate) struct Fold64 {
        lo: __m256i,
        hi: __m256i,
    }

    #[inline(always)]
    unsafe fn fold_v(v: __m256i) -> __m256i {
        unsafe {
            // Signed compares: bytes >= 0x80 are negative, never in A..=Z.
            let up = _mm256_and_si256(_mm256_cmpgt_epi8(v, _mm256_set1_epi8(b'A' as i8 - 1)), _mm256_cmpgt_epi8(_mm256_set1_epi8(b'Z' as i8 + 1), v));
            _mm256_or_si256(v, _mm256_and_si256(up, _mm256_set1_epi8(0x20)))
        }
    }

    impl Fold64 {
        /// `name.len() <= 64`.
        #[inline(always)]
        pub(crate) fn new(name: &[u8]) -> Self {
            unsafe {
                let p = name.as_ptr();
                // 64 bytes from `p` cannot fault if they stay on its page (an
                // empty slice's pointer may dangle, so it takes the copy).
                let (lo, hi) = if !name.is_empty() && (p as usize & 4095) <= 4096 - 64 {
                    (_mm256_loadu_si256(p as *const __m256i), _mm256_loadu_si256(p.add(32) as *const __m256i))
                } else {
                    let mut buf = [0u8; 64];
                    buf[..name.len()].copy_from_slice(name);
                    (_mm256_loadu_si256(buf.as_ptr() as *const __m256i), _mm256_loadu_si256(buf.as_ptr().add(32) as *const __m256i))
                };
                Self { lo: fold_v(lo), hi: fold_v(hi) }
            }
        }

        /// Positions where folded(name[i]) == c.
        #[inline(always)]
        pub(crate) fn eq(&self, c: u8) -> u64 {
            unsafe {
                let vc = _mm256_set1_epi8(c as i8);
                let l = _mm256_movemask_epi8(_mm256_cmpeq_epi8(self.lo, vc)) as u32 as u64;
                let h = _mm256_movemask_epi8(_mm256_cmpeq_epi8(self.hi, vc)) as u32 as u64;
                l | (h << 32)
            }
        }
    }
}

/// `fuzzy_score_capped` over u64 position masks: identical algorithm to the
/// memchr path — leftmost-ending greedy match, shrink from the right, then
/// rescore — but each query char's equality mask is computed once in the
/// forward pass (`eqm`) and reused by the shrink and scoring passes.
#[cfg(any(target_arch = "aarch64", all(target_arch = "x86_64", target_feature = "avx2")))]
#[inline(always)]
fn fuzzy_mask64(name: &[u8], q: &[u8], cap: i32) -> Option<i32> {
    // `eqm` is zeroed per call: size it to the query.
    if q.len() <= 16 { fuzzy_mask64_n::<16>(name, q, cap) } else { fuzzy_mask64_n::<64>(name, q, cap) }
}

#[cfg(any(target_arch = "aarch64", all(target_arch = "x86_64", target_feature = "avx2")))]
fn fuzzy_mask64_n<const N: usize>(name: &[u8], q: &[u8], cap: i32) -> Option<i32> {
    let nl = name.len();
    let ql = q.len();
    let f = mask64::Fold64::new(name);
    let nm: u64 = if nl >= 64 { !0 } else { (1u64 << nl) - 1 };
    let mut eqm = [0u64; N];
    let mut avail = nm;
    let mut end = 0usize;
    for (k, &c) in q.iter().enumerate() {
        let e = f.eq(c);
        let mk = e & avail;
        if mk == 0 {
            return None;
        }
        eqm[k] = e;
        end = mk.trailing_zeros() as usize;
        avail = if end >= 63 { 0 } else { avail & (!0u64 << (end + 1)) };
    }
    if ql == 1 {
        return Some(single_score(name, end, cap));
    }
    // Latest start that still ends at `end`: q[ql-1] already sits at `end`,
    // so the shrink walks q[ql-2..0] over positions strictly below it.
    let mut start = end;
    let mut below = nm & ((1u64 << end) - 1);
    for kk in 0..ql - 1 {
        let k = ql - 2 - kk;
        let mk = eqm[k] & below;
        if mk == 0 {
            return None;
        }
        let pos = 63 - mk.leading_zeros() as usize;
        start = pos;
        below &= (1u64 << pos) - 1;
    }
    let mut score = 0;
    let mut at = start;
    let mut first_bonus = 0;
    for (k, _) in q.iter().enumerate() {
        let mut run = false;
        if k > 0 {
            let last = at;
            let rng = (if last + 1 >= 64 { 0 } else { !0u64 << (last + 1) }) & (if end >= 63 { !0u64 } else { (1u64 << (end + 1)) - 1 });
            let mk = eqm[k] & rng;
            if mk == 0 {
                return None;
            }
            at = mk.trailing_zeros() as usize;
            run = at == last + 1;
            if !run {
                score += GAP_START + (at - last - 2) as i32 * GAP_EXT;
            }
        }
        let prev = if at == 0 { Class::Delim } else { class(name[at - 1]) };
        let mut b = bonus(prev, class(name[at]));
        if run {
            if b >= BONUS_BOUNDARY && b > first_bonus {
                first_bonus = b;
            }
            b = b.max(first_bonus).max(BONUS_CONSEC);
        } else {
            first_bonus = b;
        }
        score += SCORE_MATCH + if k == 0 { b * 2 } else { b };
    }
    let off = (nl > 1 && name[0] == b'.') as usize;
    // Only a contiguous match at the name's start earns a placement bonus.
    let placed = if start == off && end + 1 - start == ql {
        if end + 1 == nl {
            100
        } else if end + 1 == memchr::memrchr(b'.', name).filter(|&p| p > off).unwrap_or(nl) {
            80
        } else {
            30
        }
    } else {
        0
    };
    Some(score + placed.min(cap) - (nl as i32).min(80) / 3)
}

/// `fuzzy_score` with the whole-name/stem/prefix bonus capped at `cap`.
fn fuzzy_score_capped(name: &[u8], q: &[u8], cap: i32) -> Option<i32> {
    // Short query + short name: one masked vector load gives every byte's
    // position as a bitmask, so the forward/backward/scoring walks become
    // bit ops instead of dependent memchr rescans. (Intel lane)
    #[cfg(all(target_arch = "x86_64", target_feature = "avx512bw"))]
    if q.len() >= 2 && q.len() <= 16 && name.len() <= 128 {
        return fused_score(name, q, cap);
    }
    // 64-bit word masks over name bytes give the same score without
    // scalar rescans on Apple Silicon. (Mac lane, certified keep1)
    #[cfg(any(target_arch = "aarch64", all(target_arch = "x86_64", target_feature = "avx2")))]
    if name.len() <= 64 && q.len() <= 64 && !q.is_empty() {
        return fuzzy_mask64(name, q, cap);
    }
    // 65..=128-byte names: the same masks in a (lo, hi) pair.
    #[cfg(any(target_arch = "aarch64", all(target_arch = "x86_64", target_feature = "avx2", not(target_feature = "avx512bw"))))]
    if name.len() <= 128 && (2..=16).contains(&q.len()) {
        return fused_score(name, q, cap);
    }
    // Leftmost-ending match: jump to each query byte in turn (memchr is
    // SIMD; most names fail on the first or second byte).
    let mut end = 0;
    let mut from = 0;
    for &c in q {
        end = from + find_folded(&name[from..], c)?;
        from = end + 1;
    }
    if q.len() == 1 {
        return Some(single_score(name, end, cap));
    }
    // Shrink from the right: the latest start that still ends at `end`.
    let mut start = end + 1;
    for &c in q.iter().rev() {
        start = rfind_folded(&name[..start], c)?;
    }
    // Score the greedy match from `start`, jumping between matched bytes:
    // each gap costs GAP_START then GAP_EXT per byte, a run of consecutive
    // matches carries its strongest boundary bonus along.
    let mut score = 0;
    let (mut at, mut first_bonus) = (start, 0);
    for (k, &c) in q.iter().enumerate() {
        let mut run = false;
        if k > 0 {
            let last = at;
            at = last + 1 + find_folded(&name[last + 1..=end], c)?;
            run = at == last + 1;
            if !run {
                score += GAP_START + (at - last - 2) as i32 * GAP_EXT;
            }
        }
        let prev = if at == 0 { Class::Delim } else { class(name[at - 1]) };
        let mut b = bonus(prev, class(name[at]));
        if run {
            if b >= BONUS_BOUNDARY && b > first_bonus {
                first_bonus = b;
            }
            b = b.max(first_bonus).max(BONUS_CONSEC);
        } else {
            first_bonus = b;
        }
        score += SCORE_MATCH + if k == 0 { b * 2 } else { b };
    }
    // Whole-name and stem matches are what people mean most of the time. A
    // leading dot doesn't count: "zshrc" means ~/.zshrc.
    let off = (name.len() > 1 && name[0] == b'.') as usize;
    let stem = memchr::memrchr(b'.', name).filter(|&p| p > off).unwrap_or(name.len());
    let contiguous = end + 1 - start == q.len();
    let placed = if start == off && contiguous && end + 1 == name.len() {
        100
    } else if start == off && contiguous && end + 1 == stem {
        80
    } else if start == off && contiguous {
        30
    } else {
        0
    };
    Some(score + placed.min(cap) - (name.len() as i32).min(80) / 3)
}

/// First byte of `s` that folds to `c` (an already-lowercased query byte).
#[inline(always)]
fn find_folded(s: &[u8], c: u8) -> Option<usize> {
    if c.is_ascii_lowercase() { memchr::memchr2(c, c - 32, s) } else { memchr::memchr(c, s) }
}

/// Last byte of `s` that folds to `c`.
#[inline(always)]
fn rfind_folded(s: &[u8], c: u8) -> Option<usize> {
    if c.is_ascii_lowercase() { memchr::memrchr2(c, c - 32, s) } else { memchr::memrchr(c, s) }
}

/// `fused_score`'s masks (`name.len() <= 128`, `q.len() <= 16`): `m[i]` is
/// the (lo, hi) pair of position bits where `name` folds to `q[i]`; bits at
/// or past `name.len()` are never set.
#[cfg(all(target_arch = "x86_64", target_feature = "avx512bw"))]
fn fused_masks(name: &[u8], q: &[u8], m: &mut [[u64; 2]; 16]) {
    use core::arch::x86_64::*;
    let n = name.len();
    unsafe {
        let km_lo = if n >= 64 { !0u64 } else { (1u64 << n) - 1 };
        let km_hi = if n >= 128 {
            !0u64
        } else if n > 64 {
            (1u64 << (n - 64)) - 1
        } else {
            0
        };
        let lo = _mm512_maskz_loadu_epi8(km_lo, name.as_ptr() as *const i8);
        let hi = _mm512_maskz_loadu_epi8(km_hi, name.as_ptr().add(64.min(n)) as *const i8);
        for (i, &c) in q.iter().enumerate() {
            let a = _mm512_set1_epi8(c as i8);
            let mut l = _mm512_cmpeq_epi8_mask(lo, a) & km_lo;
            let mut h = _mm512_cmpeq_epi8_mask(hi, a) & km_hi;
            if c.is_ascii_lowercase() {
                let b = _mm512_set1_epi8((c - 32) as i8);
                l |= _mm512_cmpeq_epi8_mask(lo, b) & km_lo;
                h |= _mm512_cmpeq_epi8_mask(hi, b) & km_hi;
            }
            m[i] = [l, h];
        }
    }
}

/// AVX2 `fused_masks`: the name is folded once in four 32-byte vectors, then
/// each query char is a compare + movemask per vector.
#[cfg(all(target_arch = "x86_64", target_feature = "avx2", not(target_feature = "avx512bw")))]
fn fused_masks(name: &[u8], q: &[u8], m: &mut [[u64; 2]; 16]) {
    use core::arch::x86_64::*;
    let n = name.len();
    unsafe {
        let fold = |p: *const u8, j: usize| {
            let v = _mm256_loadu_si256(p.add(32 * j) as *const __m256i);
            let up = _mm256_and_si256(_mm256_cmpgt_epi8(v, _mm256_set1_epi8(b'A' as i8 - 1)), _mm256_cmpgt_epi8(_mm256_set1_epi8(b'Z' as i8 + 1), v));
            _mm256_or_si256(v, _mm256_and_si256(up, _mm256_set1_epi8(0x20)))
        };
        // 128 bytes from the name cannot fault if they stay on its page (an
        // empty slice's pointer may dangle, so it takes the copy).
        let v: [__m256i; 4] = if !name.is_empty() && (name.as_ptr() as usize & 4095) <= 4096 - 128 {
            std::array::from_fn(|j| fold(name.as_ptr(), j))
        } else {
            let mut buf = [0u8; 128];
            buf[..n].copy_from_slice(name);
            std::array::from_fn(|j| fold(buf.as_ptr(), j))
        };
        let km_lo = if n >= 64 { !0u64 } else { (1u64 << n) - 1 };
        let km_hi = if n >= 128 {
            !0u64
        } else if n > 64 {
            (1u64 << (n - 64)) - 1
        } else {
            0
        };
        let eq = |a: __m256i, c: __m256i| _mm256_movemask_epi8(_mm256_cmpeq_epi8(a, c)) as u32 as u64;
        for (i, &c) in q.iter().enumerate() {
            let vc = _mm256_set1_epi8(c as i8);
            m[i] = [(eq(v[0], vc) | eq(v[1], vc) << 32) & km_lo, (eq(v[2], vc) | eq(v[3], vc) << 32) & km_hi];
        }
    }
}

/// NEON `fused_masks`: the name is folded once in eight 16-byte vectors.
#[cfg(target_arch = "aarch64")]
fn fused_masks(name: &[u8], q: &[u8], m: &mut [[u64; 2]; 16]) {
    let n = name.len();
    let f = mask64::Fold::<8>::new(name);
    let km_lo = if n >= 64 { !0u64 } else { (1u64 << n) - 1 };
    let km_hi = if n >= 128 {
        !0u64
    } else if n > 64 {
        (1u64 << (n - 64)) - 1
    } else {
        0
    };
    for (i, &c) in q.iter().enumerate() {
        m[i] = [f.eq_at(c, 0) & km_lo, f.eq_at(c, 1) & km_hi];
    }
}

/// `fuzzy_score_capped` computed on per-byte presence masks: `m[i]`'s bits
/// are the positions where `name` folds to `q[i]` — 64 bytes per u64.
/// Structurally identical to the generic path (same leftmost-end, same
/// shrink-from-right, same greedy scoring walk), just reading bitmasks.
#[cfg(any(target_arch = "aarch64", all(target_arch = "x86_64", target_feature = "avx2")))]
fn fused_score(name: &[u8], q: &[u8], cap: i32) -> Option<i32> {
    let n = name.len();
    let ql = q.len();
    let mut m = [[0u64; 2]; 16];
    fused_masks(name, q, &mut m);
    let first_ge = |i: usize, pos: usize| -> Option<usize> {
        let [lo, hi] = m[i];
        if pos < 64 {
            let b = lo & (!0u64 << pos);
            if b != 0 {
                return Some(b.trailing_zeros() as usize);
            }
            (hi != 0).then(|| 64 + hi.trailing_zeros() as usize)
        } else if pos < 128 {
            let b = hi & (!0u64 << (pos - 64));
            (b != 0).then(|| 64 + b.trailing_zeros() as usize)
        } else {
            None
        }
    };
    let last_lt = |i: usize, pos: usize| -> Option<usize> {
        let [lo, hi] = m[i];
        if pos >= 64 {
            let b = if pos >= 128 { hi } else { hi & ((1u64 << (pos - 64)) - 1) };
            if b != 0 {
                return Some(64 + (63 - b.leading_zeros() as usize));
            }
            (lo != 0).then(|| 63 - lo.leading_zeros() as usize)
        } else {
            let b = lo & ((1u64 << pos) - 1);
            (b != 0).then(|| 63 - b.leading_zeros() as usize)
        }
    };
    let mut end = 0;
    let mut from = 0;
    for i in 0..ql {
        end = first_ge(i, from)?;
        from = end + 1;
    }
    let mut start = end + 1;
    for i in (0..ql).rev() {
        start = last_lt(i, start)?;
    }
    let mut score = 0;
    let (mut at, mut first_bonus) = (start, 0);
    for k in 0..ql {
        let mut run = false;
        if k > 0 {
            let last = at;
            at = first_ge(k, last + 1).filter(|&p| p <= end)?;
            run = at == last + 1;
            if !run {
                score += GAP_START + (at - last - 2) as i32 * GAP_EXT;
            }
        }
        let prev = if at == 0 { Class::Delim } else { class(name[at - 1]) };
        let mut b = bonus(prev, class(name[at]));
        if run {
            if b >= BONUS_BOUNDARY && b > first_bonus {
                first_bonus = b;
            }
            b = b.max(first_bonus).max(BONUS_CONSEC);
        } else {
            first_bonus = b;
        }
        score += SCORE_MATCH + if k == 0 { b * 2 } else { b };
    }
    let off = (n > 1 && name[0] == b'.') as usize;
    let stem = memchr::memrchr(b'.', name).filter(|&p| p > off).unwrap_or(n);
    let contiguous = end + 1 - start == ql;
    let placed = if start == off && contiguous && end + 1 == n {
        100
    } else if start == off && contiguous && end + 1 == stem {
        80
    } else if start == off && contiguous {
        30
    } else {
        0
    };
    Some(score + placed.min(cap) - (n as i32).min(80) / 3)
}

/// Positions where `q` matches `name` contiguously (folded): bit `i` of the
/// (hi, lo) pair is set iff `name[i..i + q.len()]` folds to `q`. Same
/// bounds as `fused_score`; bits >= name.len() can never be set.
#[cfg(all(target_arch = "x86_64", target_feature = "avx512bw"))]
#[inline]
fn contig_starts(name: &[u8], q: &[u8]) -> (u64, u64) {
    use core::arch::x86_64::*;
    let n = name.len();
    unsafe {
        let km_lo = if n >= 64 { !0u64 } else { (1u64 << n) - 1 };
        let km_hi = if n >= 128 {
            !0u64
        } else if n > 64 {
            (1u64 << (n - 64)) - 1
        } else {
            0
        };
        let lo = _mm512_maskz_loadu_epi8(km_lo, name.as_ptr() as *const i8);
        let hi = _mm512_maskz_loadu_epi8(km_hi, name.as_ptr().add(64.min(n)) as *const i8);
        let mut cand_lo = !0u64;
        let mut cand_hi = !0u64;
        for (i, &c) in q.iter().enumerate() {
            let a = _mm512_set1_epi8(c as i8);
            let mut l = _mm512_cmpeq_epi8_mask(lo, a) & km_lo;
            let mut h = _mm512_cmpeq_epi8_mask(hi, a) & km_hi;
            if c.is_ascii_lowercase() {
                let b = _mm512_set1_epi8((c - 32) as i8);
                l |= _mm512_cmpeq_epi8_mask(lo, b) & km_lo;
                h |= _mm512_cmpeq_epi8_mask(hi, b) & km_hi;
            }
            // name[i + k] folds to q[k] <=> bit i of (m[k] >> k): a 128-bit shift.
            let (slo, shi) = if i == 0 { (l, h) } else { ((l >> i) | (h << (64 - i)), h >> i) };
            cand_lo &= slo;
            cand_hi &= shi;
        }
        (cand_lo, cand_hi)
    }
}

/// `fuzzy_score` for a one-byte query matched at `i`: the general scoring
/// loop collapses to one step.
#[inline]
fn single_score(name: &[u8], i: usize, cap: i32) -> i32 {
    let prev = if i == 0 { Class::Delim } else { class(name[i - 1]) };
    let mut score = SCORE_MATCH + bonus(prev, class(name[i])) * 2;
    let off = (name.len() > 1 && name[0] == b'.') as usize;
    if i == off {
        let stem = memchr::memrchr(b'.', name).filter(|&p| p > off).unwrap_or(name.len());
        score += cap.min(if i + 1 == name.len() {
            100
        } else if i + 1 == stem {
            80
        } else {
            30
        });
    }
    score - (name.len() as i32).min(80) / 3
}

/// Best score for `q` read with one typo (see `one_edit_prefix`) at the
/// start of `name` or of a space-separated word in it: scored as if the
/// right letters had been typed, minus TYPO_COST, and never placed above a
/// prefix: "manif" is "manifest" being typed, not a typo of "manic". `m` is
/// as for `token_score`. Other word starts (`_`, `-`, camelCase) would cost
/// a scan of every name per query, ~10x the price.
fn typo_score(name: &[u8], m: u64, q: &[u8]) -> Option<i32> {
    let mut best = typo_at(name, (name.len() > 1 && name[0] == b'.') as usize, q);
    if m & char_bit(b' ') != 0 {
        for sp in memchr::memchr_iter(b' ', name) {
            best = best.max(typo_at(name, sp + 1, q));
        }
    }
    best
}

fn typo_at(name: &[u8], s: usize, q: &[u8]) -> Option<i32> {
    if name.get(s).is_none_or(|&b| fold(b) != q[0]) {
        return None;
    }
    let mut fixed = [0u8; 128];
    let fixed = fixed.get_mut(..one_edit_prefix(&name[s..], q)?)?;
    for (f, &b) in fixed.iter_mut().zip(&name[s..]) {
        *f = fold(b);
    }
    Some(fuzzy_score_capped(name, fixed, 30)? - TYPO_COST)
}

/// How long a prefix of `w` the query `q` spells with exactly one edit (a
/// wrong, extra, missing or swapped letter), if it does. Digits are never
/// edited: "hat_18" is another file than "hat_98", not a typo of it.
fn one_edit_prefix(w: &[u8], q: &[u8]) -> Option<usize> {
    let starts = |w: &[u8], q: &[u8]| w.len() >= q.len() && w.iter().zip(q).all(|(&a, &b)| fold(a) == b);
    // The first difference; none means `q` is a clean prefix, not a typo.
    let i = (0..q.len()).find(|&i| i >= w.len() || fold(w[i]) != q[i])?;
    if q[i].is_ascii_digit() || w.get(i).is_some_and(u8::is_ascii_digit) {
        return None;
    }
    let rest = &q[i + 1..];
    let after = w.get(i + 1..).unwrap_or_default();
    if i + 1 < q.len() && i + 1 < w.len() && fold(w[i]) == q[i + 1] && fold(w[i + 1]) == q[i] && starts(&w[i + 2..], &q[i + 2..]) {
        return Some(q.len());
    }
    if i < w.len() && starts(after, rest) {
        return Some(q.len());
    }
    if starts(&w[i..], rest) {
        return Some(q.len() - 1);
    }
    (i < w.len() && starts(after, &q[i..])).then_some(q.len() + 1)
}

/// Score a token against a name, honoring its mode. `m` is the name's
/// `index::name_mask`, or any superset of it (`!0` when unknown): it only
/// skips work.
#[inline]
fn token_score(name: &[u8], m: u64, t: &Token) -> Option<i32> {
    match t.mode {
        Mode::Fuzzy if takes_typos(&t.text, t.mode) => {
            let clean = if t.mask & !m == 0 { fuzzy_score(name, &t.text) } else { None };
            clean.max(if m & t.start != 0 { typo_score(name, m, &t.text) } else { None })
        }
        Mode::Fuzzy => fuzzy_score(name, &t.text),
        Mode::Exact => {
            #[cfg(all(target_arch = "x86_64", target_feature = "avx512bw"))]
            if t.text.len() <= 16 && name.len() <= 128 {
                let (lo, hi) = contig_starts(name, &t.text);
                let p = if lo != 0 {
                    lo.trailing_zeros() as usize
                } else if hi != 0 {
                    64 + hi.trailing_zeros() as usize
                } else {
                    return None;
                };
                return Some(40 + if p == 0 { 30 } else { 0 } - (name.len() as i32).min(80) / 3);
            }
            find_ci(name, &t.text).map(|p| 40 + if p == 0 { 30 } else { 0 } - (name.len() as i32).min(80) / 3)
        }
        Mode::Prefix => {
            #[cfg(all(target_arch = "x86_64", target_feature = "avx512bw"))]
            if t.text.len() <= 16 && name.len() <= 128 {
                let (lo, _) = contig_starts(name, &t.text);
                return (name.len() >= t.text.len() && lo & 1 != 0).then(|| 60 - (name.len() as i32).min(80) / 3);
            }
            (name.len() >= t.text.len() && name.iter().zip(&t.text).all(|(&a, &b)| fold(a) == b)).then(|| 60 - (name.len() as i32).min(80) / 3)
        }
        Mode::Suffix => {
            #[cfg(all(target_arch = "x86_64", target_feature = "avx512bw"))]
            if t.text.len() <= 16 && name.len() <= 128 && name.len() >= t.text.len() {
                let (lo, hi) = contig_starts(name, &t.text);
                let at = name.len() - t.text.len();
                let hit = if at < 64 { lo >> at & 1 } else { hi >> (at - 64) & 1 };
                return (hit != 0).then(|| 50 - (name.len() as i32).min(80) / 3);
            }
            (name.len() >= t.text.len() && name[name.len() - t.text.len()..].iter().zip(&t.text).all(|(&a, &b)| fold(a) == b))
                .then(|| 50 - (name.len() as i32).min(80) / 3)
        }
    }
}

#[inline]
fn token_matches(name: &[u8], t: &Token) -> bool {
    match t.mode {
        Mode::Fuzzy => is_subseq(name, &t.text),
        _ => token_score(name, !0, t).is_some(),
    }
}

/// Top k by (score, then lower entry index): a total order, so the result
/// does not depend on which thread saw which entry first. Candidates above
/// the floor collect in a buffer that is cut back to k now and then, which
/// is cheaper than a heap when most of the disk matches.
struct TopK {
    k: usize,
    buf: Vec<u64>,
    /// Keys at or below this cannot get in.
    floor: u64,
}

#[inline(always)]
fn key(score: i32, i: u32) -> u64 {
    (((score as i64 - i32::MIN as i64) as u64) << 32) | (!i) as u64
}

impl TopK {
    fn new(k: usize) -> TopK {
        TopK { k, buf: Vec::new(), floor: if k == 0 { u64::MAX } else { 0 } }
    }
    #[inline]
    fn push(&mut self, key: u64) {
        if key > self.floor {
            self.buf.push(key);
            if self.buf.len() >= (2 * self.k).max(64) {
                self.cut();
            }
        }
    }
    /// Keep the k best; the k-th becomes the floor.
    fn cut(&mut self) {
        if self.buf.len() > self.k {
            self.buf.select_nth_unstable_by(self.k - 1, |a, b| b.cmp(a));
            self.buf.truncate(self.k);
            self.floor = self.buf[self.k - 1];
        }
    }
}

/// `f(state, i)` for each `i` in `0..n`, results in order, on the pool: the
/// calling thread starts at once, taking items in order from a shared
/// counter; one helper is queued, and each helper once running queues the
/// next before it takes items too. Threads asleep wake one after another off
/// the caller's path, and a helper that runs after the items are gone
/// returns at once: no one waits for a thread to wake, and no thread sits on
/// a share of the work while it does. `state` is each thread's scratch.
/// (Upstream fsearch's.)
fn par_each<S, T: Send>(n: usize, state: impl Fn() -> S + Sync, f: impl Fn(&mut S, usize) -> T + Sync) -> Vec<T> {
    use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
    let out = Cells::new((0..n).map(|_| None));
    let next = AtomicUsize::new(0);
    let work = || {
        let mut st = state();
        loop {
            let i = next.fetch_add(1, Relaxed);
            if i >= n {
                break;
            }
            // Safety: the counter hands each index to one thread only.
            unsafe { out.set(i, Some(f(&mut st, i))) };
        }
    };
    // Helpers still to queue (each one queues the next).
    let left = AtomicUsize::new((rayon::current_num_threads() - 1).min(n.saturating_sub(1)));
    fn helper<'s>(s: &rayon::Scope<'s>, left: &'s AtomicUsize, next: &'s AtomicUsize, n: usize, work: &'s (dyn Fn() + Sync)) {
        s.spawn(move |s| {
            if next.load(Relaxed) < n {
                if left.fetch_sub(1, Relaxed) > 1 {
                    helper(s, left, next, n, work);
                }
                work();
            }
        });
    }
    rayon::scope(|s| {
        if left.load(Relaxed) > 0 {
            helper(s, &left, &next, n, &work);
        }
        work();
    });
    out.0.into_iter().map(|x| x.into_inner().unwrap()).collect()
}

/// One value per index that threads read and write without locks: each
/// index belongs to one thread at a time (in `par_each`, the one the
/// counter handed it to).
struct Cells<T>(Vec<std::cell::UnsafeCell<T>>);

unsafe impl<T: Send> Sync for Cells<T> {}

impl<T> Cells<T> {
    fn new(v: impl Iterator<Item = T>) -> Cells<T> {
        Cells(v.map(std::cell::UnsafeCell::new).collect())
    }

    /// Safety: no other thread uses index `i` meanwhile.
    unsafe fn set(&self, i: usize, v: T) {
        unsafe { *self.0[i].get() = v };
    }

    /// Safety: no other thread uses index `i` meanwhile.
    unsafe fn take(&self, i: usize) -> T
    where
        T: Default,
    {
        unsafe { std::mem::take(&mut *self.0[i].get()) }
    }
}

/// Up to this many entries to visit in all, `top_k` runs on the calling
/// thread: less time than waking others.
const INLINE_VISITS: usize = 4096;

/// Top `k` over `0..n` items with keys above `floor`: a few contiguous
/// pieces per thread, each with its own heap, then one selection over their
/// survivors (merging heaps pairwise costs more than the scan when `k` is
/// large). `visits` is about how many entries `visit` will look at.
fn top_k(n: usize, k: usize, visits: usize, floor: u64, visit: impl Fn(std::ops::Range<usize>, &mut TopK) + Sync) -> Vec<Hit> {
    let piece = |r: std::ops::Range<usize>| {
        let mut top = TopK::new(k);
        top.floor = top.floor.max(floor);
        visit(r, &mut top);
        top.cut();
        top
    };
    let tops: Vec<TopK> = if visits <= INLINE_VISITS {
        vec![piece(0..n)]
    } else {
        // More pieces than threads: per-piece work varies with region
        // density, and the shared queue hands the leftovers out.
        let pieces = (rayon::current_num_threads() * 8).min(n.max(1));
        let step = n.div_ceil(pieces).max(1);
        par_each(pieces, || (), |_, p| piece((p * step).min(n)..((p + 1) * step).min(n)))
    };
    // A full piece's k-th best already bounds the overall k-th from below.
    let floor = tops.iter().filter(|t| t.buf.len() == k).map(|t| t.floor).max().unwrap_or(0);
    let mut keys: Vec<u64> = tops.into_iter().flat_map(|t| t.buf).filter(|&x| x >= floor).collect();
    if keys.len() > k {
        if k == 0 {
            return Vec::new();
        }
        keys.select_nth_unstable_by(k - 1, |a, b| b.cmp(a));
        keys.truncate(k);
    }
    keys.into_iter().map(|x| Hit { score: ((x >> 32) as i64 + i32::MIN as i64) as i32, idx: !(x as u32), over: None }).collect()
}

/// The score below which an entry key can't beat floor `f`: `key(s, i) <=
/// f` for every `i` once `s < floor_score(f)`.
#[inline(always)]
fn floor_score(f: u64) -> i32 {
    ((f >> 32) as i64 + i32::MIN as i64) as i32
}

/// Bit `j` set where `v[j] < t` (`v` up to 64 long): a word of names
/// tested against one threshold at once.
#[inline(always)]
fn below(v: &[i16], t: i32) -> u64 {
    if t <= i16::MIN as i32 {
        return 0;
    }
    let t = t.min(i16::MAX as i32 + 1);
    #[cfg(target_arch = "aarch64")]
    if v.len() == 64 && t <= i16::MAX as i32 {
        use core::arch::aarch64::*;
        // 8 lanes of i16 per compare, narrowed to bytes, gathered to bits.
        unsafe {
            let tv = vdupq_n_s16(t as i16);
            let w = vld1q_u8([1u8, 2, 4, 8, 16, 32, 64, 128, 1, 2, 4, 8, 16, 32, 64, 128].as_ptr());
            let lt = |j: usize| vmovn_u16(vcltq_s16(vld1q_s16(v.as_ptr().add(8 * j)), tv));
            let q = |j: usize| vandq_u8(vcombine_u8(lt(2 * j), lt(2 * j + 1)), w);
            let s = vpaddq_u8(vpaddq_u8(q(0), q(1)), vpaddq_u8(q(2), q(3)));
            return vgetq_lane_u64(vreinterpretq_u64_u8(vpaddq_u8(s, s)), 0);
        }
    }
    #[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
    if v.len() == 64 && t <= i16::MAX as i32 {
        use core::arch::x86_64::*;
        // 16 lanes of i16 per compare, packed to bytes (packs interleaves
        // the 128-bit halves; the permute restores order), then movemask.
        unsafe {
            let tv = _mm256_set1_epi16(t as i16);
            let lt = |j: usize| _mm256_cmpgt_epi16(tv, _mm256_loadu_si256(v.as_ptr().add(16 * j) as *const __m256i));
            let m = |j: usize| _mm256_movemask_epi8(_mm256_permute4x64_epi64::<0xD8>(_mm256_packs_epi16(lt(2 * j), lt(2 * j + 1)))) as u32 as u64;
            return m(0) | m(1) << 32;
        }
    }
    let mut m = 0u64;
    for (j, &x) in v.iter().enumerate() {
        m |= ((x as i32) < t) as u64 * (1 << j);
    }
    m
}

fn ext_ok(name: &[u8], exts: &[Vec<u8>]) -> bool {
    let Some(dot) = memchr::memrchr(b'.', name) else { return false };
    let e = &name[dot + 1..];
    exts.iter().any(|x| x.len() == e.len() && x.iter().zip(e).all(|(&a, &b)| a == fold(b)))
}

/// Below this many entries carrying a matching name, a query visits just
/// those entries (via the name -> entries list) instead of every entry.
const SELECTIVE: usize = 60_000;
/// A pos token driving a search may fit this many names (rarest first,
/// DRIVEN_TRIES of them at most).
const DRIVEN_NAMES: u64 = 20_000;
const DRIVEN_TRIES: usize = 2;

/// Cheap bounds `ranked` tells apart: BINS of them from -BIN0.
const BINS: usize = 1024;
const BIN0: i32 = 256;
/// A `ranked` run scores FIRST_BATCH names, then four times as many each
/// time up to LAST_BATCH; batches up to INLINE_NAMES run on the caller.
const FIRST_BATCH: usize = 64;
const LAST_BATCH: usize = 4096;
const INLINE_NAMES: usize = 512;
/// Broad one-token searches go `ranked`; below BORDERLINE they do not, in
/// between the bitmaps tell (`rankable`).
const RANKED_MIN: f64 = 20_000.0;
const BORDERLINE: f64 = 1_000.0;
const CHUNK_WORDS: usize = 64;
const INLINE_WORDS: usize = 4096;

/// A name `ranked` scored, and the bound on its entries' scores.
struct Cand {
    bound: i32,
    k: u32,
    score: i16,
    flags: u8,
}

/// The char classes `s` has at least twice.
pub(crate) fn doubled(s: &[u8]) -> u64 {
    let (mut once, mut twice) = (0, 0);
    for &b in s {
        twice |= once & char_bit(b);
        once |= char_bit(b);
    }
    twice
}

/// One token's view of the name bitsets (`Live::char_fit` and on): which
/// of 64 names at a time it can match, before any name is read.
struct TokenBits<'a> {
    /// Each char's class: (first-half, second-half) bitsets.
    seq: Vec<[&'a [u64]; 2]>,
    /// The first char's class, then the token's other classes and the ones
    /// it has twice (as the names with them twice), rarest first.
    classes: Vec<&'a [u64]>,
    /// Typo-taking tokens: names with a word starting with the token's
    /// first letter (or one hashing alike).
    start: Option<&'a [u64]>,
}

impl<'a> TokenBits<'a> {
    fn new(live: &'a Live, t: &Token) -> TokenBits<'a> {
        let nw = live.base.u.div_ceil(64);
        let row = |v: &'a [u64], c: usize| &v[c * nw..(c + 1) * nw];
        let class = |b: u8| char_bit(b).trailing_zeros() as usize;
        let first = class(t.text[0]);
        let twice = doubled(&t.text);
        let mut rest: Vec<(u32, &[u64])> =
            (0..41).filter(|&c| c != first && t.mask >> c & 1 != 0).map(|c| (live.class_counts[c], row(&live.char_fit, c))).collect();
        rest.extend((0..41).filter(|&c| twice >> c & 1 != 0).map(|c| (live.double_counts[c], row(&live.double_fit, c))));
        rest.sort_by_key(|x| x.0);
        TokenBits {
            seq: t.text.iter().map(|&b| [row(&live.first_half, class(b)), row(&live.second_half, class(b))]).collect(),
            classes: std::iter::once(row(&live.char_fit, first)).chain(rest.into_iter().map(|x| x.1)).collect(),
            start: (t.start != 0).then(|| row(&live.start_fit, t.start.trailing_zeros() as usize - 41)),
        }
    }

    /// For the 64 names of word `w`: (can match, a superset of
    /// `Token::fits`; can match cleanly; may match with a typo). Cleanly,
    /// the name has every class of the token (twice where the token has it
    /// twice), and some prefix of the token fits the classes of its first
    /// half and the rest its second half. With a typo, a word starts with
    /// the first letter, at most one other of these is missing, and so does
    /// the halves test with one char after the first left out: every edit
    /// `typo_score` takes leaves the rest of the token in order in the name.
    #[inline]
    fn word(&self, w: usize) -> (u64, u64, u64) {
        let mut all = self.classes[0][w];
        let typo = self.start.map_or(0, |s| s[w] & all);
        // Names with every class so far, and typo-able ones missing one.
        let mut one = 0;
        for b in &self.classes[1..] {
            if all | one == 0 {
                break;
            }
            let x = b[w];
            one = (one & x) | (all & !x & typo);
            all &= x;
        }
        // Names whose first half holds the token so far, and names where
        // the token so far ends in the second half; then the same with one
        // char after the first left out, for typo-able names.
        let (mut first, mut second, mut first1, mut second1) = (all | one, 0, 0, 0);
        for (k, b) in self.seq.iter().enumerate() {
            if first | second | first1 | second1 == 0 {
                break;
            }
            let (b0, b1, skip) = (b[0][w], b[1][w], if k > 0 { typo } else { 0 });
            second1 = ((second1 | first1) & b1) | (second & skip);
            first1 = (first1 & b0) | (first & skip);
            second = (second | first) & b1;
            first &= b0;
        }
        let clean = (first | second) & all;
        let typo = (first | second | first1 | second1) & typo;
        (clean | typo, clean, typo)
    }
}

/// `token_score` for a name its `TokenBits::word` says can match (`clean`,
/// `typo`: its bits there): the same score, skipping what is sure to fail.
#[inline]
fn token_score_known(name: &[u8], m: u64, t: &Token, clean: bool, typo: bool) -> Option<i32> {
    if t.start == 0 {
        return if clean { token_score(name, m, t) } else { None };
    }
    let c = if clean { fuzzy_score(name, &t.text) } else { None };
    c.max(if typo { typo_score(name, m, &t.text) } else { None })
}
/// Queries answered by `driven` instead of a full name pass (test tripwire).
pub(crate) static DRIVEN_QUERIES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
/// Name ids per parallel chunk when scoring names.
const OVERLAY_CHUNK: usize = 256;
const NAME_CHUNK: usize = 1 << 15;

pub struct Searcher<'a> {
    pub live: &'a Live,
}

impl Searcher<'_> {
    /// Entry range to scan, from the `in:` scope.
    pub fn scope_range(&self, q: &Query) -> Option<(usize, usize)> {
        let idx = &self.live.base;
        let Some(scope) = &q.scope else { return Some((1, idx.n)) };
        let e = idx.lookup(scope)?;
        let d = idx.dir_of(e)? as usize;
        Some((idx.dir_start()[d] as usize, idx.dir_end()[d] as usize))
    }

    pub fn search(&self, q: &Query) -> Vec<Hit> {
        let mut hits = self.search_base(q);
        hits.extend(self.search_overlay(q));
        hits.sort_by(|a, b| b.score.cmp(&a.score).then(a.idx.cmp(&b.idx)));
        hits.truncate(q.limit);
        hits
    }

    fn search_base(&self, q: &Query) -> Vec<Hit> {
        let Some((lo, hi)) = self.scope_range(q) else { return Vec::new() };
        let pos: Vec<&Token> = q.tokens.iter().filter(|t| !t.negate).collect();
        let neg: Vec<&Token> = q.tokens.iter().filter(|t| t.negate).collect();
        if (pos.len() > 1 || !neg.is_empty())
            && let Some(hits) = self.driven(q, &pos, &neg, lo, hi)
        {
            return hits;
        }
        if let Some((t, tentative)) = self.rankable(q) {
            if let Some(hits) = self.ranked(q, t, lo, hi, tentative) {
                return hits;
            }
        }
        // No positive tokens: an entry's score is exactly its static score,
        // so the top-k is the first `limit` survivors down the stat order.
        // With no tokens at all even the name table goes unread.
        if pos.is_empty() {
            let empty = neg.is_empty() && q.exts.is_empty() && q.name_re.is_none();
            let idx = &self.live.base;
            if !empty && !q.exts.is_empty() {
                // An ext filter names only the names carrying it: score
                // just those (plus every negatable name, for dir memos).
                // Few enough ok entries visit them via the names->entries
                // lists instead of stat order.
                let scored = self.names(q, &pos, &neg);
                let s = Scan {
                    q,
                    live: self.live,
                    names: &scored.names,
                    npos: 0,
                    qbound: 0,
                    need_dirs: !neg.is_empty(),
                    dead: self.live.dead_count != 0,
                };
                return if scored.names.ok_entries <= SELECTIVE {
                    s.selective(lo, hi)
                } else {
                    s.walk(lo, hi, &|id| scored.names.get(id).is_some_and(|h| h.flags & NF_OK != 0), &|id| {
                        scored.names.get(id).is_some_and(|h| h.flags & NF_NEG != 0)
                    })
                };
            }
            // No positive tokens: entry scores are all static, so the
            // stat-ordered walk stops after `limit` survivors. Score just
            // the names the negations can hit (mask-covered tokens are
            // resolved by their fit-set alone) plus any regex, into one
            // ok-bitset the walk probes at ~3ns a candidate.
            let nw = idx.u.div_ceil(64);
            let mut ok = Vec::new();
            let mut negbits = Vec::new();
            if !empty {
                let key = NameKey::of(q);
                let hit = {
                    let g = self.live.posneg_cache.0.lock().unwrap();
                    g.as_ref().filter(|(k, _, _)| *k == key).map(|(_, o, n)| (o.clone(), n.clone()))
                };
                if let Some((o, n)) = hit {
                    (ok, negbits) = (o, n);
                } else {
                    ok = vec![!0u64; nw];
                    if idx.u % 64 != 0 {
                        ok[nw - 1] &= (1u64 << (idx.u % 64)) - 1;
                    }
                    if !neg.is_empty() {
                        negbits = vec![0u64; nw];
                        for &t in neg.iter().filter(|t| t.mask_covers()) {
                            self.union_fits(t, &mut negbits);
                        }
                        let eval: Vec<&Token> = neg.iter().filter(|t| !t.mask_covers()).copied().collect();
                        if !eval.is_empty() {
                            let mut need = vec![0u64; nw];
                            for &t in &eval {
                                self.union_fits(t, &mut need);
                            }
                            let mask = idx.name_mask();
                            negbits.par_chunks_mut(2048).enumerate().for_each(|(c, w)| {
                                for (j, wj) in w.iter_mut().enumerate() {
                                    let mut v = need[c * 2048 + j] & !*wj;
                                    while v != 0 {
                                        let k = (c * 2048 + j) * 64 + v.trailing_zeros() as usize;
                                        v &= v - 1;
                                        let m = mask[k];
                                        let name = idx.uname(k as u32);
                                        if eval.iter().any(|t| t.fits(m) && token_matches(name, t)) {
                                            *wj |= 1 << (k & 63);
                                        }
                                    }
                                }
                            });
                        }
                        ok.par_iter_mut().zip(&negbits).for_each(|(w, &n)| *w &= !n);
                    }
                    if let Some(re) = &q.name_re {
                        ok.par_chunks_mut(2048).enumerate().for_each(|(c, w)| {
                            for (j, wj) in w.iter_mut().enumerate() {
                                let mut v = *wj;
                                while v != 0 {
                                    let b = v.trailing_zeros();
                                    v &= v - 1;
                                    let k = (c * 2048 + j) * 64 + b as usize;
                                    if !re.is_match(idx.uname(k as u32)) {
                                        *wj &= !(1 << (k & 63));
                                    }
                                }
                            }
                        });
                    }
                    *self.live.posneg_cache.0.lock().unwrap() = Some((key, ok.clone(), negbits.clone()));
                }
            }
            let s = Scan {
                q,
                live: self.live,
                names: EMPTY_TABLE.get_or_init(NameTable::empty),
                npos: 0,
                qbound: 0,
                need_dirs: !neg.is_empty(),
                dead: self.live.dead_count != 0,
            };
            return s.walk(lo, hi, &|id| ok.is_empty() || ok[id as usize >> 6] >> (id & 63) & 1 != 0, &|id| {
                negbits[id as usize >> 6] >> (id & 63) & 1 != 0
            });
        }
        // Step 1: every name-only predicate, once per distinct name (~2M)
        // rather than once per entry (~7.5M); reused while you type.
        let __tn = std::time::Instant::now();
        let scored = self.names(q, &pos, &neg);
        PROF_NAMES_NS.fetch_add(__tn.elapsed().as_nanos() as u64, std::sync::atomic::Ordering::Relaxed);
        let s = Scan {
            q,
            live: self.live,
            names: &scored.names,
            npos: pos.len(),
            // One token: a hit's own name always carries it, so the gain is
            // exactly the best name score — no folder contribution exists.
            qbound: if pos.len() == 1 { scored.names.hi_score as i32 } else { scored.names.bound(pos.len()) },
            need_dirs: pos.len() > 1 || !neg.is_empty(),
            dead: self.live.dead_count != 0,
        };
        // Step 2: score entries. Few candidates: just the entries carrying a
        // matching name. One token: walk the static order — an entry's score
        // is nh + stat, so the top is decided long before the tail. Many:
        // one sequential pass over every entry.
        let __ts = std::time::Instant::now();
        let r = if scored.names.ok_entries <= SELECTIVE && !FULL_PASS.load(std::sync::atomic::Ordering::Relaxed) {
            s.selective(lo, hi)
        } else if let Some((ranges, t)) = scored.cover.as_ref().and_then(|(c, t)| Some((&c.names.as_ref()?.2, *t))) {
            s.covered(lo, hi, ranges, t)
        } else if pos.len() == 1 {
            // One token: a hit's name carries it, so negations only drop
            // entries (memo bits MAX) and the merge's bounds still hold.
            s.byname(lo, hi)
        } else {
            let __tm = std::time::Instant::now();
            let memo = if s.need_dirs { Some(scored.memo.get_or_init(|| self.dir_tokens(&scored.names))) } else { None };
            PROF_MEMO_NS.fetch_add(__tm.elapsed().as_nanos() as u64, std::sync::atomic::Ordering::Relaxed);
            s.full(lo, hi, memo.map(|m| m.as_slice()))
        };
        PROF_SCAN_NS.fetch_add(__ts.elapsed().as_nanos() as u64, std::sync::atomic::Ordering::Relaxed);
        r
    }

    /// Several positive tokens: a hit has each in its own name or in a
    /// folder above it, so the names the rarest token matches and the
    /// subtrees of folders so named hold every hit. When those are few they
    /// are all that gets scored — not all two million names. None if the
    /// rarest fits are still many.
    /// The query's token if `ranked` can answer it: one fuzzy token (and
    /// any negated ones), no filter on the entries themselves, and broad
    /// enough to be worth it. A bitmap sample decides: few names and a name
    /// table costs less; between BORDERLINE and RANKED_MIN the run is
    /// tentative (true) and gives up early.
    fn rankable<'q>(&self, q: &'q Query) -> Option<(&'q Token, bool)> {
        let mut pos = q.tokens.iter().filter(|t| !t.negate);
        let (Some(t), None) = (pos.next(), pos.next()) else { return None };
        if t.mode != Mode::Fuzzy
            || q.scope.is_some()
            || q.kind.is_some()
            || q.size != (0, u64::MAX)
            || q.mtime != (0, u32::MAX)
            || q.path_re.is_some()
            || q.limit == 0
        {
            return None;
        }
        // Typing on from a table (the last query was narrow): narrowing it
        // is cheaper.
        let key = NameKey::of(q);
        if self.live.names_cache.0.lock().unwrap().as_ref().is_some_and(|p| key.narrows(&p.key)) {
            return None;
        }
        let (counts, u) = (&self.live.class_counts[..], self.live.base.u as f64);
        let est = (0..41).filter(|&c| t.mask & (1 << c) != 0).fold(u, |e, c| e * counts[c] as f64 / u);
        if est >= RANKED_MIN {
            return Some((t, false));
        }
        (est >= BORDERLINE && t.text.iter().all(u8::is_ascii_alphabetic)).then_some((t, true))
    }

    /// A broad one-token search ("a", "de"): its top `limit` comes from a
    /// few dozen of the million names it matches. Names are scored in
    /// batches, in falling order of a bound on their entries' scores read
    /// without the name (`index::NameInfo`): 24 per char + 8, the placement
    /// bonus its length and stem leave possible if it starts with the
    /// token's first byte, the best location prior among its folders and
    /// the best rank tweak. Each batch's entries are visited until no name
    /// left can beat the limit-th entry found. Names that cannot start
    /// with the token come from a list of the highest keys, or, if the
    /// floor ends up below it, from a pass over them all.
    /// None if fewer than `limit` hits turn up among the few thousand
    /// names with the best bounds: then a name table, which typing can
    /// narrow from, costs about the same.
    fn ranked(&self, q: &Query, t: &Token, lo: usize, hi: usize, tentative: bool) -> Option<Vec<Hit>> {
        let idx = &self.live.base;
        let (prior, info, ne_off, ne) = (idx.name_prior(), idx.name_info(), idx.name_ents_off(), idx.name_ents());
        let tb = TokenBits::new(self.live, t);
        let tweak = |flags: u8| 10 + if flags & NF_APP != 0 { 25 } else { 0 } - if flags & NF_DOT != 0 { 8 } else { 0 };
        let neg: Vec<&Token> = q.tokens.iter().filter(|t| t.negate).collect();
        let mask = idx.name_mask();
        // Name `k`, given its word's `TokenBits::word`: its hit and the
        // bound on its entries.
        let score = |k: u32, (_, clean, typo): (u64, u64, u64), re: Option<&regex::bytes::Regex>| -> Option<Cand> {
            let (name, i) = (idx.uname(k), k % 64);
            let s = token_score_known(name, mask[k as usize], t, clean >> i & 1 != 0, typo >> i & 1 != 0)?;
            if !(q.exts.is_empty() || ext_ok(name, &q.exts)) || re.is_some_and(|re| !re.is_match(name)) || neg.iter().any(|t| token_matches(name, t))
            {
                return None;
            }
            let score = s.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
            let flags = name_flags(name) | NF_OK;
            Some(Cand { bound: score as i32 + prior[k as usize] as i32 + tweak(flags), k, score, flags })
        };
        // The cheap bound: 24 per char + 8 (16 less if the token starts
        // with a delimiter, which takes no boundary bonus), plus the
        // placement bonus its length and stem leave possible if the name
        // starts with the token's first byte, plus its key.
        let (m, t0) = (t.text.len() as i32, fold(t.text[0]));
        let base = 24 * m + 8 - if class(t0) == Class::Delim { 16 } else { 0 };
        let cheap = |x: crate::index::NameInfo| {
            base + x.key as i32
                + match x.head == t0 {
                    false => 0,
                    true if x.len as i32 == m => 100,
                    true if x.stem as i32 == m => 80,
                    true => 30,
                }
        };
        let scan = Scan {
            q,
            live: self.live,
            names: EMPTY_TABLE.get_or_init(NameTable::empty),
            npos: 1,
            need_dirs: false,
            dead: self.live.dead_count != 0,
            qbound: 0,
        };
        let entries = |k: u32| &ne[ne_off[k as usize] as usize..ne_off[k as usize + 1] as usize];
        let (parent, de, dp, en) = (idx.parent(), idx.dir_entry(), idx.dir_parent(), idx.ent_name());
        // Whether a folder on the path to dir `d` has a negated token's
        // name.
        let negated = |d: u32, cache: &mut HashMap<u32, bool, crate::index::Fx>| {
            let mut chain = Vec::new();
            let mut k = d;
            let mut out = loop {
                if k == 0 {
                    break false;
                }
                if let Some(&b) = cache.get(&k) {
                    break b;
                }
                chain.push(k);
                k = dp[k as usize];
            };
            for &k in chain.iter().rev() {
                out = out || neg.iter().any(|t| token_matches(idx.uname(en[de[k as usize] as usize]), t));
                cache.insert(k, out);
            }
            out
        };
        // The entries of `names` that beat `floor` (a `key`). A name's
        // entries come best stat first (`Live::o_ne`): its run stops at the
        // first that can't, and `limit` of them at most count as work.
        let (o_ne, stat) = (&self.live.o_ne, &self.live.stat);
        let visit = |names: &[Cand], floor: u64| {
            let visits = names.iter().map(|c| entries(c.k).len().min(q.limit)).sum();
            top_k(names.len(), q.limit, visits, floor, |r, top| {
                let (mut pbuf, mut cache) = (Vec::new(), HashMap::default());
                for c in &names[r] {
                    let nh = NameHit { score: c.score, bits: 1, flags: c.flags, best: [0; 4] };
                    for &e in &o_ne[ne_off[c.k as usize] as usize..ne_off[c.k as usize + 1] as usize] {
                        let st = stat[e as usize];
                        if key(c.score as i32 + st, 0) <= top.floor {
                            break;
                        }
                        if !(lo..hi).contains(&(e as usize)) || !neg.is_empty() && negated(parent[e as usize], &mut cache) {
                            continue;
                        }
                        if let Some(key) = scan.score(e as usize, st, nh, DirMemo::default(), top.floor, &mut pbuf) {
                            top.push(key);
                        }
                    }
                }
            })
        };
        // The best `limit` hits so far, the limit-th one's score (i32::MIN
        // if fewer) and key (0 if fewer).
        let best = |mut hits: Vec<Hit>| {
            hits.sort_by(|a, b| b.score.cmp(&a.score).then(a.idx.cmp(&b.idx)));
            hits.truncate(q.limit);
            let last = hits.get(q.limit - 1).map(|h| (h.score, key(h.score, h.idx)));
            let (floor, fkey) = last.unwrap_or((i32::MIN, 0));
            (hits, floor, fkey)
        };
        // The names that can start with the token (the only ones that can
        // get a placement bonus), best cheap bound first, with how many
        // fall in each bin.
        let bin = |b: i32| (b + BIN0).clamp(0, BINS as i32 - 1) as usize;
        let sorted = |got: Vec<(i32, u32)>, h: Vec<u32>| {
            let mut at = vec![0u32; BINS];
            let mut acc = 0;
            for b in (0..BINS).rev() {
                at[b] = acc;
                acc += h[b];
            }
            let mut sorted = vec![(0, 0); got.len()];
            for x in got {
                let b = bin(x.0);
                sorted[at[b] as usize] = x;
                at[b] += 1;
            }
            (sorted, h)
        };
        let by_key = idx.by_key();
        // How far a run of batches got: the best hits so far, the limit-th
        // one's score and key, the bin from which on every name is scored,
        // the next batch's size and how much of `by_key.list` is done.
        struct Run {
            hits: Vec<Hit>,
            floor: i32,
            fkey: u64,
            top: usize,
            want: usize,
            listed: usize,
        }
        // Names with their cheap bounds, best first, and how many per bin.
        type Part = (Vec<(i32, u32)>, Vec<u32>);
        // Score them best bin first, a batch at a time, until no name left
        // can beat the limit-th entry found. With `complete` false `parts`
        // holds only some of them, the rest bounded by `cover + 29` over
        // base: unless that settles the search, how far it got.
        let run = |parts: Vec<Part>, complete: bool, r: Run| -> Result<Option<Vec<Hit>>, Run> {
            let Run { mut hits, mut floor, mut fkey, mut top, mut want, mut listed } = r;
            let mut hist = vec![0usize; BINS];
            for (_, h) in &parts {
                hist.iter_mut().zip(h).for_each(|(a, &b)| *a += b as usize);
            }
            // Names that cannot start with the token get no placement
            // bonus: base + key bounds them. The highest keys come in
            // order, the rest only matter if the floor ends up below them.
            for (key, &n) in (by_key.cover..=i8::MAX as i32).zip(&by_key.counts) {
                hist[bin(base + key)] += n as usize;
            }
            let mut taken: Vec<usize> = parts.iter().map(|(items, _)| items.iter().take_while(|x| bin(x.0) >= top).count()).collect();
            let done = |floor: i32, top: usize| floor != i32::MIN && (top == 0 || floor > top as i32 - 1 - BIN0);
            // Without all of them, names bounded by `cover + 29` over base
            // cannot settle it.
            let least = if complete { 0 } else { bin(base + by_key.cover + 30) };
            while want <= LAST_BATCH && !done(floor, top) && top > least {
                let low = if floor == i32::MIN { 0 } else { bin(floor) }.max(least);
                let (mut cut, mut n) = (top, 0);
                while cut > low && n < want {
                    cut -= 1;
                    n += hist[cut];
                }
                let ends: Vec<usize> =
                    parts.iter().zip(&taken).map(|((items, _), &a)| a + items[a..].iter().take_while(|x| bin(x.0) >= cut).count()).collect();
                let lend = listed + by_key.list[listed..].iter().take_while(|x| bin(base + x.0 as i32) >= cut).count();
                let one = |p: usize| {
                    let re = q.name_re.clone();
                    parts[p].0[taken[p]..ends[p]]
                        .iter()
                        .filter_map(|&(_, k)| score(k, tb.word(k as usize / 64), re.as_ref()).filter(|c| c.bound >= floor))
                        .collect::<Vec<_>>()
                };
                let mut batch: Vec<Cand> = if n <= INLINE_NAMES {
                    (0..parts.len()).flat_map(one).collect()
                } else {
                    par_each(parts.len(), || (), |_, p| one(p)).into_iter().flatten().collect()
                };
                let re = q.name_re.clone();
                batch.extend(by_key.list[listed..lend].iter().filter_map(|&(_, k)| {
                    let (w, i) = (k as usize / 64, k % 64);
                    if info[k as usize].head == t0 {
                        return None;
                    }
                    let x = tb.word(w);
                    (x.0 >> i & 1 != 0).then(|| score(k, x, re.as_ref())).flatten().filter(|c| c.bound >= floor)
                }));
                (taken, listed) = (ends, lend);
                hits = best(hits.into_iter().chain(visit(&batch, fkey)).collect()).0;
                floor = hits.get(q.limit - 1).map_or(i32::MIN, |h| h.score);
                fkey = hits.get(q.limit - 1).map_or(0, |h| key(h.score, h.idx));
                top = cut;
                want *= 4;
                // A tentative try that one batch does not settle has few
                // good matches: a name table costs no more and typing can
                // narrow it. So does any try whose first two batches turn
                // up a handful of hits.
                if tentative && !(done(floor, top) && floor >= base + by_key.cover) || hits.len() < q.limit / 4 && want > FIRST_BATCH * 4 {
                    if !complete {
                        break;
                    }
                    return Ok(None);
                }
            }
            // Then, if the floor is low, every other name that can still
            // beat it, in one pass.
            if done(floor, top) && floor >= base + by_key.cover && (complete || floor > base + by_key.cover + 29) {
                return Ok(Some(hits));
            }
            if !complete {
                return Err(Run { hits, floor, fkey, top, want, listed });
            }
            if floor == i32::MIN {
                return Ok(None);
            }
            let scored = |k: usize| {
                let x = info[k];
                let placed = x.head == t0;
                bin(cheap(x)) >= top && (placed || x.key as i32 >= by_key.cover)
            };
            let rest = par_each(
                idx.u.div_ceil(64 * CHUNK_WORDS),
                || q.name_re.clone(),
                |re, c| {
                    let mut out = Vec::new();
                    for w in c * CHUNK_WORDS..((c + 1) * CHUNK_WORDS).min(idx.u.div_ceil(64)) {
                        let x = tb.word(w);
                        let mut m = x.0;
                        while m != 0 {
                            let k = w * 64 + m.trailing_zeros() as usize;
                            m &= m - 1;
                            if cheap(info[k]) >= floor
                                && !scored(k)
                                && let Some(c) = score(k as u32, x, re.as_ref()).filter(|c| c.bound >= floor)
                            {
                                out.push(c);
                            }
                        }
                    }
                    out
                },
            )
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
            Ok(Some(best(hits.into_iter().chain(visit(&rest, fkey)).collect()).0))
        };
        let mut from = Run { hits: Vec::new(), floor: i32::MIN, fkey: 0, top: BINS, want: FIRST_BATCH, listed: 0 };
        // A short token first tries the names starting with it that it can
        // find without a pass over every word: those with the highest keys,
        // and those it can match whole (`index::ByKey`). Any other such
        // name has no bonus above 30 and a key below `cover`. If that does
        // not settle it, the pass goes on from there.
        if t.text.len() <= crate::index::SHORT {
            let m = t.text.len();
            let whole = |k: &u32| {
                let x = info[*k as usize];
                (x.len as usize == m || x.stem as usize == m) && (x.key as i32) < by_key.cover
            };
            let (mut got, mut h) = (Vec::new(), vec![0u32; BINS]);
            for &k in by_key.heads[t0 as usize].iter().chain(by_key.short[t0 as usize].iter().filter(|k| whole(k))) {
                if tb.word(k as usize / 64).0 >> (k % 64) & 1 != 0 {
                    let c = cheap(info[k as usize]);
                    h[bin(c)] += 1;
                    got.push((c, k));
                }
            }
            match run(vec![sorted(got, h)], false, from) {
                Ok(found) => return found,
                Err(r) => from = r,
            }
        }
        // Otherwise all of them, from a pass over the words in pieces.
        let nw = idx.u.div_ceil(64);
        let pieces = if nw <= INLINE_WORDS { 1 } else { rayon::current_num_threads() * 2 };
        let step = nw.div_ceil(pieces);
        let sf = &self.live.start_fit[(t0 as usize % 23) * nw..];
        let piece = |p: usize| {
            let (mut got, mut h) = (Vec::new(), vec![0u32; BINS]);
            for w in (p * step)..((p + 1) * step).min(nw) {
                let mut x = sf[w];
                if x == 0 {
                    continue;
                }
                x &= tb.word(w).0;
                while x != 0 {
                    let k = w * 64 + x.trailing_zeros() as usize;
                    x &= x - 1;
                    if info[k].head == t0 {
                        let c = cheap(info[k]);
                        h[bin(c)] += 1;
                        got.push((c, k as u32));
                    }
                }
            }
            sorted(got, h)
        };
        let parts = if pieces == 1 { vec![piece(0)] } else { par_each(pieces, || (), |_, p| piece(p)) };
        run(parts, true, from).ok().flatten()
    }

    fn driven(&self, q: &Query, pos: &[&Token], neg: &[&Token], lo: usize, hi: usize) -> Option<Vec<Hit>> {
        // A query that only narrows the last one is a keystroke: the cover
        // path amortizes across those, so let `names()` handle it.
        let key = NameKey::of(q);
        if self.live.names_cache.0.lock().unwrap().as_ref().is_some_and(|p| key.narrows(&p.key)) {
            return None;
        }
        // An earlier token's cover, already paid for, beats re-driving.
        {
            let cache = self.live.cover_cache.0.lock().unwrap();
            if pos[..pos.len().saturating_sub(1)].iter().any(|t| cache.iter().any(|c| c.names.is_some() && c.text == t.text && c.mode == t.mode)) {
                return None;
            }
        }
        let nw = self.live.base.u.div_ceil(64);
        let (counts, u) = (&self.live.class_counts[..], self.live.base.u as f64);
        let est = |t: &Token| (0..41).filter(|&c| t.mask >> c & 1 != 0).fold(u, |e, c| e * counts[c] as f64 / u);
        let mut order: Vec<usize> = (0..pos.len()).filter(|&t| est(pos[t]) <= DRIVEN_NAMES as f64).collect();
        order.sort_by(|&a, &b| est(pos[a]).total_cmp(&est(pos[b])));
        for &t in order.iter().take(DRIVEN_TRIES) {
            if let Some(h) = self.drive(q, pos, neg, lo, hi, t, nw) {
                return Some(h);
            }
        }
        None
    }

    /// `driven` from token `t`'s names; None if it gathers more than
    /// SELECTIVE candidates.
    fn drive(&self, q: &Query, pos: &[&Token], neg: &[&Token], lo: usize, hi: usize, t: usize, nw: usize) -> Option<Vec<Hit>> {
        let idx = &self.live.base;
        let (ne_off, ne, kind, en, parent) = (idx.name_ents_off(), idx.name_ents(), idx.kind(), idx.ent_name(), idx.parent());
        let mask = idx.name_mask();
        let mut fits = vec![0u64; nw];
        self.union_fits(pos[t], &mut fits);
        // The names' entries, and the subtrees of those that are folders.
        let (mut ents, mut trees, mut n) = (Vec::new(), Vec::new(), 0usize);
        // The names that match, scored on every thread, in id order.
        let matched = par_each(nw.div_ceil(CHUNK_WORDS), || (), |_, c| {
            let mut out = Vec::new();
            for w in c * CHUNK_WORDS..((c + 1) * CHUNK_WORDS).min(nw) {
                let mut word = fits[w];
                while word != 0 {
                    let k = w * 64 + word.trailing_zeros() as usize;
                    word &= word - 1;
                    if token_score(idx.uname(k as u32), mask[k], pos[t]).is_some() {
                        out.push(k);
                    }
                }
            }
            out
        });
        'names: for k in matched.into_iter().flatten() {
            for &e in &ne[ne_off[k] as usize..ne_off[k + 1] as usize] {
                if n > SELECTIVE {
                    break 'names;
                }
                let e = e as usize;
                if (lo..hi).contains(&e) {
                    ents.push(e as u32);
                    n += 1;
                }
                if kind[e] & 3 == KIND_DIR
                    && let Some(d) = idx.dir_of(e as u32)
                {
                    let (a, b) = (idx.dir_start()[d as usize].max(lo as u32), idx.dir_end()[d as usize].min(hi as u32));
                    if a < b {
                        trees.push((a, b));
                        n += (b - a) as usize;
                    }
                }
            }
        }
        if n > SELECTIVE {
            return None;
        }
        trees.sort_unstable();
        let mut merged: Vec<(u32, u32)> = Vec::new();
        for (a, b) in trees {
            match merged.last_mut() {
                Some(l) if a <= l.1 => l.1 = l.1.max(b),
                _ => merged.push((a, b)),
            }
        }
        // In those subtrees, entries whose name has the token are in `ents`.
        let first = ents.len();
        if first + merged.iter().map(|&(a, b)| (b - a) as usize).sum::<usize>() > SELECTIVE {
            return None;
        }
        ents.extend(merged.iter().flat_map(|&(a, b)| a..b));
        // Score the names of those entries and of the folders above them —
        // the dir memos need them — rather than every name in the index.
        let (de, dp) = (idx.dir_entry(), idx.dir_parent());
        let (mut bits, mut seen_dir) = (vec![0u64; nw], vec![0u64; idx.d.div_ceil(64)]);
        for &e in &ents {
            let e = e as usize;
            let k = en[e];
            bits[k as usize >> 6] |= 1 << (k & 63);
            let mut d = parent[e];
            while d != 0 && seen_dir[d as usize >> 6] >> (d & 63) & 1 == 0 {
                seen_dir[d as usize >> 6] |= 1 << (d & 63);
                let k = en[de[d as usize] as usize];
                bits[k as usize >> 6] |= 1 << (k & 63);
                d = dp[d as usize];
            }
        }
        DRIVEN_QUERIES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let names = self.score_names(q, pos, neg, Some(&bits), None, &[]);
        let s = Scan {
            q,
            live: self.live,
            names: &names,
            npos: pos.len(),
            qbound: 0,
            need_dirs: true,
            dead: self.live.dead_count != 0,
        };
        Some(top_k(ents.len(), q.limit, ents.len(), 0, |r, top| {
            let (mut pbuf, mut memo) = (Vec::new(), HashMap::<u32, DirMemo, crate::index::Fx>::default());
            for j in r {
                let i = ents[j] as usize;
                // Subtree members whose own name carries `t` are already in
                // `ents` proper; visiting them twice would double-count.
                let Some(nh) = names.get(en[i]).filter(|h| h.flags & NF_OK != 0 && (j < first || h.bits >> t & 1 == 0)) else {
                    continue;
                };
                let m = s.memo_of(parent[i], &mut memo, &|id| names.get(id));
                if let Some(k) = s.score(i, self.live.stat[i], nh, m, top.floor, &mut pbuf) {
                    top.push(k);
                }
            }
        }))
    }

    /// The name table for this query: cached for a repeat of the last one
    /// (the second, longer page of results), narrowed from the last one when
    /// this query only extends it (typing), else scored from scratch.
    fn names(&self, q: &Query, pos: &[&Token], neg: &[&Token]) -> std::sync::Arc<Scored> {
        let key = NameKey::of(q);
        let prev = self.live.names_cache.0.lock().unwrap().clone();
        if let Some(p) = &prev {
            if p.key == key && p.names.fast_limit.is_none_or(|limit| q.limit <= limit && self.prunable(q, pos, neg)) {
                return p.clone();
            }
        }
        // Scan-set for the rebuild. A name can only enter the table when
        // some current token's char mask fits it, and a token unchanged
        // from the last query contributes only names that table already
        // holds — so the candidates are `prev.bits` plus the union of the
        // changed tokens' fit-sets, and everything else can be skipped.
        let narrowed = prev.as_ref().is_some_and(|p| key.narrows(&p.key));
        // Carry-over on multi-token queries: the last table's hits still
        // answer every shared token. A name it matched on token `t` carries
        // that score (from `best[t]`), or — when `t` merely narrowed —
        // needs a rescore; a name it did not match on `t` can never match
        // the narrower `t`, and a name outside the table matches none of
        // the shared tokens.
        let carry = prev.as_ref().and_then(|p| {
            if pos.len() <= 1 {
                return None;
            }
            let (pp, pn): (Vec<_>, Vec<_>) = p.key.tokens.iter().partition(|t| !t.2);
            let cn: Vec<_> = key.tokens.iter().filter(|t| t.2).collect();
            if pp.is_empty() {
                return None;
            }
            Some(PrevCtx { tab: &p.names, ptoks: pp, narrow: narrowed, neg_same: pn == cn })
        });
        let mut cand: Vec<u64>;
        let prevb_hold: std::borrow::Cow<[u64]>;
        let scan: Option<&[u64]> = if pos.is_empty() {
            // Only names carrying a query extension can pass, and only a
            // name fitting a negation can be negated (a negated dir name
            // kills its subtree even if it can't match the ext filter).
            let nw = self.live.base.u.div_ceil(64);
            cand = vec![0; nw];
            for x in &q.exts {
                if let Some(list) = self.live.ext_names.get(x.as_slice()) {
                    for &id in list {
                        cand[id as usize >> 6] |= 1 << (id & 63);
                    }
                }
            }
            for &t in neg {
                self.union_fits(t, &mut cand);
            }
            Some(&cand)
        } else if let Some(p) = prev.as_ref().filter(|_| narrowed) {
            // A kept name still fits some current token, so the rescan only
            // needs `prev.bits` kept within the union of the tokens'
            // fit-sets (`TokenBits`, so rescoring stays exact) — computed
            // only for words the last table used, when there are few of
            // them. A table whose maybe-bits cover most of the index rescans
            // them as is: the pass below would just rebuild the same words.
            prevb_hold = p.names.maybe();
            let nw = prevb_hold.len();
            let nz = prevb_hold.iter().filter(|&&w| w != 0).count();
            if nz * 4 <= nw {
                let tbs: Vec<TokenBits> = q.tokens.iter().map(|t| TokenBits::new(self.live, t)).collect();
                cand = vec![0; nw];
                for (w, &pv) in prevb_hold.iter().enumerate() {
                    if pv != 0 {
                        cand[w] = pv & tbs.iter().fold(0, |u, tb| u | tb.word(w).0);
                    }
                }
                Some(&cand)
            } else {
                Some(&prevb_hold[..])
            }
        } else if carry.is_none() {
            // Nothing carries over from the last table (a first keystroke,
            // or a query that doesn't narrow it): the full pass's own mask
            // early-out beats a fit-set prepass, and it runs on every thread.
            None
        } else {
            cand = prev.as_ref().map(|p| p.names.maybe().into_owned()).unwrap_or_else(|| vec![0; self.live.base.u.div_ceil(64)]);
            for (i, t) in q.tokens.iter().enumerate() {
                let same =
                    prev.as_ref().and_then(|p| p.key.tokens.get(i)).is_some_and(|(pt, pm, pn)| pt == &t.text && pm == &t.mode && pn == &t.negate);
                if !same {
                    self.union_fits(t, &mut cand);
                }
            }
            Some(&cand)
        };
        // Multi-token: a hit lies in every positive token's cover, so a name
        // outside the smallest cover of an earlier (already typed) token
        // can't be a hit's own name. It can still be a folder on a hit's
        // path, so dir names are always scored. The rest go to `pruned`:
        // unscored, but present for rescans and carry.
        let mut restricted: Option<(Vec<u64>, Vec<u64>)> = None;
        let mut used = None;
        if pos.len() >= 2 && self.live.dead_count == 0 && self.live.over.is_empty() {
            let covers: Vec<_> = pos[..pos.len() - 1].iter().map(|t| self.cover(t)).collect();
            if let Some(t) = (0..covers.len().min(8)).filter(|&t| covers[t].names.is_some()).min_by_key(|&t| covers[t].names.as_ref().unwrap().1) {
                let cn = &covers[t].names.as_ref().unwrap().0;
                let (u, dn) = (self.live.base.u, &self.live.dir_names);
                let (s, x) = (0..cn.len())
                    .map(|w| {
                        let all = scan.map_or_else(|| if (w + 1) * 64 <= u { !0 } else { (1u64 << (u - w * 64)) - 1 }, |f| f[w]);
                        let keep = cn[w] | dn[w];
                        (all & keep, all & !keep)
                    })
                    .unzip();
                restricted = Some((s, x));
                used = Some((covers[t].clone(), t));
            }
        }
        let scan = restricted.as_ref().map_or(scan, |r| Some(&r.0[..]));
        let seeds: Vec<u32> = prev.as_ref().map_or_else(Vec::new, |p| p.names.top_names.iter().map(|e| e.0).collect());
        let mut names = self.score_names(q, pos, neg, scan, carry, &seeds);
        if let Some((_, x)) = restricted {
            names.pruned = x;
        }
        let scored = std::sync::Arc::new(Scored { at: std::time::Instant::now(), key, names, memo: std::sync::OnceLock::new(), cover: used });
        *self.live.names_cache.0.lock().unwrap() = Some(scored.clone());
        scored
    }

    fn fast1(&self, q: &Query, pos: &[&Token], neg: &[&Token]) -> bool {
        self.prunable(q, pos, neg)
            && pos[0].mode == Mode::Fuzzy
            && pos[0].text.len() == 1
            && matches!(pos[0].text[0], b'a'..=b'z' | b'0'..=b'9' | b'.' | b' ')
    }

    /// One positive token and nothing else: an entry scores exactly its name's
    /// score plus its stat, so a name table may skip names that can't reach
    /// the page (see `score_names`), reusable only under the same terms.
    fn prunable(&self, q: &Query, pos: &[&Token], neg: &[&Token]) -> bool {
        pos.len() == 1
            && neg.is_empty()
            && q.exts.is_empty()
            && q.name_re.is_none()
            && q.kind.is_none()
            && q.scope.is_none()
            && q.size == (0, u64::MAX)
            && q.mtime == (0, u32::MAX)
            && q.path_re.is_none()
            && q.grep.is_none()
            && self.live.dead_count == 0
            && self.live.over.is_empty()
    }

    /// Score distinct names against the query's name-only predicates: every
    /// name, or only the ones `from` matched. Matches come back sparse (plus
    /// a 256 KB membership bitset), so a selective query never touches a
    /// table the size of the name count.
    /// `seeds`: names likely to rank well (the last table's best), for
    /// warming a pruned table's floor.
    fn score_names(&self, q: &Query, pos: &[&Token], neg: &[&Token], scan: Option<&[u64]>, carry: Option<PrevCtx>, seeds: &[u32]) -> NameTable {
        let idx = &self.live.base;
        let mask = idx.name_mask();
        let ne_off = idx.name_ents_off();
        // Extension slots: a name fails the ext filter by its slot without
        // a byte compare; only slot 255 (a dot with an uncommon or long
        // extension) falls back to `ext_ok`.
        let (ext_allow, name_ext): (Option<[bool; 256]>, &[u8]) = if q.exts.is_empty() {
            (None, &[])
        } else {
            let (_, name_ext) = idx.ext_tables();
            let mut allow = [false; 256];
            for e in &q.exts {
                let f: Vec<u8> = e.iter().map(|&b| fold(b)).collect();
                allow[idx.ext_slot(&f) as usize] = true;
            }
            (Some(allow), name_ext)
        };
        // Only the one-token merge scan consumes `ok_ids`/`top`; collecting
        // them for every query would tax multi-token and filtered scans.
        let collect_merge = pos.len() == 1;
        // Per-chunk accumulator: ok-entry count, best scores for the query
        // bound, the ok names themselves (id-ascending), and the chunk's
        // top names (asc by score) for seeding the merged scan.
        #[derive(Default)]
        struct Acc {
            ok_entries: usize,
            hi_score: i16,
            hi_best: [i16; 4],
            ok_ids: Vec<(i16, u32)>,
            top: Vec<(i16, u32, NameHit)>,
        }
        // Name `k` is bit `k % 64` of `per` (each token's `TokenBits::word`,
        // positive ones first).
        let score_one = |k: usize, per: &[(u64, u64, u64)], acc: &mut Acc| -> Option<NameHit> {
            let m = mask[k];
            let bit = |x: u64| x >> (k % 64) & 1 != 0;
            let fits = |t: usize| bit(per[t].0);
            let known = |t: usize, tok: &Token, name: &[u8]| token_score_known(name, m, tok, bit(per[t].1), bit(per[t].2));
            let negs = || (pos.len()..per.len()).zip(neg);
            // A name no token can match matters only when there are no positive
            // tokens (then every name passes).
            if !pos.is_empty() && !(0..per.len()).any(fits) {
                return None;
            }
            // A one-byte alphanumeric negated token is decided by the mask
            // alone: its char bit is unique, so a fit means a match.
            if negs().any(|(j, t)| fits(j) && t.mask_covers()) {
                return Some(NameHit { score: 0, bits: 0, flags: NF_NEG, best: [0; 4] });
            }
            let name = idx.uname(k as u32);
            let mut h = NameHit { score: 0, bits: 0, flags: 0, best: [0; 4] };
            let mut neg_done = false;
            if let Some(pv) = &carry {
                match pv.tab.get_or_pruned(k as u32) {
                    Some(h0) => {
                        h.flags = if pv.neg_same { h0.flags & NF_NEG } else { 0 };
                        // `bits` answers "matched" only for hits the token
                        // loop actually scored — a skip path (floor-skip,
                        // early negation) leaves it all-zero, so those get
                        // scored like a fresh name.
                        let scored = h0.bits != 0;
                        for (t, tok) in pos.iter().enumerate() {
                            let same = t < pv.ptoks.len() && pv.ptoks[t].0 == tok.text && pv.ptoks[t].1 == tok.mode;
                            if scored && same && t < 4 && h0.bits & (1 << t) != 0 {
                                h.bits |= 1 << t;
                                h.score = h.score.saturating_add(h0.best[t]);
                                h.best[t] = h0.best[t];
                                continue;
                            }
                            if scored && t < pv.ptoks.len() && (same || pv.narrow) && h0.bits & (1 << t) == 0 {
                                continue;
                            }
                            if fits(t) {
                                if let Some(s) = known(t, tok, name) {
                                    h.bits |= 1 << t;
                                    let s16 = s.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
                                    h.score = h.score.saturating_add(s16);
                                    if t < 4 {
                                        h.best[t] = s16;
                                    }
                                }
                            }
                        }
                        neg_done = pv.neg_same;
                    }
                    None => {
                        for (t, tok) in pos.iter().enumerate() {
                            if t < pv.ptoks.len() && ((pv.ptoks[t].0 == tok.text && pv.ptoks[t].1 == tok.mode) || pv.narrow) {
                                continue;
                            }
                            if fits(t) {
                                if let Some(s) = known(t, tok, name) {
                                    h.bits |= 1 << t;
                                    let s16 = s.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
                                    h.score = h.score.saturating_add(s16);
                                    if t < 4 {
                                        h.best[t] = s16;
                                    }
                                }
                            }
                        }
                        neg_done = pv.neg_same;
                    }
                }
            } else {
                for (t, tok) in pos.iter().enumerate() {
                    if fits(t) {
                        if let Some(s) = known(t, tok, name) {
                            h.bits |= 1 << t;
                            let s16 = s.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
                            h.score = h.score.saturating_add(s16);
                            if t < 4 {
                                h.best[t] = s16;
                            }
                        }
                    }
                }
            }
            if !neg_done && negs().any(|(j, t)| fits(j) && token_matches(name, t)) {
                h.flags |= NF_NEG;
            }
            // As a file match it must hit a token and pass name filters;
            // as a folder on someone's path, the raw token bits matter.
            let ok = (pos.is_empty() || h.bits != 0)
                && h.flags & NF_NEG == 0
                && (q.exts.is_empty() || {
                    let s = name_ext[k];
                    if s == 255 { ext_ok(name, &q.exts) } else { ext_allow.as_ref().unwrap()[s as usize] }
                })
                && q.name_re.as_ref().is_none_or(|re| re.is_match(name));
            if ok {
                h.flags |= NF_OK;
                acc.ok_entries += (ne_off[k + 1] - ne_off[k]) as usize;
                acc.hi_score = acc.hi_score.max(h.score);
                if collect_merge {
                    acc.ok_ids.push((h.score, k as u32));
                    if acc.top.len() < SEED_NAMES || h.score > acc.top[0].0 {
                        let p = acc.top.partition_point(|e| e.0 < h.score);
                        acc.top.insert(p, (h.score, k as u32, h));
                        if acc.top.len() > SEED_NAMES {
                            acc.top.remove(0);
                        }
                    }
                }
            }
            let keep = ok || h.bits != 0 || h.flags & NF_NEG != 0;
            if keep {
                for t in 0..4 {
                    acc.hi_best[t] = acc.hi_best[t].max(h.best[t]);
                }
            }
            keep.then_some(h)
        };
        // Each chunk of name ids writes its own slice of the bitset and of a
        // reused dense table. Slots without their bit set are never read, so
        // the table is never cleared (and never page-faulted in again).
        // A one-token query with no name filters stores a hit only for names
        // that matched, and each such hit is `NameHit::one(score)`: `ok_ids`
        // already holds the scores, so it gets no dense table at all.
        let simple = collect_merge && neg.is_empty() && q.exts.is_empty() && q.name_re.is_none();
        let mut bits = vec![0u64; idx.u.div_ceil(64)];
        let mut dense = if simple {
            Vec::new()
        } else {
            DENSE_POOL.lock().unwrap().pop().filter(|d| d.len() == idx.u).unwrap_or_else(|| vec![NameHit::NONE; idx.u])
        };
        let slot_chunks: Vec<&mut [NameHit]> =
            if simple { (0..idx.u.div_ceil(NAME_CHUNK)).map(|_| &mut [][..]).collect() } else { dense.chunks_mut(NAME_CHUNK).collect() };
        // A lone fuzzy char whose class bit is unambiguous: names starting
        // with it score straight from packed data, and the rest score at
        // most 32 — usually too low to reach the entry floor, so their
        // names stay unscored but still marked for rescans.
        let fast1 = self.fast1(q, pos, neg);
        let prune = self.prunable(q, pos, neg);
        // Floor-pruning chunks share their best floor: any chunk's
        // `limit`-th real entry key bounds the answer's from below.
        use std::sync::atomic::Ordering::Relaxed;
        let shared = std::sync::atomic::AtomicU64::new(0);
        if prune && q.limit > 0 {
            // Seed it before the pass: real entries of a few likely names
            // (the last table's best, the top-stat entries'), scored for
            // this query. Their `limit`-th best key is a valid floor from
            // the first word on. Names deduped: each entry counts once.
            let (o_ne, o_nestat, en) = (&self.live.o_ne, &self.live.o_nestat, idx.ent_name());
            let mut ids: Vec<u32> = seeds.iter().copied().chain(self.live.order.iter().take(SEED_STAT).map(|&e| en[e as usize])).collect();
            // A single short token: the by-key lists name the names a token
            // that short can match whole, best key first — cheap, high-quality
            // seeds for the shared floor (upstream `ByKey::heads`/`short`).
            if pos.len() == 1 && pos[0].text.len() <= 3 {
                let t0 = fold(pos[0].text[0]) as usize;
                let m = pos[0].text.len();
                let (bk, info) = (idx.by_key(), idx.name_info());
                ids.extend(bk.heads[t0].iter().take(2048));
                ids.extend(bk.short[t0].iter().filter(|&&k| info[k as usize].len as usize == m || info[k as usize].stem as usize == m).take(2048));
            }
            ids.retain(|&id| (id as usize) < idx.u);
            ids.sort_unstable();
            ids.dedup();
            let mut top = Vec::new();
            for id in ids {
                let (k, m) = (id as usize, mask[id as usize]);
                let Some(s) = pos[0].fits(m).then(|| token_score(idx.uname(id), m, pos[0])).flatten() else { continue };
                let s = s.clamp(i16::MIN as i32, i16::MAX as i32);
                let (a, b) = (ne_off[k] as usize, ne_off[k + 1] as usize);
                top.extend((a..b.min(a + SEED_ENTRIES)).map(|j| key(s + o_nestat[j] as i32, o_ne[j])));
            }
            if top.len() >= q.limit {
                top.select_nth_unstable_by(q.limit - 1, |a, b| b.cmp(a));
                shared.store(top[q.limit - 1], Relaxed);
            }
        }
        // Names pruning left unscored (see `NameTable::pruned`); untouched
        // pages of it are never faulted in.
        let mut pruned = vec![0u64; bits.len()];
        let ub = &self.live.ubase;
        let nw = idx.u.div_ceil(64);
        // Each chunk's slots and words of `bits` and `pruned`, handed out
        // by `par_each` — or all on this thread when the scan set has few
        // words, which takes less time than waking threads.
        let chunks = Cells::new(slot_chunks.into_iter().zip(bits.chunks_mut(NAME_CHUNK / 64)).zip(pruned.chunks_mut(NAME_CHUNK / 64)).map(Some));
        let few = scan.is_some_and(|s| s.iter().filter(|&&w| w != 0).count() <= INLINE_WORDS);
        let each = |f: &(dyn Fn(usize) -> (usize, Acc) + Sync)| -> Vec<(usize, Acc)> {
            if few { (0..chunks.0.len()).map(f).collect() } else { par_each(chunks.0.len(), || (), |_, c| f(c)) }
        };
        // Safety: `each` runs each chunk once.
        let take = |c: usize| unsafe { chunks.take(c) }.unwrap();
        let mut counts: Vec<(usize, Acc)> = if fast1 {
            let c = pos[0].text[0];
            let cbit = crate::index::char_bit(c).trailing_zeros() as usize;
            let (fit, ffit) = (&self.live.char_fit, &self.live.first_fit);
            let (fc, sp, nl) = (&self.live.first_cls, &self.live.stem_pos, &self.live.name_len8);
            let (pk, one) = (&self.live.peak, &self.live.o_ne);
            let b2 = if c.is_ascii_alphanumeric() { 16 } else { 0 };
            each(&|ci| {
                    let ((slots, words), prw) = take(ci);
                    let (a, b) = (ci * NAME_CHUNK, ((ci + 1) * NAME_CHUNK).min(idx.u));
                    let (mut n, mut acc) = (0usize, Acc::default());
                    let mut bounds = TopK::new(q.limit);
                    let mut published = 0u64;
                    let mut skipped = false;
                    let mut skip_hi = i32::MIN;
                    let mut put = |k: usize, h: NameHit| {
                        if !simple {
                            slots[k - a] = h;
                        }
                        words[(k - a) >> 6] |= 1 << (k & 63);
                        n += 1;
                    };
                    for w in a / 64..b.div_ceil(64) {
                        let m = scan.map_or(!0u64, |f| f[w]) & fit[cbit * nw + w];
                        let fbits = ffit[cbit * nw + w];
                        // Starting names score 16 + b2 + placed (<= 100) - len_pen.
                        bounds.floor = bounds.floor.max(shared.load(Relaxed));
                        let skip_a =
                            m & fbits & below(&ub[w * 64..(w * 64 + 64).min(idx.u)], floor_score(bounds.floor).saturating_sub(16 + b2 + 100));
                        if skip_a != 0 {
                            prw[w - a / 64] |= skip_a;
                            skip_hi = skip_hi.max(16 + b2 + 100);
                        }
                        let mut m_a = m & fbits & !skip_a;
                        while m_a != 0 {
                            let k = w * 64 + m_a.trailing_zeros() as usize;
                            m_a &= m_a - 1;
                            let (len, stem, off) = (nl[k] as usize, sp[k] as usize, (fc[k] >> 6) as usize);
                            let placed = if c == b'.' && off != 0 {
                                0
                            } else if off + 1 == len {
                                100
                            } else if off + 1 == stem {
                                80
                            } else {
                                30
                            };
                            let s = 16 + b2 + placed - (len.min(80) / 3) as i32;
                            if key(s + pk[k] as i32, 0) <= bounds.floor {
                                prw[w - a / 64] |= 1 << (k & 63);
                                skip_hi = skip_hi.max(s);
                                continue;
                            }
                            let s16 = s as i16;
                            let h = NameHit { score: s16, bits: 1, flags: NF_OK, best: [s16, 0, 0, 0] };
                            put(k, h);
                            acc.ok_entries += (ne_off[k + 1] - ne_off[k]) as usize;
                            acc.hi_score = acc.hi_score.max(s16);
                            acc.hi_best[0] = acc.hi_best[0].max(h.best[0]);
                            acc.ok_ids.push((s16, k as u32));
                            if acc.top.len() < SEED_NAMES || s16 > acc.top[0].0 {
                                let p = acc.top.partition_point(|e| e.0 < s16);
                                acc.top.insert(p, (s16, k as u32, h));
                                if acc.top.len() > SEED_NAMES {
                                    acc.top.remove(0);
                                }
                            }
                            if ne_off[k] < ne_off[k + 1] {
                                bounds.push(key(s + pk[k] as i32, one[ne_off[k] as usize]));
                            }
                        }
                        if bounds.floor > published {
                            published = bounds.floor;
                            shared.fetch_max(published, Relaxed);
                        }
                        bounds.floor = bounds.floor.max(shared.load(Relaxed));
                        // Non-starting matches cap at 16+8*2−len_pen: 32 + ubase.
                        let skip = m & !fbits & below(&ub[w * 64..(w * 64 + 64).min(idx.u)], floor_score(bounds.floor).saturating_sub(32));
                        prw[w - a / 64] |= skip;
                        skipped |= skip != 0;
                        let mut m_b = m & !fbits & !skip;
                        while m_b != 0 {
                            let k = w * 64 + m_b.trailing_zeros() as usize;
                            m_b &= m_b - 1;
                            if key(32 + ub[k] as i32, 0) <= bounds.floor {
                                prw[w - a / 64] |= 1 << (k & 63);
                                skipped = true;
                                continue;
                            }
                            if acc.ok_entries <= SELECTIVE {
                                acc.ok_entries += (ne_off[k + 1] - ne_off[k]) as usize;
                            }
                            let name = idx.uname(k as u32);
                            if let Some(s) = fuzzy_score(name, &pos[0].text) {
                                let s16 = s.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
                                let h = NameHit { score: s16, bits: 1, flags: NF_OK, best: [s16, 0, 0, 0] };
                                put(k, h);
                                acc.hi_score = acc.hi_score.max(s16);
                                acc.hi_best[0] = acc.hi_best[0].max(h.best[0]);
                                acc.ok_ids.push((s16, k as u32));
                                if acc.top.len() < SEED_NAMES || s16 > acc.top[0].0 {
                                    let p = acc.top.partition_point(|e| e.0 < s16);
                                    acc.top.insert(p, (s16, k as u32, h));
                                    if acc.top.len() > SEED_NAMES {
                                        acc.top.remove(0);
                                    }
                                }
                                if ne_off[k] < ne_off[k + 1] {
                                    bounds.push(key(s + pk[k] as i32, one[ne_off[k] as usize]));
                                }
                            }
                        }
                    }
                    if skipped {
                        // Underscored names still cap at 32; keep the
                        // query bound honest for `full`/`walk` breaks.
                        skip_hi = skip_hi.max(32);
                    }
                    // Each word pushed its starting names first: restore the
                    // id order `lazy_hit` binary-searches.
                    acc.ok_ids.sort_unstable_by_key(|e| e.1);
                    if skip_hi != i32::MIN {
                        let s = skip_hi.min(i16::MAX as i32) as i16;
                        acc.hi_score = acc.hi_score.max(s);
                        acc.hi_best[0] = acc.hi_best[0].max(s);
                    }
                    (n, acc)
                })
        } else {
            // Prunable (one token, nothing else): like fast1, chunks track
            // the `limit`-th best real entry key (a name's best entry: score
            // + peak), and a name whose best possible score plus peak can't
            // beat it stays unscored, marked in `pruned` for rescans. The
            // bound needs no name text. Fuzzy: 16 + 2*8 for the first char,
            // 16 + 8 for each other (gaps only cost), the placed bonus its
            // length, stem and first char allow; typo scores (capped at 30,
            // minus 60) sit below. Exact/prefix/suffix: their fixed best.
            // Minus the length penalty (in `ubase`).
            let (fc, sp, nl, pk, one) = (&self.live.first_cls, &self.live.stem_pos, &self.live.name_len8, &self.live.peak, &self.live.o_ne);
            let (tl, q0) =
                (pos.first().map_or(0, |t| t.text.len()), pos.first().map_or(0, |t| crate::index::char_bit(t.text[0]).trailing_zeros() as u8));
            let mode = pos.first().map_or(Mode::Fuzzy, |t| t.mode);
            let typo = prune && takes_typos(&pos[0].text, mode);
            // The most a name scores, length penalty aside: `top` for any
            // name, `loose_top` for one missing a char class (typo only:
            // fuzzy of a <= tl+1 fix capped at 30, minus TYPO_COST).
            let (top, loose_top) = match mode {
                Mode::Fuzzy => (32 + 24 * (tl as i32 - 1) + 100, 24 * tl as i32 + 2),
                Mode::Exact => (70, 70),
                Mode::Prefix => (60, 60),
                Mode::Suffix => (50, 50),
            };
            // Name `k`'s own bound (`clean`: its `TokenBits` clean bit); plus
            // `ubase` it bounds score + peak.
            let part = |k: usize, clean: bool| -> i32 {
                if mode != Mode::Fuzzy {
                    return top;
                }
                if !clean {
                    return loose_top;
                }
                let (off, len) = ((fc[k] >> 6) as usize, nl[k] as usize);
                let placed = if fc[k] & 63 != q0 {
                    0
                } else if len < 255 && len == off + tl {
                    100
                } else if sp[k] as usize == off + tl {
                    80
                } else {
                    30
                };
                32 + 24 * (tl as i32 - 1) + placed
            };
            // Only names some token's bitsets admit can match it (with no
            // positive token every name is a candidate).
            let tbs: Vec<TokenBits> = pos.iter().chain(neg).map(|t| TokenBits::new(self.live, t)).collect();
            each(&|c| {
                    let ((slots, words), prw) = take(c);
                    let (a, b) = (c * NAME_CHUNK, ((c + 1) * NAME_CHUNK).min(idx.u));
                    let (mut n, mut acc) = (0usize, Acc::default());
                    let mut floor = TopK::new(q.limit);
                    let mut skip_hi = i32::MIN;
                    let mut per = vec![(0, 0, 0); tbs.len()];
                    for w in a / 64..b.div_ceil(64) {
                        let mut m = scan.map_or_else(|| if b - w * 64 >= 64 { !0 } else { (1u64 << (b - w * 64)) - 1 }, |f| f[w]);
                        if m == 0 {
                            continue;
                        }
                        for (p, tb) in per.iter_mut().zip(&tbs) {
                            *p = tb.word(w);
                        }
                        if !pos.is_empty() {
                            m &= per.iter().fold(0, |a, p| a | p.0);
                        }
                        if prune && m != 0 {
                            // Any chunk's floor bounds the answer's: take the best so far.
                            floor.floor = floor.floor.max(shared.load(Relaxed));
                            let (fs, uw) = (floor_score(floor.floor), &ub[w * 64..(w * 64 + 64).min(idx.u)]);
                            let mut skip = below(uw, fs.saturating_sub(top));
                            if mode == Mode::Fuzzy {
                                // No placed bonus unless the first char's class is q[0]'s.
                                skip |= below(uw, fs.saturating_sub(top - 100)) & !self.live.first_fit[q0 as usize * nw + w];
                            }
                            if typo {
                                // Not clean: a typo score at most.
                                skip |= below(uw, fs.saturating_sub(loose_top)) & !per[0].1;
                            }
                            skip &= m;
                            if skip != 0 {
                                prw[w - a / 64] |= skip;
                                skip_hi = skip_hi.max(top);
                                m &= !skip;
                            }
                        }
                        while m != 0 {
                            let k = w * 64 + m.trailing_zeros() as usize;
                            m &= m - 1;
                            if prune {
                                let part = part(k, per[0].1 >> (k % 64) & 1 != 0);
                                if key(part + ub[k] as i32, 0) <= floor.floor {
                                    prw[w - a / 64] |= 1 << (k & 63);
                                    skip_hi = skip_hi.max(part);
                                    continue;
                                }
                            }
                            let Some(h) = score_one(k, &per, &mut acc) else { continue };
                            if !simple {
                                slots[k - a] = h;
                            }
                            words[(k - a) >> 6] |= 1 << (k & 63);
                            n += 1;
                            if prune && h.flags & NF_OK != 0 && ne_off[k] < ne_off[k + 1] {
                                let was = floor.floor;
                                floor.push(key(h.score as i32 + pk[k] as i32, one[ne_off[k] as usize]));
                                if floor.floor > was {
                                    shared.fetch_max(floor.floor, Relaxed);
                                }
                            }
                        }
                    }
                    if skip_hi != i32::MIN {
                        // Keep the query bound honest for skipped names
                        // (their score is at most `part`, length penalty aside).
                        let s = skip_hi.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
                        acc.hi_score = acc.hi_score.max(s);
                        acc.hi_best[0] = acc.hi_best[0].max(s);
                    }
                    (n, acc)
                })
        };
        drop(chunks);
        if !prune {
            pruned = Vec::new();
        }
        let ok_entries = counts.iter().map(|c| c.1.ok_entries).sum();
        let total: usize = counts.iter().map(|c| c.0).sum();
        let hi_score = counts.iter().map(|c| c.1.hi_score).max().unwrap_or(i16::MIN);
        let hi_best = std::array::from_fn(|t| counts.iter().map(|c| c.1.hi_best[t]).max().unwrap_or(0));
        let ok_ids = counts.iter_mut().map(|c| std::mem::take(&mut c.1.ok_ids)).collect();
        let mut top: Vec<(i16, u32, NameHit)> = counts.into_iter().flat_map(|c| c.1.top).collect();
        top.sort_unstable_by(|a, b| b.0.cmp(&a.0));
        top.truncate(SEED_NAMES);
        let top_names = top.into_iter().map(|(_, id, h)| (id, h)).collect();
        let mut t = NameTable {
            bits,
            sparse: Vec::new(),
            dense: (!simple).then_some(dense),
            lazy: simple,
            fast_limit: prune.then_some(q.limit),
            ok_entries,
            hi_score,
            hi_best,
            ok_ids,
            top_names,
            pruned,
        };
        if total <= 1 << 16 {
            // Few matches: keep a compact copy and give the big table back.
            // A lazy table's names are exactly its `ok_ids`, already in id
            // order: flattening them beats a binary search per name.
            t.sparse = if t.lazy && t.ok_ids.iter().map(Vec::len).sum::<usize>() == total {
                t.ok_ids.iter().flatten().map(|&(s, id)| (id, NameHit::one(s))).collect()
            } else {
                t.iter().collect()
            };
            t.lazy = false;
            if let Some(d) = t.dense.take() {
                DENSE_POOL.lock().unwrap().push(d);
            }
        }
        t
    }

    /// `t`'s cover (see `Cover`), cached across keystrokes.
    fn cover(&self, t: &Token) -> std::sync::Arc<Cover> {
        let cache = self.live.cover_cache.0.lock().unwrap();
        if let Some(c) = cache.iter().find(|c| c.text == t.text && c.mode == t.mode) {
            return c.clone();
        }
        drop(cache);
        let idx = &self.live.base;
        let (en, ds, dend, mask, dn) = (idx.ent_name(), idx.dir_start(), idx.dir_end(), idx.name_mask(), &self.live.dir_name);
        // Dir names the token matches (each distinct name scored once)...
        let mut matched = self.live.dir_names.clone();
        matched.par_chunks_mut(256).enumerate().for_each(|(c, ws)| {
            for (j, w) in ws.iter_mut().enumerate() {
                let mut v = *w;
                while v != 0 {
                    let id = (c * 256 + j) * 64 + v.trailing_zeros() as usize;
                    v &= v - 1;
                    let m = mask[id];
                    if !(t.fits(m) && token_score(idx.uname(id as u32), m, t).is_some()) {
                        *w &= !(1 << (id & 63));
                    }
                }
            }
        });
        // ...then the subtree entry ranges of the dirs carrying them. Two
        // subtrees nest or are disjoint: sorted by start, the outermost stay.
        let mut hits: Vec<(u32, u32)> = (1..idx.d)
            .into_par_iter()
            .with_min_len(1 << 14)
            .filter_map(|k| {
                let id = dn[k];
                (matched[id as usize >> 6] >> (id & 63) & 1 != 0 && ds[k] < dend[k]).then_some((ds[k], dend[k]))
            })
            .collect();
        hits.sort_unstable();
        let (mut ranges, mut end, mut total) = (Vec::new(), 0u32, 0usize);
        for (a, b) in hits {
            if a < end {
                continue;
            }
            ranges.push((a, b));
            end = b;
            total += (b - a) as usize;
            if total > COVER_MAX {
                break;
            }
        }
        let names = (total <= COVER_MAX).then(|| {
            let mut bits = vec![0u64; idx.u.div_ceil(64)];
            self.union_fits(t, &mut bits);
            for &(a, b) in &ranges {
                for &id in &en[a as usize..b as usize] {
                    bits[id as usize >> 6] |= 1 << (id & 63);
                }
            }
            let n = bits.iter().map(|w| w.count_ones() as usize).sum();
            (bits, n, ranges)
        });
        let c = std::sync::Arc::new(Cover { text: t.text.clone(), mode: t.mode, names });
        let mut cache = self.live.cover_cache.0.lock().unwrap();
        if cache.len() >= 8 {
            cache.remove(0);
        }
        cache.push(c.clone());
        c
    }

    /// Sets `out`'s bit for every name `t` can match by its bitsets
    /// (`TokenBits`). For a one-char token that is exactly its class's
    /// names (what `Token::mask_covers` relies on).
    fn union_fits(&self, t: &Token, out: &mut [u64]) {
        let tb = TokenBits::new(self.live, t);
        out.par_chunks_mut(2048).enumerate().for_each(|(c, w)| {
            for (j, wj) in w.iter_mut().enumerate() {
                *wj |= tb.word(c * 2048 + j).0;
            }
        });
    }

    /// The overlay (entries added since the last compaction) has no dir
    /// memo: each candidate's path components stand in for it. Its top
    /// `limit` is all the merge can use.
    fn search_overlay(&self, q: &Query) -> Vec<Hit> {
        let now = now_secs();
        let pos: Vec<&Token> = q.tokens.iter().filter(|t| !t.negate).collect();
        // A hit needs some positive token in its own name.
        // `flat` lays the overlay out so the mask pass scans one array.
        let flat = &self.live.flat;
        let cands: Vec<&(Vec<u8>, crate::live::OEnt)> = flat
            .masks
            .iter()
            .zip(&flat.items)
            .filter(|&(&m, _)| m != 0 && (pos.is_empty() || pos.iter().any(|t| t.fits(m))))
            .map(|(_, item)| item)
            .collect();
        if cands.is_empty() {
            return Vec::new();
        }
        // Overlay entries cluster in a few busy folders: match each folder's
        // components once per folder, not per entry.
        let score = |items: &[&(Vec<u8>, crate::live::OEnt)]| {
            let mut memo = HashMap::<&[u8], DirMatch, crate::index::Fx>::default();
            let mut out = Vec::new();
            for &&(ref path, o) in items {
                let cut = path.iter().rposition(|&b| b == b'/').unwrap_or(0);
                let dir = &path[..cut];
                if let Some(score) = q.match_path_with(path, o.kind, o.size, o.mtime, |_| *memo.entry(dir).or_insert_with(|| q.dir_match(dir))) {
                    let score = score + o.prior as i32 + rank_tweaks(name_flags(&path[cut + 1..]), o.kind, o.mtime, now);
                    out.push(Hit { score, idx: u32::MAX, over: Some(path.clone()) });
                }
            }
            out
        };
        let mut hits = if cands.len() <= OVERLAY_CHUNK { score(&cands) } else { cands.par_chunks(OVERLAY_CHUNK).flat_map_iter(score).collect() };
        // Best first, equal scores by path (the overlay map's order).
        hits.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.over.cmp(&b.over)));
        hits.truncate(q.limit);
        hits
    }

    /// For each dir: which tokens its name or an ancestor's matches, with the
    /// best score per token (first 4); bits == u32::MAX if a negated token does.
    fn dir_tokens(&self, names: &NameTable) -> Vec<DirMemo> {
        let idx = &self.live.base;
        let de = idx.dir_entry();
        let en = idx.ent_name();
        let mut out = MEMO_POOL.lock().unwrap().pop().unwrap_or_default();
        out.clear();
        out.resize(idx.d, DirMemo::default());
        out.par_iter_mut().enumerate().with_min_len(1 << 12).for_each(|(k, slot)| {
            if k > 0 {
                *slot = DirMemo::own(names, en[de[k] as usize]);
            }
        });
        // Fold ancestors in, parents first. A dir's descendants are one
        // contiguous id range, so the children of huge dirs go one by one,
        // then every small subtree in parallel.
        let dp = idx.dir_parent();
        let plan = idx.memo_plan();
        for &k in &plan.upper {
            out[k as usize] = out[k as usize].under(out[dp[k as usize] as usize]);
        }
        let roots: Vec<(u32, DirMemo)> = plan.chunks.iter().map(|(c, _)| (*c, out[*c as usize])).collect();
        let mut slices = Vec::with_capacity(plan.chunks.len());
        let mut rest: &mut [DirMemo] = &mut out;
        let mut at = 0usize;
        for (_, r) in &plan.chunks {
            let (_, tail) = std::mem::take(&mut rest).split_at_mut(r.start as usize - at);
            let (mine, tail) = tail.split_at_mut((r.end - r.start) as usize);
            slices.push(mine);
            rest = tail;
            at = r.end as usize;
        }
        slices.into_par_iter().zip(&plan.chunks).zip(roots).for_each(|((slice, (_, r)), (c, root))| {
            let a = r.start as usize;
            for k in a..r.end as usize {
                let p = dp[k];
                let pm = if p == c { root } else { slice[p as usize - a] };
                slice[k - a] = slice[k - a].under(pm);
            }
        });
        out
    }
}

/// Debug switch: always take the full pass (for checking the selective one).
pub static FULL_PASS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Profiling counters (temporary): time inside score_names vs the entry scan.
#[doc(hidden)]
pub static PROF_NAMES_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
#[doc(hidden)]
pub static PROF_SCAN_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
#[doc(hidden)]
pub static PROF_MEMO_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
#[doc(hidden)]
pub static PROF_EVALS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// An empty name table for tokenless queries, whose scores are static.
static EMPTY_TABLE: std::sync::OnceLock<NameTable> = std::sync::OnceLock::new();

/// How many top names a query keeps for warming the merged scan's floor.
const SEED_NAMES: usize = 32;
/// How many of each seed name's stat-sorted entries seed the floor.
const SEED_ENTRIES: usize = 8;
/// How many top-stat entries' names seed a pruned table's floor.
const SEED_STAT: usize = 512;
/// A scoped `walk` takes this many steps per scope entry before it visits
/// the scope in place.
const WALK_SCOPE: usize = 4;

/// Is dir `d`'s own name or one of its ancestors' negated? `is_neg` answers
/// for a name id; `dirneg` remembers answers (0 unknown, 1 yes, 2 no), so
/// each dir is priced once and only the dirs asked about are priced at all.
fn dir_neg_at(idx: &Index, d: usize, dirneg: &mut [u8], chain: &mut Vec<usize>, is_neg: &dyn Fn(u32) -> bool) -> bool {
    if dirneg[d] != 0 {
        return dirneg[d] == 1;
    }
    let (de, en, dp) = (idx.dir_entry(), idx.ent_name(), idx.dir_parent());
    chain.clear();
    let mut k = d;
    let mut neg = false;
    while k != 0 {
        match dirneg[k] {
            1 => {
                neg = true;
                break;
            }
            2 => break,
            _ => {}
        }
        chain.push(k);
        if is_neg(en[de[k] as usize]) {
            neg = true;
            break;
        }
        k = dp[k] as usize;
    }
    // The negated node (or known-neg ancestor) always ends the walk
    // above every node the chain collected, so the whole chain
    // inherits its answer.
    for &c in chain.iter() {
        dirneg[c] = 1 + u8::from(!neg);
    }
    neg
}

/// The `NameHit` a pos-empty walk scores with: the flag check already
/// happened in `walk`, and with no positive tokens bits/score are all 0.
const OK_HIT: NameHit = NameHit { score: 0, bits: 0, flags: NF_OK, best: [0; 4] };

struct Scan<'a> {
    q: &'a Query,
    live: &'a Live,
    names: &'a NameTable,
    npos: usize,
    need_dirs: bool,
    /// Dead-bitset lookups run only when something was ever killed.
    dead: bool,
    /// The most name and folder matches can add to an entry's `stat`.
    qbound: i32,
}

impl Scan<'_> {
    /// Score entry `i` whose name scored `nh`, given its parent's memo and
    /// the entry's static score `st`. `None` if it fails a filter or cannot
    /// beat `floor`.
    #[inline(always)]
    fn score(&self, i: usize, st: i32, nh: NameHit, memo: DirMemo, floor: u64, pbuf: &mut Vec<u8>) -> Option<u64> {
        let idx = &self.live.base;
        let q = self.q;
        if q.kind.is_some_and(|want| idx.kind()[i] & 3 != want) {
            return None;
        }
        if q.size != (0, u64::MAX) {
            let sz = crate::index::dec_size(idx.size_raw()[i]);
            if sz < q.size.0 || sz > q.size.1 {
                return None;
            }
        }
        if q.mtime != (0, u32::MAX) {
            let mtime = idx.mtime()[i];
            if mtime < q.mtime.0 || mtime > q.mtime.1 {
                return None;
            }
        }
        let mut score = nh.score as i32;
        if self.need_dirs {
            let all = (1u32 << self.npos) - 1;
            if memo.bits == u32::MAX || (nh.bits as u32 | memo.bits) & all != all {
                return None;
            }
            for t in 0..self.npos {
                if nh.bits & (1 << t) == 0 {
                    // Matched by a folder on the path instead.
                    score += memo.best.get(t).map_or(6, |&b| b as i32 * 3 / 4);
                }
            }
        }
        if self.dead && self.live.is_dead(i as u32) {
            return None;
        }
        score += st;
        let key = key(score, i as u32);
        if key <= floor {
            return None;
        }
        if let Some(re) = &q.path_re {
            idx.path(i, pbuf);
            if !re.is_match(pbuf) {
                return None;
            }
        }
        Some(key)
    }

    /// One sequential pass over every entry in `lo..hi`.
    fn full(&self, lo: usize, hi: usize, memo: Option<&[DirMemo]>) -> Vec<Hit> {
        let idx = &self.live.base;
        let (ent_name, parent) = (idx.ent_name(), idx.parent());
        top_k(hi - lo, self.q.limit, hi - lo, 0, |r, top| {
            let mut pbuf = Vec::new();
            for i in lo + r.start..lo + r.end {
                let Some(nh) = self.names.get(ent_name[i]).filter(|h| h.flags & NF_OK != 0) else { continue };
                let m = memo.map_or(DirMemo::default(), |m| m[parent[i] as usize]);
                if let Some(k) = self.score(i, self.live.stat[i], nh, m, top.floor, &mut pbuf) {
                    top.push(k);
                }
            }
        })
    }

    /// Entries by static score, best first. With no positive tokens a hit's
    /// score is exactly `stat[i]`, so the top-k is the first `limit`
    /// survivors of this walk — usually tens of entries, not millions.
    /// Dir memos come from `memo_of` lazily, so negation never folds the
    /// whole dir tree either.
    /// Pos-empty scan: `ok(name id)` decides whether an entry's own name
    /// passes the name-side predicates, `is_neg(name id)` whether it is
    /// negated (checked lazily up each visited entry's dir chain).
    #[inline(always)]
    fn walk(&self, lo: usize, hi: usize, ok: &dyn Fn(u32) -> bool, is_neg: &dyn Fn(u32) -> bool) -> Vec<Hit> {
        let idx = &self.live.base;
        let (order, ostat, o_en, parent) = (&self.live.order[..], &self.live.ostat[..], &self.live.o_en[..], idx.parent());
        let mut dirneg = if self.need_dirs { vec![0u8; idx.d] } else { Vec::new() };
        let mut chain = Vec::new();
        let mut top = TopK::new(self.q.limit);
        let mut pbuf = Vec::new();
        // Nothing can fail a name: no tokens, exts or name regex to check.
        let all_ok = self.npos == 0 && !self.need_dirs && self.q.exts.is_empty() && self.q.name_re.is_none();
        let qb = self.qbound;
        let mut dbg_scanned = 0u64;
        // A size/mtime/kind filter skips whole runs of `order` whose ranges
        // it excludes: a filter matching nothing reads ~40K run summaries
        // instead of gathering every entry's stats.
        let q = self.q;
        let stats = q.size != (0, u64::MAX) || q.mtime != (0, u32::MAX) || q.kind.is_some();
        let excluded = |b: &Blk| {
            b.size.1 < q.size.0
                || b.size.0 > q.size.1
                || b.mtime.1 < q.mtime.0
                || b.mtime.0 > q.mtime.1
                || q.kind.is_some_and(|k| b.kinds & (1 << k) == 0)
        };
        // A scope's best entries may come early in the disk-wide order, or
        // not for millions of steps: past WALK_SCOPE times its size, the
        // walk gives up and visits the scope in place instead.
        let all = order.len();
        let order = &order[..all.min(WALK_SCOPE.saturating_mul(hi - lo))];
        let (mut p, mut settled) = (0, false);
        while p < order.len() {
            if stats && p % OBLK == 0 && excluded(&self.live.oblk[p / OBLK]) {
                p += OBLK;
                continue;
            }
            let (p, e) = {
                p += 1;
                (p - 1, order[p - 1])
            };
            // Later entries score at most ostat[p]+qb; only entries at stat
            // ostat[p] itself can reach that bound, and the smallest index
            // still to come there is e (same-stat runs go ascending), so
            // key(ostat[p]+qb, e) is the largest remaining key.
            if key(ostat[p] + qb, e) <= top.floor {
                settled = true;
                break;
            }
            let i = e as usize;
            if i < lo || i >= hi {
                continue;
            }
            if !all_ok && !ok(o_en[p]) {
                continue;
            }
            if self.need_dirs && dir_neg_at(idx, parent[i] as usize, &mut dirneg, &mut chain, is_neg) {
                continue;
            }
            dbg_scanned += 1;
            if let Some(k) = self.score(i, ostat[p], OK_HIT, DirMemo::default(), top.floor, &mut pbuf) {
                top.push(k);
            }
        }
        if !settled && p < all {
            return self.visit_scope(lo, hi, all_ok, ok, is_neg);
        }
        if std::env::var_os("WALK_DBG").is_some() {
            eprintln!("walk: candidates={} results={}", dbg_scanned, top.buf.len());
        }
        top.cut();
        top.buf.into_iter().map(|x| Hit { score: ((x >> 32) as i64 + i32::MIN as i64) as i32, idx: !(x as u32), over: None }).collect()
    }

    /// `walk` over the scope `lo..hi` in place.
    #[inline(never)]
    fn visit_scope(&self, lo: usize, hi: usize, all_ok: bool, ok: &dyn Fn(u32) -> bool, is_neg: &dyn Fn(u32) -> bool) -> Vec<Hit> {
        let idx = &self.live.base;
        let (en, parent) = (idx.ent_name(), idx.parent());
        let mut dirneg = if self.need_dirs { vec![0u8; idx.d] } else { Vec::new() };
        let (mut top, mut chain, mut pbuf) = (TopK::new(self.q.limit), Vec::new(), Vec::new());
        for i in lo..hi {
            if (all_ok || ok(en[i]))
                && !(self.need_dirs && dir_neg_at(idx, parent[i] as usize, &mut dirneg, &mut chain, is_neg))
                && let Some(k) = self.score(i, self.live.stat[i], OK_HIT, DirMemo::default(), top.floor, &mut pbuf)
            {
                top.push(k);
            }
        }
        top.cut();
        top.buf.into_iter().map(|x| Hit { score: ((x >> 32) as i64 + i32::MIN as i64) as i32, idx: !(x as u32), over: None }).collect()
    }

    /// One positive token, broad match: an entry's score is name + stat, so
    /// merge the ok names' stat-sorted entry lists — a name whose best
    /// (highest-stat) entry can't reach the floor is skipped outright, and
    /// each list stops the moment its stat dips below it. The seed names'
    /// heads warm the floor first; the seeds themselves re-enter through
    /// the merge (same list positions), so no dedup pass is needed.
    /// Negations need the dir memo only for the entries the merge visits:
    /// `memo_of`, lazily, per piece.
    fn byname(&self, lo: usize, hi: usize) -> Vec<Hit> {
        let idx = &self.live.base;
        let (ne_off, o_ne, o_nestat, peak) = (idx.name_ents_off(), &self.live.o_ne[..], &self.live.o_nestat[..], &self.live.peak[..]);
        let parent = idx.parent();
        let dm = |i: usize, cache: &mut HashMap<u32, DirMemo, crate::index::Fx>| {
            if self.need_dirs { self.memo_of(parent[i], cache, &|id| self.names.get(id)) } else { DirMemo::default() }
        };
        let mut seeds = TopK::new(self.q.limit);
        let (mut pbuf, mut cache) = (Vec::new(), HashMap::default());
        for &(id, nh) in &self.names.top_names {
            let (a, b) = (ne_off[id as usize] as usize, ne_off[id as usize + 1] as usize);
            for j in a..b.min(a + SEED_ENTRIES) {
                let i = o_ne[j] as usize;
                if i < lo || i >= hi {
                    continue;
                }
                if let Some(k) = self.score(i, o_nestat[j] as i32, nh, dm(i, &mut cache), seeds.floor, &mut pbuf) {
                    seeds.push(k);
                }
            }
        }
        seeds.cut();
        // The merge rejects `key <= floor`, so the seed holding the floor
        // would drop out of its own re-entry. Floor keys are unique, so
        // one ulp down admits exactly every key >= the seed boundary.
        let fl = seeds.floor.saturating_sub(1);
        // The merge takes `ok_ids` a chunk at a time; each chunk's own floor
        // rises as its keys come in, and its top-`limit` keys must contain
        // its share of the global top — so the union of chunk tops is the
        // answer's exact superset, and a final pick is byte-identical.
        let limit = self.q.limit;
        let mut buf: Vec<u64> = self
            .names
            .ok_ids
            .par_iter()
            .flat_map_iter(|ch| {
                let mut top = TopK::new(limit);
                top.floor = fl;
                let (mut pbuf, mut cache) = (Vec::new(), HashMap::default());
                for &(s, id) in ch {
                    // Even this name's best entry can't reach the floor: skip it.
                    if key(s as i32 + peak[id as usize] as i32, 0) <= top.floor {
                        continue;
                    }
                    let nh = NameHit::one(s);
                    let (a, b) = (ne_off[id as usize] as usize, ne_off[id as usize + 1] as usize);
                    for j in a..b {
                        // Entries run stat-desc: once the best possible remaining
                        // key (score s + this stat, index 0) can't beat the floor,
                        // none later in this list can.
                        if key(s as i32 + o_nestat[j] as i32, 0) <= top.floor {
                            break;
                        }
                        let i = o_ne[j] as usize;
                        if i < lo || i >= hi {
                            continue;
                        }
                        if let Some(k) = self.score(i, o_nestat[j] as i32, nh, dm(i, &mut cache), top.floor, &mut pbuf) {
                            top.push(k);
                        }
                    }
                }
                top.cut();
                top.buf
            })
            .collect();
        buf.sort_unstable_by(|a, b| b.cmp(a));
        buf.truncate(limit);
        buf.into_iter().map(|x| Hit { score: ((x >> 32) as i64 + i32::MIN as i64) as i32, idx: !(x as u32), over: None }).collect()
    }

    /// Visit only the entries carrying a matching name; folder tokens are
    /// checked by walking each candidate's ancestors (memoized per piece).
    fn selective(&self, lo: usize, hi: usize) -> Vec<Hit> {
        let idx = &self.live.base;
        let (ne_off, ne, parent) = (idx.name_ents_off(), idx.name_ents(), idx.parent());
        // The candidates, ascending: the same list `names.iter()` yields,
        // gathered in parallel per bitset chunk.
        let ok: Vec<(u32, NameHit)> = match &self.names.dense {
            Some(d) => self
                .names
                .bits
                .par_chunks(2048)
                .enumerate()
                .flat_map_iter(|(c, w)| {
                    let mut v = Vec::new();
                    for (j, &b) in w.iter().enumerate() {
                        let mut b = b;
                        while b != 0 {
                            let id = (c * 2048 + j) as u32 * 64 + b.trailing_zeros();
                            let h = d[id as usize];
                            if h.flags & NF_OK != 0 {
                                v.push((id, h));
                            }
                            b &= b - 1;
                        }
                    }
                    v.into_iter()
                })
                .collect(),
            None if self.names.lazy => self.names.iter().filter(|(_, h)| h.flags & NF_OK != 0).collect(),
            None => self.names.sparse.par_iter().copied().filter(|(_, h)| h.flags & NF_OK != 0).collect(),
        };
        top_k(ok.len(), self.q.limit, self.names.ok_entries, 0, |r, top| {
            let (mut pbuf, mut memo) = (Vec::new(), HashMap::<u32, DirMemo, crate::index::Fx>::default());
            for &(id, nh) in &ok[r] {
                for &e in &ne[ne_off[id as usize] as usize..ne_off[id as usize + 1] as usize] {
                    let i = e as usize;
                    if i < lo || i >= hi {
                        continue;
                    }
                    let m = if self.need_dirs { self.memo_of(parent[i], &mut memo, &|id| self.names.get(id)) } else { DirMemo::default() };
                    if let Some(k) = self.score(i, self.live.stat[i], nh, m, top.floor, &mut pbuf) {
                        top.push(k);
                    }
                }
            }
        })
    }

    /// Multi-token, restricted to token `t`'s cover: the entries under its
    /// matching dirs (`ranges`) plus, outside them, the entries whose own
    /// name matches it. Every hit is one of these; dir memos come lazily.
    fn covered(&self, lo: usize, hi: usize, ranges: &[(u32, u32)], t: usize) -> Vec<Hit> {
        let idx = &self.live.base;
        let (ne_off, ne, en, parent) = (idx.name_ents_off(), idx.name_ents(), idx.ent_name(), idx.parent());
        let inside = |e: u32| {
            let p = ranges.partition_point(|r| r.0 <= e);
            p > 0 && e < ranges[p - 1].1
        };
        let ids: Vec<u32> = self.names.iter().filter(|(_, h)| h.flags & NF_OK != 0 && h.bits & (1 << t) != 0).map(|(id, _)| id).collect();
        let outside: Vec<u32> = ids
            .par_iter()
            .flat_map_iter(|&id| ne[ne_off[id as usize] as usize..ne_off[id as usize + 1] as usize].iter().copied().filter(|&e| !inside(e)))
            .collect();
        // One virtual list: the ranges (clipped to lo..hi), then `outside`.
        let segs: Vec<(u32, u32)> = ranges.iter().map(|&(a, b)| (a.max(lo as u32), b.min(hi as u32))).filter(|s| s.0 < s.1).collect();
        let mut starts = Vec::with_capacity(segs.len());
        let mut nr = 0usize;
        for s in &segs {
            starts.push(nr);
            nr += (s.1 - s.0) as usize;
        }
        top_k(nr + outside.len(), self.q.limit, nr + outside.len(), 0, |r, top| {
            let (mut pbuf, mut memo) = (Vec::new(), HashMap::<u32, DirMemo, crate::index::Fx>::default());
            let mut last = (u32::MAX, DirMemo::default());
            let mut visit = |i: usize, top: &mut TopK| {
                let Some(nh) = self.names.get(en[i]).filter(|h| h.flags & NF_OK != 0) else { return };
                let d = parent[i];
                if last.0 != d {
                    last = (d, self.memo_of(d, &mut memo, &|id| self.names.get(id)));
                }
                if let Some(k) = self.score(i, self.live.stat[i], nh, last.1, top.floor, &mut pbuf) {
                    top.push(k);
                }
            };
            let (a, b) = (r.start.min(nr), r.end.min(nr));
            if a < b {
                let (mut s, mut p) = (starts.partition_point(|&x| x <= a) - 1, a);
                while p < b {
                    let (sa, sb) = segs[s];
                    let off = p - starts[s];
                    let take = ((sb - sa) as usize - off).min(b - p);
                    for i in sa as usize + off..sa as usize + off + take {
                        visit(i, top);
                    }
                    p += take;
                    s += 1;
                }
            }
            for &e in &outside[r.start.max(nr) - nr..r.end.max(nr) - nr] {
                if (lo..hi).contains(&(e as usize)) {
                    visit(e as usize, top);
                }
            }
        })
    }

    /// The dir memo of `d` (see `dir_tokens`), from its ancestor chain.
    fn memo_of(&self, d: u32, cache: &mut HashMap<u32, DirMemo, crate::index::Fx>, names: &dyn Fn(u32) -> Option<NameHit>) -> DirMemo {
        let idx = &self.live.base;
        let (de, en, dp) = (idx.dir_entry(), idx.ent_name(), idx.dir_parent());
        let mut chain = Vec::new();
        let mut k = d;
        let mut acc = loop {
            if k == 0 {
                break DirMemo::default();
            }
            if let Some(&m) = cache.get(&k) {
                break m;
            }
            chain.push(k);
            k = dp[k as usize];
        };
        for &k in chain.iter().rev() {
            acc = DirMemo::own_hit(names(en[de[k as usize] as usize])).under(acc);
            cache.insert(k, acc);
        }
        acc
    }
}

/// The dir memo is ~13 MB; reusing it saves a page-fault storm per query.
static MEMO_POOL: std::sync::Mutex<Vec<Vec<DirMemo>>> = std::sync::Mutex::new(Vec::new());

/// What the name table depends on: two queries with the same key score
/// every name the same.
#[derive(PartialEq)]
pub(crate) struct NameKey {
    tokens: Vec<(Vec<u8>, Mode, bool)>,
    exts: Vec<Vec<u8>>,
    name_re: Option<String>,
}

impl NameKey {
    fn of(q: &Query) -> NameKey {
        NameKey {
            tokens: q.tokens.iter().map(|t| (t.text.clone(), t.mode, t.negate)).collect(),
            exts: q.exts.clone(),
            name_re: q.name_re.as_ref().map(|r| r.as_str().to_string()),
        }
    }

    /// Can only match names `prev` matched: same filters, same tokens, each
    /// positive token the same or longer in a way that only narrows it.
    fn narrows(&self, prev: &NameKey) -> bool {
        self.exts == prev.exts
            && self.name_re == prev.name_re
            && self.tokens.len() == prev.tokens.len()
            && self.tokens.iter().any(|t| !t.2)
            && self.tokens.iter().zip(&prev.tokens).all(|(a, b)| {
                a.1 == b.1
                    && a.2 == b.2
                    && if a.2 {
                        a.0 == b.0
                    } else if a.1 == Mode::Suffix {
                        a.0.ends_with(&b.0)
                    } else {
                        // Gaining a typo widens the match: score afresh.
                        a.0.starts_with(&b.0) && takes_typos(&a.0, a.1) == takes_typos(&b.0, b.1)
                    }
            })
    }
}

/// The previous query's name table for carry-over scoring: which of the
/// current positive tokens it already covered, whether the query only
/// narrowed (so an unmatched token can never match), and whether the
/// negations are unchanged (so their flags carry).
struct PrevCtx<'a> {
    tab: &'a NameTable,
    ptoks: Vec<&'a (Vec<u8>, Mode, bool)>,
    narrow: bool,
    neg_same: bool,
}

/// The last query's name table (and dir memo, built on first use).
pub struct Scored {
    at: std::time::Instant,
    key: NameKey,
    names: NameTable,
    memo: std::sync::OnceLock<Vec<DirMemo>>,
    /// The cover the table was restricted to, and its token's index: every
    /// hit lies in it, so the entry scan can too.
    cover: Option<(std::sync::Arc<Cover>, usize)>,
}

impl Drop for Scored {
    fn drop(&mut self) {
        if let Some(v) = self.memo.take() {
            let mut pool = MEMO_POOL.lock().unwrap();
            if pool.is_empty() {
                pool.push(v);
            }
        }
    }
}

/// Lives with the index it was scored against (`Live`).
#[derive(Default)]
pub struct NameCache(std::sync::Mutex<Option<std::sync::Arc<Scored>>>);

/// Pos-empty query helper: `ok`/`negbits` name bitsets depend only on the
/// name-side predicates (neg tokens, name regex), so an identical key — a
/// repeat, or a stat-filter-only edit — can reuse them verbatim.
#[derive(Default)]
pub struct PosNegCache(pub(crate) std::sync::Mutex<Option<(NameKey, Vec<u64>, Vec<u64>)>>);

impl PosNegCache {
    #[doc(hidden)]
    pub fn clear(&self) {
        *self.0.lock().unwrap() = None;
    }
}

/// A positive token's cover: every name an entry the token covers can
/// carry — the entry's own name fits the token, or it lies under a dir
/// whose name matches it. `None` when that's too broad to be worth it.
/// Depends only on the token and the base tree.
pub(crate) struct Cover {
    text: Vec<u8>,
    mode: Mode,
    /// The names (bitset, count) and the outermost matching dirs' subtree
    /// entry ranges (ascending, disjoint).
    names: Option<(Vec<u64>, usize, Vec<(u32, u32)>)>,
}

/// The last few tokens' covers: a multi-token query is typed one token
/// at a time, so the earlier tokens' covers repeat keystroke after keystroke.
#[derive(Default)]
pub struct CoverCache(std::sync::Mutex<Vec<std::sync::Arc<Cover>>>);

/// Covers spanning more entries than this aren't built.
const COVER_MAX: usize = 1 << 21;

impl NameCache {
    /// Drop the cached table now (profiling/diagnostics).
    #[doc(hidden)]
    pub fn clear(&self) {
        *self.0.lock().unwrap() = None;
    }

    /// Drop the cached table and spare buffers once searching has stopped:
    /// tens of MB after a broad query, worth keeping only while typing.
    pub fn trim_if_idle(&self, idle: std::time::Duration) {
        let mut g = self.0.lock().unwrap();
        if g.as_ref().is_some_and(|s| s.at.elapsed() > idle) {
            *g = None;
            drop(g);
            trim_pools();
        }
    }
}

#[derive(Clone, Copy, Default)]
pub struct DirMemo {
    bits: u32,
    best: [i16; 4],
}

impl DirMemo {
    /// A dir's own name's contribution.
    #[inline]
    fn own(names: &NameTable, name: u32) -> DirMemo {
        Self::own_hit(names.get(name))
    }

    /// The contribution of a name whose hit was already looked up.
    #[inline]
    fn own_hit(h: Option<NameHit>) -> DirMemo {
        match h {
            Some(h) if h.flags & NF_NEG != 0 => DirMemo { bits: u32::MAX, best: [0; 4] },
            Some(h) => DirMemo { bits: h.bits as u32, best: h.best.map(|s| s.max(0)) },
            None => DirMemo::default(),
        }
    }

    /// This dir's memo with its parent's folded in.
    #[inline]
    fn under(self, p: DirMemo) -> DirMemo {
        if p.bits == u32::MAX || self.bits == u32::MAX {
            return DirMemo { bits: u32::MAX, best: self.best };
        }
        let mut best = self.best;
        for t in 0..4 {
            best[t] = best[t].max(p.best[t]);
        }
        DirMemo { bits: self.bits | p.bits, best }
    }
}

/// Spare dense name tables (24 MB each on this disk), so a broad query
/// doesn't page-fault a fresh one in.
static DENSE_POOL: std::sync::Mutex<Vec<Vec<NameHit>>> = std::sync::Mutex::new(Vec::new());

/// Free the spare buffers searches keep for speed (after a quiet spell).
pub fn trim_pools() {
    DENSE_POOL.lock().unwrap().clear();
    MEMO_POOL.lock().unwrap().clear();
}

impl Drop for NameTable {
    fn drop(&mut self) {
        if let Some(d) = self.dense.take() {
            let mut pool = DENSE_POOL.lock().unwrap();
            if pool.len() < 2 {
                pool.push(d);
            }
        }
    }
}

struct NameTable {
    bits: Vec<u64>,
    sparse: Vec<(u32, NameHit)>,
    dense: Option<Vec<NameHit>>,
    /// No dense table: a name's hit is rebuilt from `ok_ids` (see `lazy_hit`).
    /// Only for big one-token tables with no name filters.
    lazy: bool,
    /// A floor-pruned table is reusable only for the same unrestricted
    /// query with an equal or smaller result limit.
    fast_limit: Option<usize>,
    /// Entries carrying a name that passes as a match (NF_OK).
    ok_entries: usize,
    /// Best `score` over NF_OK names, and best per-token scores over all
    /// names in the table: together they bound what any entry can add to
    /// its static score (`bound`).
    hi_score: i16,
    hi_best: [i16; 4],
    /// `(score, id)` of every NF_OK name, id-ascending within each name
    /// chunk's list: the one-token merge scan iterates these and pulls each
    /// name's stat-sorted entries.
    ok_ids: Vec<Vec<(i16, u32)>>,
    /// The best-scoring NF_OK names (up to SEED_NAMES), for warming the
    /// merged scan's floor.
    top_names: Vec<(u32, NameHit)>,
    /// Names a floor-pruned table skipped unscored (empty if none): not in
    /// `bits`, yet they may match, so rescans and carry treat them as
    /// present but unscored.
    pruned: Vec<u64>,
}

impl NameTable {
    /// The all-empty table for tokenless queries whose entries are static.
    fn empty() -> NameTable {
        NameTable {
            bits: Vec::new(),
            sparse: Vec::new(),
            dense: None,
            lazy: false,
            fast_limit: None,
            ok_entries: 0,
            hi_score: 0,
            hi_best: [0; 4],
            ok_ids: Vec::new(),
            top_names: Vec::new(),
            pruned: Vec::new(),
        }
    }

    /// Every name this table might hold a match for: `bits` plus `pruned`.
    fn maybe(&self) -> std::borrow::Cow<'_, [u64]> {
        if self.pruned.is_empty() {
            return std::borrow::Cow::Borrowed(&self.bits);
        }
        std::borrow::Cow::Owned(self.bits.iter().zip(&self.pruned).map(|(&b, &p)| b | p).collect())
    }

    /// The table's hit for a carry: a pruned name reads as present but
    /// unscored (`NONE`, `bits == 0`).
    #[inline]
    fn get_or_pruned(&self, id: u32) -> Option<NameHit> {
        self.get(id).or_else(|| (self.pruned.get(id as usize >> 6).map_or(0, |&w| w) >> (id & 63) & 1 != 0).then_some(NameHit::NONE))
    }

    #[inline(always)]
    fn get(&self, id: u32) -> Option<NameHit> {
        if self.bits.get(id as usize >> 6).map_or(0, |&w| w) & (1 << (id & 63)) == 0 {
            return None;
        }
        match &self.dense {
            Some(d) => Some(d[id as usize]),
            None if self.lazy => Some(self.lazy_hit(id)),
            None => self.sparse.binary_search_by_key(&id, |e| e.0).ok().map(|k| self.sparse[k].1),
        }
    }

    /// The most a hit's score can gain from name and folder matches: the
    /// best own-name score plus, per positive token the name may lack, the
    /// best folder contribution (`memo.best` is `best * 3/4`, `6` past the
    /// tracked four). Used to bound what an entry's score could be.
    fn bound(&self, npos: usize) -> i32 {
        let mut b = self.hi_score as i32;
        for t in 0..npos.min(4) {
            b += self.hi_best[t] as i32 * 3 / 4;
        }
        b + 6 * npos.saturating_sub(4) as i32
    }

    /// The hit of a name in a lazy table: its `ok_ids` score, or `NONE` for
    /// a name stored without one.
    fn lazy_hit(&self, id: u32) -> NameHit {
        let ok = &self.ok_ids[id as usize / NAME_CHUNK];
        ok.binary_search_by_key(&id, |e| e.1).map_or(NameHit::NONE, |p| NameHit::one(ok[p].0))
    }

    /// Every name in the table, ascending.
    fn iter(&self) -> Box<dyn Iterator<Item = (u32, NameHit)> + '_> {
        if self.dense.is_none() && !self.lazy {
            return Box::new(self.sparse.iter().copied());
        }
        Box::new(self.bits.iter().enumerate().flat_map(move |(w, &b)| {
            let mut b = b;
            std::iter::from_fn(move || {
                (b != 0).then(|| {
                    let id = w as u32 * 64 + b.trailing_zeros();
                    b &= b - 1;
                    (id, self.dense.as_ref().map_or_else(|| self.lazy_hit(id), |d| d[id as usize]))
                })
            })
        }))
    }
}

#[derive(Clone, Copy)]
struct NameHit {
    score: i16,
    /// Which positive tokens the name matched.
    bits: u8,
    flags: u8,
    /// Raw per-token scores; folder contributions clamp these to zero.
    best: [i16; 4],
}

impl NameHit {
    const NONE: NameHit = NameHit { score: 0, bits: 0, flags: 0, best: [0; 4] };

    /// The hit of a name that matched a lone token with this score.
    const fn one(score: i16) -> NameHit {
        NameHit { score, bits: 1, flags: NF_OK, best: [score, 0, 0, 0] }
    }
}

const NF_OK: u8 = 1;
const NF_DOT: u8 = 2;
const NF_NEG: u8 = 8;
const NF_APP: u8 = 4;

#[cfg(test)]
mod cache_tests {
    use super::*;
    use crate::index::Index;
    use crate::walk::{Listing, NONE, RawEnt};

    /// `TokenBits` may only drop names a token can't match, and
    /// `token_score_known` must score the rest exactly as `token_score`.
    #[test]
    fn token_bits_admit_every_match() {
        let mut seed = 0x9E3779B97F4A7C15u64;
        let mut rng = move |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        let alphabet = b"abcdeABCDE ._-1\xc3";
        let mut l = Listing { id: 0, names: Vec::new(), ents: Vec::new() };
        for _ in 0..20_000 {
            let name: Vec<u8> = (0..1 + rng(14)).map(|_| alphabet[rng(alphabet.len() as u64) as usize]).collect();
            l.ents.push(RawEnt { name_off: l.names.len() as u32, name_len: name.len() as u16, kind: KIND_FILE, size: 1, mtime: 0, child: NONE });
            l.names.extend(name);
        }
        let live = Live::new(Index::build(vec![l], 0, 0, b"/"));
        let (idx, mask) = (&live.base, live.base.name_mask());
        let (mut matched, mut typos) = (0, 0);
        for _ in 0..300 {
            let text: String = (0..1 + rng(7)).map(|_| b"abcde ._1"[rng(9) as usize] as char).collect();
            let text = ["", "'", "^"][rng(3) as usize].to_string() + text.trim();
            let Ok(q) = Query::parse(&text, "/") else { continue };
            let Some(t) = q.tokens.first() else { continue };
            let tb = TokenBits::new(&live, t);
            for k in 0..idx.u {
                let (name, m, (fits, clean, typo)) = (idx.uname(k as u32), mask[k], tb.word(k / 64));
                let want = token_score(name, m, t);
                let bit = |x: u64| x >> (k % 64) & 1 != 0;
                if want.is_some() {
                    assert!(bit(fits), "{text:?} dropped {:?}", String::from_utf8_lossy(name));
                    matched += 1;
                    typos += (t.start != 0 && !bit(clean)) as usize;
                }
                if bit(fits) {
                    assert_eq!(token_score_known(name, m, t, bit(clean), bit(typo)), want, "{text:?} {:?}", String::from_utf8_lossy(name));
                }
            }
        }
        assert!(matched > 10_000 && typos > 100, "{matched} {typos}");
    }

    #[test]
    fn carry_preserves_negative_fuzzy_scores() {
        let mut name = b"a".to_vec();
        name.extend([b'x'; 150]);
        name.extend(b"bcd");
        let ent = RawEnt { name_off: 0, name_len: name.len() as u16, kind: KIND_FILE, size: 1, mtime: 0, child: NONE };
        let live = Live::new(Index::build(vec![Listing { id: 0, names: name, ents: vec![ent] }], 0, 0, b"/"));
        let s = Searcher { live: &live };
        s.search(&Query::parse("ab c", "/").unwrap());
        let q = Query::parse("ab d", "/").unwrap();
        let cached = s.search(&q);
        *live.names_cache.0.lock().unwrap() = None;
        let fresh = s.search(&q);
        assert_eq!(fresh.len(), 1);
        assert_eq!(cached.iter().map(|h| (h.idx, h.score)).collect::<Vec<_>>(), fresh.iter().map(|h| (h.idx, h.score)).collect::<Vec<_>>());
        assert!(fresh[0].score < 0);
    }

    #[test]
    fn carry_preserves_negative_single_character_scores() {
        let mut name = vec![b'x'; 150];
        name.extend(b"acd");
        let ent = RawEnt { name_off: 0, name_len: name.len() as u16, kind: KIND_FILE, size: 1, mtime: 0, child: NONE };
        let live = Live::new(Index::build(vec![Listing { id: 0, names: name, ents: vec![ent] }], 0, 0, b"/"));
        let s = Searcher { live: &live };
        s.search(&Query::parse("a", "/").unwrap());
        let q = Query::parse("a d", "/").unwrap();
        let cached = s.search(&q);
        *live.names_cache.0.lock().unwrap() = None;
        let fresh = s.search(&q);
        assert_eq!(fresh.len(), 1);
        assert_eq!(cached[0].score, fresh[0].score);
        assert!(fresh[0].score < 0);
    }

    #[test]
    fn floor_pruned_cache_respects_entry_filters_and_larger_pages() {
        let mut root = Listing { id: 0, names: Vec::new(), ents: Vec::new() };
        let mut dirs = Vec::new();
        for id in 1..=100 {
            let name = format!("a{id:03}.app");
            root.ents.push(RawEnt { name_off: root.names.len() as u32, name_len: name.len() as u16, kind: KIND_DIR, size: 1, mtime: 0, child: id });
            root.names.extend(name.as_bytes());
            dirs.push(Listing { id, names: Vec::new(), ents: Vec::new() });
        }
        root.ents.push(RawEnt { name_off: root.names.len() as u32, name_len: 7, kind: KIND_FILE, size: 123, mtime: 0, child: NONE });
        root.names.extend(b"zza.txt");
        dirs.push(root);
        let live = Live::new(Index::build(dirs, 0, 0, b"/"));
        let s = Searcher { live: &live };
        for query in ["kind:file a", "limit:120 a", "limit:10 a", "size:>2 a", "path:zza a"] {
            *live.names_cache.0.lock().unwrap() = None;
            s.search(&Query::parse("a", "/").unwrap());
            let q = Query::parse(query, "/").unwrap();
            let cached = s.search(&q);
            *live.names_cache.0.lock().unwrap() = None;
            let fresh = s.search(&q);
            assert!(!fresh.is_empty(), "{query}");
            assert_eq!(
                cached.iter().map(|h| (h.idx, h.score)).collect::<Vec<_>>(),
                fresh.iter().map(|h| (h.idx, h.score)).collect::<Vec<_>>(),
                "{query}",
            );
        }
    }

    #[test]
    fn packed_dot_scores_match_generic_for_leading_dots() {
        let mut root = Listing { id: 0, names: Vec::new(), ents: Vec::new() };
        for name in [".", "..", "...", "..abc", ".abc", ".abc.", "foo..abc"] {
            root.ents.push(RawEnt {
                name_off: root.names.len() as u32,
                name_len: name.len() as u16,
                kind: KIND_FILE,
                size: 1,
                mtime: 0,
                child: NONE,
            });
            root.names.extend(name.as_bytes());
        }
        let live = Live::new(Index::build(vec![root], 0, 0, b"/"));
        let s = Searcher { live: &live };
        let packed = s.search(&Query::parse(".", "/").unwrap());
        *live.names_cache.0.lock().unwrap() = None;
        let generic = s.search(&Query::parse("kind:file .", "/").unwrap());
        assert_eq!(packed.len(), 7);
        assert_eq!(packed.iter().map(|h| (h.idx, h.score)).collect::<Vec<_>>(), generic.iter().map(|h| (h.idx, h.score)).collect::<Vec<_>>());
    }

    /// Queries typed a character at a time (each answered from the previous
    /// name table) must equal a cold search of each prefix. Big enough that
    /// one-character tables are lazy (no dense table) and feed later tokens.
    #[test]
    fn typed_chains_match_cold_searches() {
        let mut seed = 0x2545F4914F6CDD1Du64;
        let mut rng = move |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        let mut root = Listing { id: 0, names: Vec::new(), ents: Vec::new() };
        let mut dirs = Vec::new();
        let push = |l: &mut Listing, name: &str, kind: u8, child: u32, mtime: u32| {
            l.ents.push(RawEnt { name_off: l.names.len() as u32, name_len: name.len() as u16, kind, size: 100, mtime, child });
            l.names.extend(name.as_bytes());
        };
        for d in 1..=150u32 {
            push(&mut root, &format!("{}{d}", ["src", "docs", "node_modules", "tmp"][d as usize % 4]), KIND_DIR, d, 0);
            let mut l = Listing { id: d, names: Vec::new(), ents: Vec::new() };
            for _ in 0..1000 {
                let len = 3 + rng(8) as usize;
                let alphabet = b"abcdefghijABCDEFGHIJ_-. 0123";
                let mut name: String = (0..len).map(|_| alphabet[rng(alphabet.len() as u64) as usize] as char).collect();
                if rng(3) == 0 {
                    name += [".rs", ".md", ".txt"][rng(3) as usize];
                }
                push(&mut l, &name, KIND_FILE, NONE, rng(400) as u32 * 86_400);
            }
            dirs.push(l);
        }
        dirs.push(root);
        let live = Live::new(Index::build(dirs, 0, 0, b"/"));
        let s = Searcher { live: &live };
        let tok = |rng: &mut dyn FnMut(u64) -> u64| -> String {
            let t: String = (0..1 + rng(4)).map(|_| b"abcdefghij"[rng(10) as usize] as char).collect();
            match rng(8) {
                0 => format!("!{t}"),
                1 => format!("'{t}"),
                2 => format!("^{t}"),
                3 => format!("{t}$"),
                _ => t,
            }
        };
        let flat = |h: Vec<Hit>| h.iter().map(|h| (h.idx, h.score)).collect::<Vec<_>>();
        for _ in 0..40 {
            let text = (0..1 + rng(3)).map(|_| tok(&mut rng)).collect::<Vec<_>>().join(" ");
            let prefixes: Vec<&str> = text.char_indices().map(|(i, c)| &text[..i + c.len_utf8()]).collect();
            *live.names_cache.0.lock().unwrap() = None;
            let cached: Vec<_> = prefixes.iter().map(|p| flat(s.search(&Query::parse(p, "/").unwrap()))).collect();
            for (p, c) in prefixes.iter().zip(cached) {
                *live.names_cache.0.lock().unwrap() = None;
                assert_eq!(c, flat(s.search(&Query::parse(p, "/").unwrap())), "{p:?} of {text:?}");
            }
        }
    }

    /// Multi-token tables score only an earlier token's cover; on a nested
    /// tree whose dir names the tokens hit at every depth, typed chains must
    /// answer exactly like cold searches with every cover marked too broad.
    #[test]
    fn cover_restricted_chains_match_unrestricted() {
        let mut seed = 0x6A09E667F3BCC909u64;
        let mut rng = move |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        let push = |l: &mut Listing, name: &str, kind: u8, child: u32| {
            l.ents.push(RawEnt { name_off: l.names.len() as u32, name_len: name.len() as u16, kind, size: 100, mtime: 0, child });
            l.names.extend(name.as_bytes());
        };
        let (mut lists, mut next, mut stack) = (Vec::new(), 1u32, vec![(0u32, 0u64)]);
        while let Some((id, depth)) = stack.pop() {
            let mut l = Listing { id, names: Vec::new(), ents: Vec::new() };
            for _ in 0..if depth == 0 { 12 } else { 3 + rng(20) } {
                let name: String = (0..2 + rng(6)).map(|_| b"abcdefgh_.AB"[rng(12) as usize] as char).collect();
                if depth < 4 && rng(4) == 0 {
                    push(&mut l, &name, KIND_DIR, next);
                    stack.push((next, depth + 1));
                    next += 1;
                } else {
                    push(&mut l, &name, KIND_FILE, NONE);
                }
            }
            lists.push(l);
        }
        let live = Live::new(Index::build(lists, 0, 0, b"/"));
        let s = Searcher { live: &live };
        let flat = |h: Vec<Hit>| h.iter().map(|h| (h.idx, h.score)).collect::<Vec<_>>();
        let mut covered = 0;
        // Second half: no selective pass, so restricted tables take the
        // cover scan (and the reference the full pass).
        for round in 0..120 {
            FULL_PASS.store(round >= 60, std::sync::atomic::Ordering::Relaxed);
            let text = (0..2 + rng(2))
                .map(|_| {
                    let t: String = (0..1 + rng(3)).map(|_| b"abcdefgh"[rng(8) as usize] as char).collect();
                    if rng(6) == 0 { format!("!{t}") } else { t }
                })
                .collect::<Vec<_>>()
                .join(" ");
            // Typed, then edited: the first token shortened, then dropped
            // (the cover's token leaves the query; carry must still hold).
            let mut prefixes: Vec<String> = text.char_indices().map(|(i, c)| text[..i + c.len_utf8()].to_string()).collect();
            let (first, rest) = text.split_once(' ').unwrap();
            if first.len() > 1 {
                prefixes.push(format!("{} {rest}", &first[..first.len() - 1]));
            }
            prefixes.push(rest.to_string());
            *live.names_cache.0.lock().unwrap() = None;
            let cached: Vec<_> = prefixes.iter().map(|p| flat(s.search(&Query::parse(p, "/").unwrap()))).collect();
            covered += live.cover_cache.0.lock().unwrap().iter().filter(|c| c.names.is_some()).count();
            for (p, c) in prefixes.iter().zip(cached) {
                let q = Query::parse(p, "/").unwrap();
                *live.names_cache.0.lock().unwrap() = None;
                *live.cover_cache.0.lock().unwrap() = q
                    .tokens
                    .iter()
                    .filter(|t| !t.negate)
                    .map(|t| std::sync::Arc::new(Cover { text: t.text.clone(), mode: t.mode, names: None }))
                    .collect();
                assert_eq!(c, flat(s.search(&q)), "{p:?} of {text:?}");
            }
            live.cover_cache.0.lock().unwrap().clear();
        }
        FULL_PASS.store(false, std::sync::atomic::Ordering::Relaxed);
        assert!(covered + DRIVEN_QUERIES.load(std::sync::atomic::Ordering::Relaxed) > 40, "covers built: {covered}");
    }

    /// Token-less stat filters skip runs of the stat order by their ranges;
    /// the answer must equal checking every entry. Sizes, ages and kinds
    /// cluster by folder, so many runs are skippable and some are not.
    #[test]
    fn stat_filter_walk_matches_brute_force() {
        let mut seed = 0x9E3779B97F4A7C15u64;
        let mut rng = move |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        let now = now_secs();
        let mut root = Listing { id: 0, names: Vec::new(), ents: Vec::new() };
        let mut dirs = Vec::new();
        for d in 1..=120u32 {
            root.ents.push(RawEnt { name_off: root.names.len() as u32, name_len: 4, kind: KIND_DIR, size: 0, mtime: now, child: d });
            root.names.extend(format!("d{d:03}").as_bytes());
            let mut l = Listing { id: d, names: Vec::new(), ents: Vec::new() };
            let (big, old) = (d % 7 == 0, d % 5 == 0);
            for f in 0..1500 {
                let name = format!("f{f}.bin");
                let size = if big { 1_000_000 + rng(1_000_000) } else { rng(5_000) };
                let mtime = now - if old { 400 * 86_400 } else { rng(30) as u32 * 86_400 };
                let kind = if f % 97 == 0 { KIND_LINK } else { KIND_FILE };
                l.ents.push(RawEnt { name_off: l.names.len() as u32, name_len: name.len() as u16, kind, size, mtime, child: NONE });
                l.names.extend(name.as_bytes());
            }
            dirs.push(l);
        }
        dirs.push(root);
        let live = Live::new(Index::build(dirs, 0, 0, b"/"));
        let (s, idx) = (Searcher { live: &live }, &live.base);
        let mut p = Vec::new();
        for text in [
            "size:>1500000",
            "size:>9gb",
            "size:<10",
            "size:100..120",
            "mtime:>300d",
            "mtime:<2d size:>1mb",
            "kind:dir",
            "kind:link size:>1mb",
            "limit:900 size:>1mb",
            // Small scopes, visited in place.
            "in:/d014",
            "in:/d014 size:>1500000",
            "in:/d030 !f1",
            "in:/d035 kind:link",
            "in:/d007 limit:2000",
        ] {
            let q = Query::parse(text, "/").unwrap();
            let got: Vec<(u32, i32)> = s.search(&q).iter().map(|h| (h.idx, h.score)).collect();
            let mut want: Vec<(u32, i32)> = (1..idx.n)
                .filter_map(|i| {
                    idx.path(i, &mut p);
                    q.match_path(&p, idx.kind()[i], idx.size_of(i), idx.mtime()[i]).map(|_| (i as u32, live.stat[i]))
                })
                .collect();
            want.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            want.truncate(q.limit);
            assert_eq!(got, want, "{text}");
        }
    }
}

pub(crate) fn name_flags(name: &[u8]) -> u8 {
    let mut f = 0;
    if name.first() == Some(&b'.') {
        f |= NF_DOT;
    }
    if name.ends_with(b".app") {
        f |= NF_APP;
    }
    f
}

/// Small per-entry nudges on top of match quality and the location prior.
#[inline]
pub(crate) fn rank_tweaks(flags: u8, kind: u8, mtime: u32, now: u32) -> i32 {
    let mut s = 0;
    if flags & NF_DOT != 0 {
        s -= 8;
    }
    if kind & FLAG_HIDDEN != 0 {
        s -= 8;
    }
    // Apps are dirs, or symlinks into the cryptex (/Applications/Safari.app).
    if matches!(kind & 3, KIND_DIR | KIND_LINK) && flags & NF_APP != 0 {
        s += 25;
    }
    let age = now.saturating_sub(mtime);
    s += match age {
        0..=86_400 => 10,
        86_401..=604_800 => 7,
        604_801..=2_592_000 => 4,
        2_592_001..=31_536_000 => 1,
        _ => 0,
    };
    s
}

#[cfg(all(test, any(target_arch = "aarch64", all(target_arch = "x86_64", target_feature = "avx2"))))]
mod mask64_tests {
    use super::*;

    /// Verbatim copy of the pre-mask memchr implementation (reference).
    fn fuzzy_ref(name: &[u8], q: &[u8], cap: i32) -> Option<i32> {
        let mut end = 0;
        let mut from = 0;
        for &c in q {
            end = from + find_folded(&name[from..], c)?;
            from = end + 1;
        }
        if q.len() == 1 {
            return Some(single_score(name, end, cap));
        }
        let mut start = end + 1;
        for &c in q.iter().rev() {
            start = rfind_folded(&name[..start], c)?;
        }
        let mut score = 0;
        let (mut at, mut first_bonus) = (start, 0);
        for (k, &c) in q.iter().enumerate() {
            let mut run = false;
            if k > 0 {
                let last = at;
                at = last + 1 + find_folded(&name[last + 1..=end], c)?;
                run = at == last + 1;
                if !run {
                    score += GAP_START + (at - last - 2) as i32 * GAP_EXT;
                }
            }
            let prev = if at == 0 { Class::Delim } else { class(name[at - 1]) };
            let mut b = bonus(prev, class(name[at]));
            if run {
                if b >= BONUS_BOUNDARY && b > first_bonus {
                    first_bonus = b;
                }
                b = b.max(first_bonus).max(BONUS_CONSEC);
            } else {
                first_bonus = b;
            }
            score += SCORE_MATCH + if k == 0 { b * 2 } else { b };
        }
        let off = (name.len() > 1 && name[0] == b'.') as usize;
        let stem = memchr::memrchr(b'.', name).filter(|&p| p > off).unwrap_or(name.len());
        let contiguous = end + 1 - start == q.len();
        let placed = if start == off && contiguous && end + 1 == name.len() {
            100
        } else if start == off && contiguous && end + 1 == stem {
            80
        } else if start == off && contiguous {
            30
        } else {
            0
        };
        Some(score + placed.min(cap) - (name.len() as i32).min(80) / 3)
    }

    #[test]
    fn mask64_matches_memchr_reference() {
        // Deterministic xorshift corpus: adversarial name/query shapes.
        let alphabet: &[u8] =
            b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789 .-_/()[]+,@!#$%^&*=;:'\"~`{}\xc3\xa9\xe4\xb8\xad\xe6\x96\x87";
        let mut seed = 0x9E3779B97F4A7C15u64;
        let mut rng = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let mut cases = 0u64;
        for i in 0..200_000u64 {
            let nl = (rng() % 140) as usize;
            // Random bytes follow the name: in-place vector loads see them.
            let buf: Vec<u8> = (0..nl + 128).map(|_| alphabet[(rng() as usize) % alphabet.len()]).collect();
            let name = &buf[..nl];
            let ql = 1 + (rng() % 18) as usize;
            // Bias queries toward characters present in the name so both
            // match and non-match paths are exercised.
            let q: Vec<u8> = (0..ql)
                .map(|_| {
                    if !name.is_empty() && rng() % 3 != 0 {
                        fold(name[(rng() as usize) % name.len()])
                    } else {
                        fold(alphabet[(rng() as usize) % alphabet.len()])
                    }
                })
                .collect();
            for cap in [30, 100] {
                let a = fuzzy_score_capped(&name, &q, cap);
                let b = fuzzy_ref(&name, &q, cap);
                assert_eq!(a, b, "iter {i} name={name:?} q={q:?} cap={cap}");
            }
            cases += 1;
        }
        // Structured edges: lengths at the mask boundary, repeats, dots.
        for nl in [0usize, 1, 2, 63, 64, 65, 128] {
            for fill in [b'a', b'.', b'Z'] {
                let name = vec![fill; nl];
                for q in [&b"a"[..], b"aa", b"zz", b"a.a", b"................................................................"] {
                    for cap in [30, 100] {
                        assert_eq!(fuzzy_score_capped(&name, q, cap), fuzzy_ref(&name, q, cap), "nl={nl} fill={fill} q={q:?} cap={cap}");
                    }
                }
            }
        }
        // A name of repeats so every char matches at many positions.
        let name = b"ababababababababababababababababababababababababababababababab".to_vec();
        for q in [&b"ab"[..], b"aba", b"bab", b"ababababababababababababababababababababababababababababababa"] {
            for cap in [30, 100] {
                assert_eq!(fuzzy_score_capped(&name, q, cap), fuzzy_ref(&name, q, cap), "q={q:?}");
            }
        }
        assert_eq!(cases, 200_000);
    }

    #[test]
    fn below_and_floor_score() {
        let mut seed = 0x2545F4914F6CDD1Du64;
        for _ in 0..20_000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let len = if seed % 4 == 0 { (seed >> 8) as usize % 65 } else { 64 };
            let v: Vec<i16> = (0..len).map(|j| (seed.rotate_left(j as u32 * 5) as i16) >> (seed % 9)).collect();
            for t in
                [i32::MIN, i16::MIN as i32, i16::MIN as i32 + 1, -1, 0, 7, (seed >> 20) as i16 as i32, i16::MAX as i32, i16::MAX as i32 + 1, i32::MAX]
            {
                let want = v.iter().enumerate().fold(0u64, |m, (j, &x)| m | ((((x as i32) < t) as u64) << j));
                assert_eq!(below(&v, t), want, "t={t} v={v:?}");
            }
        }
        for s in [i32::MIN, -5, 0, 300, i32::MAX] {
            for i in [0u32, 1, 77, u32::MAX] {
                assert_eq!(floor_score(key(s, i)), s);
            }
        }
    }

    #[test]
    fn mask64_boundary_edges() {
        // Empty query: must take the generic path verbatim (the mask loop's
        // 0..ql-1 range would underflow); reachable via typo fixed="" and
        // empty fuzzy tokens.
        for name in [&b""[..], b"a", b".x", &vec![b'z'; 64][..], b"abcdef"] {
            for cap in [30, 100] {
                assert_eq!(fuzzy_score_capped(name, b"", cap), fuzzy_ref(name, b"", cap), "name={name:?} q=empty cap={cap}");
            }
        }
        // Exact 64-byte name and query on both sides of the gate.
        let n64: Vec<u8> = (0..64).map(|i| b'a' + (i % 26) as u8).collect();
        let n65 = [&n64[..], b"z"].concat();
        let q64: Vec<u8> = (0..64).map(|i| b'a' + (i % 26) as u8).collect();
        let q65 = [&q64[..], b"y"].concat();
        for (name, q) in [
            (n64.as_slice(), q64.as_slice()),
            (n65.as_slice(), q64.as_slice()), // name just over -> generic
            (n64.as_slice(), q65.as_slice()), // query just over -> generic
            (n64.as_slice(), &q64[..1]),
            (n64.as_slice(), &q64[..63]),
            (&n64[..1], q64.as_slice()),
        ] {
            for cap in [30, 100] {
                assert_eq!(fuzzy_score_capped(name, q, cap), fuzzy_ref(name, q, cap), "nl={} ql={}", name.len(), q.len());
            }
        }
        // Last-byte-only hit: match possible solely at position 63.
        let mut n = vec![b'b'; 64];
        n[63] = b'a';
        for q in [&b"a"[..], b"ba", b"bba", b"bbba"] {
            for cap in [30, 100] {
                assert_eq!(fuzzy_score_capped(&n, q, cap), fuzzy_ref(&n, q, cap), "lastbyte q={q:?}");
            }
        }
        // Empty and 1-byte names; 1-byte queries; NUL and UTF-8 stress.
        for name in [&b""[..], b"a", b".", b"\x00x", b"a\x00b", &vec![0u8; 64][..], "héllo".as_bytes(), "".as_bytes(), &vec![0xC3u8; 64][..]] {
            for q in [&b"a"[..], b"", b"\x00", b"x\x00", "".as_bytes()] {
                for cap in [30, 100] {
                    assert_eq!(fuzzy_score_capped(name, q, cap), fuzzy_ref(name, q, cap), "name={name:?} q={q:?}");
                }
            }
        }
        // Query longer than name can never match: 64-char q on short name.
        for name in [&b"ab"[..], b"a"] {
            for cap in [30, 100] {
                assert_eq!(fuzzy_score_capped(name, &q64, cap), fuzzy_ref(name, &q64, cap));
            }
        }
    }
}

#[cfg(test)]
mod sparse_narrow_tests {
    use super::*;
    use crate::index::Index;
    use crate::walk::{Listing, NONE, RawEnt};

    /// A narrowed keystroke over a sparse previous table must score exactly
    /// the names a cold search scores — the rescan's word-level fit union is
    /// a superset of `Token::fits`, never a filter on the answer.
    #[test]
    fn typo_narrowed_chain_matches_cold() {
        let mut root = Listing { id: 0, names: Vec::new(), ents: Vec::new() };
        let push = |l: &mut Listing, name: &str| {
            l.ents.push(RawEnt { name_off: l.names.len() as u32, name_len: name.len() as u16, kind: KIND_FILE, size: 1, mtime: 0, child: NONE });
            l.names.extend(name.as_bytes());
        };
        // Filler without any of {., g, i, t} classes keeps the ".giti"
        // table sparse.
        let mut seed = 0xC0FFEEu64;
        let mut rng = move |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        for _ in 0..3000 {
            let len = 4 + rng(8) as usize;
            let name: String = (0..len).map(|_| b"aeouwxyz_-"[rng(10) as usize] as char).collect();
            push(&mut root, &name);
        }
        // Names a ".giti*"/typo chain may or may not keep.
        for name in [
            ".gitignore", ".gitmodules", ".git", "gitignore", ".gignore", ".gtignore", ".gitignoe",
            ".gxtignore", "a.gitig b", ".gitig", "x.gitignore", ".itignore", ".gtigore",
        ] {
            push(&mut root, name);
        }
        let live = Live::new(Index::build(vec![root], 0, 0, b"/"));
        let s = Searcher { live: &live };
        let flat = |h: Vec<Hit>| h.iter().map(|h| (h.idx, h.score)).collect::<Vec<_>>();
        for q in [".giti", ".gitig", ".gitign", ".gitigno", ".gitignor", ".gitignore"] {
            let cached = flat(s.search(&Query::parse(q, "/").unwrap()));
            *live.names_cache.0.lock().unwrap() = None;
            let fresh = flat(s.search(&Query::parse(q, "/").unwrap()));
            assert_eq!(cached, fresh, "{q:?}");
        }
    }
}
