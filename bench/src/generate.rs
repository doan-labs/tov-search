//! The synthetic disk: a developer's Mac, about 10M entries, shaped after a
//! real 12M-entry disk (see `bench stats`): most of it node_modules, package
//! caches, app and framework bundles, Library folders and temp clutter, with
//! the user's own documents a thin layer on top.
//!
//! Deterministic everywhere: one seeded integer PRNG, no floats, no hash-map
//! iteration order. `bench/DATASET` pins the result.

use crate::disk::{KIND_DIR, KIND_FILE, KIND_LINK, Writer};
use std::collections::HashSet;

/// The pinned clock (2026-09-21): ages are relative to it.
const NOW: u32 = 1_790_000_000;
/// npm writes every packed file with this mtime (1985-10-26).
const NPM_EPOCH: u32 = 499_162_500;
const DAY: u32 = 86_400;
/// Entries in the whole disk (approximate: regions stop at their budget).
const TOTAL: u32 = 10_000_000;

/// splitmix64 in a Cell: drawing a number needs only `&self`, so it can sit
/// inside the arguments of a call that holds the generator mutably.
struct Rng(std::cell::Cell<u64>);

impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(std::cell::Cell::new(seed ^ 0x6a09_e667_f3bc_c909))
    }
    fn next(&self) -> u64 {
        let s = self.0.get().wrapping_add(0x9e37_79b9_7f4a_7c15);
        self.0.set(s);
        let mut z = s;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    fn below(&self, n: u64) -> u64 {
        if n == 0 { 0 } else { ((self.next() as u128 * n as u128) >> 64) as u64 }
    }
    /// lo..=hi
    fn range(&self, lo: u64, hi: u64) -> u64 {
        lo + self.below(hi - lo + 1)
    }
    /// True with probability `per_mille`/1000.
    fn odds(&self, per_mille: u64) -> bool {
        self.below(1000) < per_mille
    }
    fn pick<'a, T: ?Sized>(&self, v: &'a [&'a T]) -> &'a T {
        v[self.below(v.len() as u64) as usize]
    }
    /// lo..=hi, heavily skewed toward lo (most packages are small, a few huge).
    fn skew(&self, lo: u64, hi: u64) -> u64 {
        let a = self.below(hi - lo + 1);
        let b = self.below(a + 1);
        lo + self.below(b + 1)
    }
    fn hex(&self, len: usize) -> String {
        (0..len).map(|_| b"0123456789abcdef"[self.below(16) as usize] as char).collect()
    }
    fn alnum(&self, len: usize) -> String {
        (0..len).map(|_| b"abcdefghijklmnopqrstuvwxyz0123456789"[self.below(36) as usize] as char).collect()
    }
    fn uuid(&self) -> String {
        let h = self.hex(32).to_uppercase();
        format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
    }
    /// An mtime: mostly old, some fresh (ranking rewards recent files).
    fn mtime(&self) -> u32 {
        let age = match self.below(1000) {
            0..20 => self.below(DAY as u64),
            20..60 => self.range(DAY as u64, 7 * DAY as u64),
            60..150 => self.range(7 * DAY as u64, 30 * DAY as u64),
            150..500 => self.range(30 * DAY as u64, 365 * DAY as u64),
            _ => self.range(365 * DAY as u64, 6 * 365 * DAY as u64),
        };
        NOW - age as u32
    }
    /// A size of 2^lo .. 2^(hi+1) bytes, log-uniform.
    fn size(&self, lo: u32, hi: u32) -> u64 {
        let e = self.range(lo as u64, hi as u64);
        (1u64 << e) + self.below(1 << e)
    }
}

fn hash(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3))
}

fn words(s: &'static str) -> Vec<&'static str> {
    s.split_whitespace().collect()
}

/// A directory being filled: names must be unique inside it.
struct Dir {
    id: u32,
    seen: HashSet<Vec<u8>>,
}

struct Vocab {
    words: Vec<&'static str>,
    pkgs: Vec<&'static str>,
    scopes: Vec<&'static str>,
    locales: Vec<&'static str>,
    fw_pre: Vec<&'static str>,
    fw_suf: Vec<&'static str>,
    apps: Vec<&'static str>,
    electron: Vec<&'static str>,
    crates: Vec<&'static str>,
    pys: Vec<&'static str>,
    brews: Vec<&'static str>,
    tools: Vec<&'static str>,
    orgs: Vec<&'static str>,
}

struct G {
    w: Writer,
    r: Rng,
    v: Vocab,
    /// Entry count at the last `more` call, and how many calls since it grew.
    last: std::cell::Cell<(u32, u32)>,
}

/// Case styles for code names.
#[derive(Clone, Copy)]
enum Style {
    Camel,
    Pascal,
    Kebab,
    Snake,
}

impl G {
    fn root(&mut self) -> Dir {
        Dir { id: 0, seen: HashSet::new() }
    }

    fn dir(&mut self, p: &mut Dir, name: &str) -> Option<Dir> {
        let m = self.r.mtime();
        self.dir_at(p, name, m)
    }

    fn dir_at(&mut self, p: &mut Dir, name: &str, mtime: u32) -> Option<Dir> {
        if !p.seen.insert(name.as_bytes().to_vec()) {
            return None;
        }
        let id = self.w.push(p.id, name.as_bytes(), KIND_DIR, 0, mtime);
        Some(Dir { id, seen: HashSet::new() })
    }

    /// `p/name`, creating it if new; a fresh handle to an existing one would
    /// lose its name set, so callers keep handles they need again.
    fn must(&mut self, p: &mut Dir, name: &str) -> Dir {
        self.dir(p, name).unwrap_or_else(|| panic!("duplicate dir {name}"))
    }

    fn file(&mut self, p: &mut Dir, name: &str, size: u64, mtime: u32) -> bool {
        if !p.seen.insert(name.as_bytes().to_vec()) {
            return false;
        }
        self.w.push(p.id, name.as_bytes(), KIND_FILE, size, mtime);
        true
    }

    fn link(&mut self, p: &mut Dir, name: &str, mtime: u32) {
        if p.seen.insert(name.as_bytes().to_vec()) {
            self.w.push(p.id, name.as_bytes(), KIND_LINK, 0, mtime);
        }
    }

    fn n(&self) -> u32 {
        self.w.len()
    }

    /// Condition for "fill until the disk has `until` entries". Panics when a
    /// loop stops adding entries (its name space ran out) instead of spinning.
    fn more(&self, until: u32) -> bool {
        let n = self.n();
        let (last, stalls) = self.last.get();
        let stalls = if n == last { stalls + 1 } else { 0 };
        assert!(stalls < 100_000, "generator stalled at {n} entries");
        self.last.set((n, stalls));
        n < until
    }

    /// Size for a file by its extension.
    fn size_for(&self, name: &str) -> u64 {
        let ext = name.rsplit_once('.').map_or("", |e| e.1);
        let (lo, hi) = match ext {
            "png" | "jpg" | "jpeg" | "heic" | "HEIC" | "webp" | "gif" => (14, 22),
            "mov" | "mp4" | "m4v" => (22, 31),
            "zip" | "dmg" | "pkg" | "gz" | "tgz" | "pak" | "asar" => (16, 28),
            "pdf" | "docx" | "xlsx" | "pptx" | "key" | "pages" => (14, 23),
            "map" | "rlib" | "dylib" | "o" | "a" | "node" => (12, 22),
            "plist" | "strings" | "json" | "yml" | "yaml" | "toml" | "env" | "lock" => (6, 13),
            "nib" | "car" | "icns" => (10, 20),
            _ => (8, 16),
        };
        self.r.size(lo, hi)
    }

    fn word(&self) -> &'static str {
        let w = &self.v.words;
        w[self.r.below(w.len() as u64) as usize]
    }

    /// One to three words joined in a code style: "parseDate", "user-list".
    fn ident(&self, style: Style) -> String {
        let k = self.r.range(1, 3) as usize;
        let parts: Vec<&str> = (0..k).map(|_| self.word()).collect();
        let cap = |s: &str| s[..1].to_uppercase() + &s[1..];
        match style {
            Style::Camel => {
                parts.iter().enumerate().map(|(i, p)| if i == 0 { p.to_string() } else { cap(p) }).collect()
            }
            Style::Pascal => parts.iter().map(|p| cap(p)).collect(),
            Style::Kebab => parts.join("-"),
            Style::Snake => parts.join("_"),
        }
    }

    fn version(&self) -> String {
        let major = self.r.skew(0, 30);
        format!("{}.{}.{}", major, self.r.below(40), self.r.below(30))
    }
}

pub fn disk() -> Vec<u8> {
    let mut g = G {
        w: Writer::new(),
        r: Rng::new(0x5eed),
        v: Vocab {
            words: words(WORDS),
            pkgs: words(PKGS),
            scopes: words(SCOPES),
            locales: words(LOCALES),
            fw_pre: words(FW_PRE),
            fw_suf: words(FW_SUF),
            apps: APPS.split(',').map(str::trim).collect(),
            electron: ELECTRON.split(',').map(str::trim).collect(),
            crates: words(CRATES),
            pys: words(PYS),
            brews: words(BREWS),
            tools: words(TOOLS),
            orgs: words(ORGS),
        },
        last: Default::default(),
    };
    let mut root = g.root();
    let budget = |pct: u32| TOTAL / 1000 * pct;

    // Order matters only for entry numbering; budgets are cumulative targets.
    let mut sys = g.must(&mut root, "System");
    system(&mut g, &mut sys, budget(35));
    let mut apps = g.must(&mut root, "Applications");
    applications(&mut g, &mut apps, budget(65));
    let mut lib = g.must(&mut root, "Library");
    library_root(&mut g, &mut lib, budget(85));
    let mut opt = g.must(&mut root, "opt");
    homebrew(&mut g, &mut opt, budget(105));
    let mut usr = g.must(&mut root, "usr");
    usr_tree(&mut g, &mut usr);
    for d in ["bin", "sbin", "cores", "dev", "Volumes"] {
        let mut x = g.must(&mut root, d);
        if d == "bin" || d == "sbin" {
            for t in 0..40 {
                let name = format!("{}{}", g.r.pick(&g.v.tools.clone()), if t % 3 == 0 { "" } else { "d" });
                let s = g.r.size(14, 20);
                g.file(&mut x, &name, s, NOW - 200 * DAY);
            }
        }
    }
    for f in [".VolumeIcon.icns", ".file"] {
        g.file(&mut root, f, 4096, NOW - 400 * DAY);
    }
    let mut private = g.must(&mut root, "private");
    private_tree(&mut g, &mut private, budget(355));
    let mut users = g.must(&mut root, "Users");
    let mut shared = g.must(&mut users, "Shared");
    g.file(&mut shared, ".localized", 0, NOW - 900 * DAY);
    let mut home = g.must(&mut users, "bench");
    home_tree(&mut g, &mut home);
    let start = 0;
    let _ = start;
    g.w.finish(NOW)
}

// ---------------------------------------------------------------- bundles

/// A framework bundle: Versions/A/{binary, Resources/*.lproj, Headers, ...}.
fn framework(g: &mut G, p: &mut Dir, name: &str, locales: usize, headers: usize) {
    let Some(mut fw) = g.dir(p, &format!("{name}.framework")) else { return };
    let m = fw_mtime(g);
    let mut vers = g.must(&mut fw, "Versions");
    let mut a = g.must(&mut vers, "A");
    g.file(&mut a, name, g.r.size(16, 24), m);
    let mut res = g.must(&mut a, "Resources");
    g.file(&mut res, "Info.plist", 1500, m);
    g.file(&mut res, "version.plist", 500, m);
    lprojs(g, &mut res, locales, m);
    if headers > 0 {
        let mut h = g.must(&mut a, "Headers");
        for _ in 0..headers {
            let n = format!("{}{}.h", name, g.ident(Style::Pascal));
            g.file(&mut h, &n, 3000, m);
        }
        let mut mods = g.must(&mut a, "Modules");
        g.file(&mut mods, "module.modulemap", 200, m);
    }
    let mut cs = g.must(&mut a, "_CodeSignature");
    g.file(&mut cs, "CodeResources", 9000, m);
    g.link(&mut vers, "Current", m);
    for l in [name, "Resources"] {
        g.link(&mut fw, l, m);
    }
    if headers > 0 {
        g.link(&mut fw, "Headers", m);
        g.link(&mut fw, "Modules", m);
    }
}

fn fw_mtime(g: &mut G) -> u32 {
    NOW - g.r.range(30, 400) as u32 * DAY
}

/// Localized resource folders, each with a few .strings / .nib files.
fn lprojs(g: &mut G, res: &mut Dir, n: usize, m: u32) {
    let files = g.r.range(1, 6) as usize;
    let names: Vec<String> = (0..files)
        .map(|k| if k == 0 { "Localizable.strings".to_string() } else { format!("{}.strings", g.ident(Style::Pascal)) })
        .collect();
    let nibs = g.r.below(3) as usize;
    let nib_names: Vec<String> = (0..nibs).map(|_| format!("{}.nib", g.ident(Style::Pascal))).collect();
    let locs = g.v.locales.clone();
    for loc in locs.iter().take(n) {
        let Some(mut l) = g.dir_at(res, &format!("{loc}.lproj"), m) else { continue };
        for f in names.iter().chain(&nib_names) {
            g.file(&mut l, f, 2000, m);
        }
        if *loc == "en" || *loc == "Base" {
            g.file(&mut l, "InfoPlist.strings", 300, m);
        }
    }
}

/// An app bundle. Electron apps carry Chromium's framework and locale paks.
fn app(g: &mut G, p: &mut Dir, name: &str, electron: bool, size: usize) {
    let Some(mut a) = g.dir(p, &format!("{name}.app")) else { return };
    let m = fw_mtime(g);
    let mut c = g.must(&mut a, "Contents");
    g.file(&mut c, "Info.plist", 4000, m);
    g.file(&mut c, "PkgInfo", 8, m);
    let mut macos = g.must(&mut c, "MacOS");
    g.file(&mut macos, name, g.r.size(20, 26), m);
    let mut res = g.must(&mut c, "Resources");
    g.file(&mut res, "AppIcon.icns", 400_000, m);
    g.file(&mut res, "Assets.car", 900_000, m);
    lprojs(g, &mut res, if electron { 55 } else { 30 }, m);
    for _ in 0..size {
        let n = format!(
            "{}.{}",
            g.ident(Style::Pascal),
            g.r.pick(&["png", "tiff", "plist", "nib", "storyboardc", "json", "js", "css"])
        );
        let s = g.size_for(&n);
        g.file(&mut res, &n, s, m);
    }
    let mut cs = g.must(&mut c, "_CodeSignature");
    g.file(&mut cs, "CodeResources", 50_000, m);
    let mut fws = g.must(&mut c, "Frameworks");
    if electron {
        let mut ef = g.must(&mut fws, "Electron Framework.framework");
        let mut vers = g.must(&mut ef, "Versions");
        let mut va = g.must(&mut vers, "A");
        g.file(&mut va, "Electron Framework", 180_000_000, m);
        let mut r = g.must(&mut va, "Resources");
        for f in [
            "Info.plist",
            "icudtl.dat",
            "resources.pak",
            "chrome_100_percent.pak",
            "chrome_200_percent.pak",
            "v8_context_snapshot.arm64.bin",
        ] {
            let s = g.size_for(f);
            g.file(&mut r, f, s, m);
        }
        let locs = g.v.locales.clone();
        for loc in locs.iter().take(55) {
            if let Some(mut l) = g.dir_at(&mut r, &format!("{loc}.lproj"), m) {
                g.file(&mut l, "locale.pak", 120_000, m);
            }
        }
        g.link(&mut vers, "Current", m);
        for h in ["Helper", "Helper (GPU)", "Helper (Plugin)", "Helper (Renderer)"] {
            let hn = format!("{name} {h}");
            if let Some(mut ha) = g.dir(&mut fws, &format!("{hn}.app")) {
                let mut hc = g.must(&mut ha, "Contents");
                g.file(&mut hc, "Info.plist", 2000, m);
                let mut hm = g.must(&mut hc, "MacOS");
                g.file(&mut hm, &hn, 100_000, m);
            }
        }
        for fw in ["Mantle", "ReactiveObjC", "Squirrel"] {
            framework(g, &mut fws, fw, 1, 0);
        }
        let mut appd = g.must(&mut res, "app.asar.unpacked");
        let mut nm = g.must(&mut appd, "node_modules");
        let k = g.r.range(5, 40);
        for _ in 0..k {
            let p = g.r.pick(&g.v.pkgs.clone()).to_string();
            package(g, &mut nm, &p, None);
        }
        g.file(&mut res, "app.asar", 60_000_000, m);
    } else {
        let k = g.r.range(0, 6);
        for _ in 0..k {
            let n = fw_name(g);
            framework(g, &mut fws, &n, 3, 0);
        }
    }
    if g.r.odds(400) {
        let mut pl = g.must(&mut c, "PlugIns");
        for _ in 0..g.r.range(1, 5) {
            let n = format!("{}{}.appex", name.replace(' ', ""), g.ident(Style::Pascal));
            if let Some(mut x) = g.dir(&mut pl, &n) {
                let mut xc = g.must(&mut x, "Contents");
                g.file(&mut xc, "Info.plist", 2000, m);
            }
        }
    }
}

fn fw_name(g: &mut G) -> String {
    let p = g.r.pick(&g.v.fw_pre.clone()).to_string();
    let s = g.r.pick(&g.v.fw_suf.clone()).to_string();
    // Two parts give only ~4k names; System needs ~15k frameworks.
    let mid = if g.r.odds(600) { g.r.pick(&g.v.fw_pre.clone()).to_string() } else { String::new() };
    format!("{p}{mid}{s}")
}

// ---------------------------------------------------------------- packages

/// An npm package, laid out the same way wherever it is installed (its
/// shape comes from its name), so names repeat across the disk as they do
/// in real node_modules.
fn package(g: &mut G, nm: &mut Dir, name: &str, mtime: Option<u32>) {
    let (scope, base) = match name.split_once('/') {
        Some((s, b)) => (Some(s), b),
        None => (None, name),
    };
    let mut scoped;
    let parent: &mut Dir = match scope {
        Some(s) => {
            // Reuse the scope folder if this node_modules has it already.
            match g.dir(nm, s) {
                Some(d) => {
                    scoped = d;
                    &mut scoped
                }
                None => return, // ponytail: a second package in an existing scope is skipped; scopes stay smaller than real ones
            }
        }
        None => nm,
    };
    let Some(mut d) = g.dir(parent, base) else { return };
    let m = mtime.unwrap_or(NPM_EPOCH);
    // The package's own shape, independent of where it is installed.
    let mut r = Rng::new(hash(name));
    let files = r.skew(4, 2500) as usize;
    std::mem::swap(&mut g.r, &mut r);
    for f in ["package.json", "README.md", "LICENSE"] {
        let s = g.size_for(f);
        g.file(&mut d, f, s, m);
    }
    if g.r.odds(400) {
        g.file(&mut d, "CHANGELOG.md", 20_000, m);
    }
    if g.r.odds(200) {
        g.file(&mut d, ".npmignore", 100, m);
    }
    if g.r.odds(150) {
        g.file(&mut d, "tsconfig.json", 600, m);
    }
    g.file(&mut d, "index.js", 2000, m);
    if g.r.odds(500) {
        g.file(&mut d, "index.d.ts", 3000, m);
    }
    let style = [Style::Camel, Style::Kebab, Style::Snake, Style::Pascal][g.r.below(4) as usize];
    let layout = g.r.below(4);
    let top = ["lib", "dist", "build", "esm", "cjs", "src", "types"];
    let mut dirs: Vec<Dir> = Vec::new();
    let picked: Vec<&str> = top.iter().copied().filter(|_| g.r.odds(350)).take(3).collect();
    for t in picked {
        if let Some(x) = g.dir_at(&mut d, t, m) {
            dirs.push(x);
        }
    }
    if dirs.is_empty() {
        dirs.push(g.dir_at(&mut d, if layout == 0 { "lib" } else { "dist" }, m).unwrap());
    }
    // Sub-folders by feature, each with modules in several flavors.
    let mut placed = 0;
    while placed < files {
        let k = g.r.below(dirs.len() as u64) as usize;
        let mut target = if g.r.odds(250) && dirs.len() < 60 {
            let sub = g.ident(style);
            match g.dir_at(&mut dirs[k], &sub, m) {
                Some(x) => {
                    dirs.push(x);
                    dirs.len() - 1
                }
                None => k,
            }
        } else {
            k
        };
        if dirs[target].seen.len() > 400 {
            target = 0;
        }
        let stem = if g.r.odds(80) { "index".to_string() } else { g.ident(style) };
        let exts: &[&str] = match layout {
            0 => &["js", "d.ts"],
            1 => &["js", "js.map", "d.ts", "d.ts.map"],
            2 => &["mjs", "cjs", "d.ts"],
            _ => &["js"],
        };
        for e in exts {
            let n = format!("{stem}.{e}");
            let s = g.size_for(&n);
            if g.file(&mut dirs[target], &n, s, m) {
                placed += 1;
            }
        }
    }
    std::mem::swap(&mut g.r, &mut r);
}

/// node_modules with `n` packages: flat (npm, yarn) or pnpm's nested store.
fn node_modules(g: &mut G, p: &mut Dir, n: usize, pnpm: bool) {
    let Some(mut nm) = g.dir(p, "node_modules") else { return };
    let mut bin = g.must(&mut nm, ".bin");
    let pkgs = g.v.pkgs.clone();
    let scopes = g.v.scopes.clone();
    let mut chosen: Vec<String> = Vec::new();
    for _ in 0..n {
        let i = g.r.skew(0, pkgs.len() as u64 - 1) as usize;
        let name = if g.r.odds(220) { format!("{}/{}", g.r.pick(&scopes), pkgs[i]) } else { pkgs[i].to_string() };
        chosen.push(name);
    }
    if pnpm {
        let mut store = g.must(&mut nm, ".pnpm");
        for name in &chosen {
            let v = g.version();
            let key = format!("{}@{v}", name.replace('/', "+"));
            let Some(mut e) = g.dir(&mut store, &key) else { continue };
            let mut inner = g.must(&mut e, "node_modules");
            package(g, &mut inner, name, Some(NOW - 60 * DAY));
            for _ in 0..g.r.below(4) {
                g.link(&mut inner, &g.r.pick(&pkgs).to_string(), NOW - 60 * DAY);
            }
        }
        g.file(&mut store, "lock.yaml", 300_000, NOW - 60 * DAY);
        for name in chosen.iter().take(30) {
            g.link(&mut nm, name.split('/').next().unwrap(), NOW - 60 * DAY);
        }
    } else {
        for name in &chosen {
            package(g, &mut nm, name, None);
        }
        g.file(&mut nm, ".package-lock.json", 500_000, NOW - 60 * DAY);
    }
    for _ in 0..g.r.range(5, 40) {
        let t = g.ident(Style::Kebab);
        g.link(&mut bin, &t, NOW - 60 * DAY);
    }
}

// ---------------------------------------------------------------- projects

/// A git working copy: source tree, config files, .git, dependencies and
/// build output.
fn project(g: &mut G, p: &mut Dir, name: &str) {
    let Some(mut d) = g.dir(p, name) else { return };
    let kind = g.r.below(100);
    let m = g.r.mtime();
    let fresh = |g: &mut G| {
        if g.r.odds(300) { NOW - g.r.below(14 * DAY as u64) as u32 } else { m - g.r.below(200 * DAY as u64) as u32 }
    };
    for f in ["README.md", ".gitignore"] {
        let t = fresh(g);
        g.file(&mut d, f, 2000, t);
    }
    if g.r.odds(600) {
        g.file(&mut d, "LICENSE", 1070, m);
    }
    if g.r.odds(300) {
        let t = fresh(g);
        g.file(&mut d, ".env", g.r.range(80, 900), t);
        g.file(&mut d, ".env.example", 300, m);
    }
    if g.r.odds(250) {
        g.file(&mut d, "Makefile", 1500, m);
    }
    if g.r.odds(300) {
        g.file(&mut d, "Dockerfile", 800, m);
        g.file(&mut d, "docker-compose.yml", 1200, m);
    }
    if g.r.odds(200) {
        let mut gh = g.must(&mut d, ".github");
        let mut wf = g.must(&mut gh, "workflows");
        g.file(&mut wf, "ci.yml", 1500, m);
    }
    if g.r.odds(150) {
        let t = fresh(g);
        g.file(&mut d, "config.yaml", 900, t);
    }
    git_dir(g, &mut d, m);
    let (exts, style): (&[&str], Style) = match kind {
        0..55 => {
            for f in ["package.json", "tsconfig.json"] {
                let t = fresh(g);
                g.file(&mut d, f, 1500, t);
            }
            for f in [
                "pnpm-lock.yaml",
                "vite.config.ts",
                "webpack.config.js",
                ".eslintrc.json",
                ".prettierrc",
                "next.config.js",
                "jest.config.js",
                "tailwind.config.js",
            ] {
                if g.r.odds(300) {
                    g.file(&mut d, f, 1500, m);
                }
            }
            let pnpm = g.r.odds(450);
            let n = g.r.skew(60, 1400) as usize;
            if g.r.odds(250) {
                // Monorepo: packages/*, each with its own deps.
                let mut pk = g.must(&mut d, "packages");
                for _ in 0..g.r.range(2, 6) {
                    let sub = g.ident(Style::Kebab);
                    if let Some(mut s) = g.dir(&mut pk, &sub) {
                        g.file(&mut s, "package.json", 900, m);
                        g.file(&mut s, "tsconfig.json", 400, m);
                        source_tree(g, &mut s, &["ts", "tsx"], Style::Pascal, 60);
                        node_modules(g, &mut s, n / 6, pnpm);
                    }
                }
            }
            node_modules(g, &mut d, n, pnpm);
            if g.r.odds(500) {
                let mut dist = g.must(&mut d, if g.r.odds(500) { "dist" } else { "build" });
                let mut assets = g.must(&mut dist, "assets");
                for _ in 0..g.r.range(5, 120) {
                    let n = format!(
                        "{}-{}.{}",
                        g.ident(Style::Camel),
                        g.r.alnum(8),
                        g.r.pick(&["js", "css", "js.map", "svg", "png", "woff2"])
                    );
                    let s = g.size_for(&n);
                    g.file(&mut assets, &n, s, m);
                }
                g.file(&mut dist, "index.html", 1200, m);
            }
            let mut public = g.must(&mut d, "public");
            for f in ["favicon.ico", "logo.svg", "robots.txt", "avatar.png", "og-image.png", "manifest.json"] {
                if g.r.odds(500) {
                    let s = g.size_for(f);
                    g.file(&mut public, f, s, m);
                }
            }
            (&["ts", "tsx", "js", "jsx", "css", "json", "test.ts", "test.tsx"], Style::Pascal)
        }
        55..67 => {
            for f in ["Cargo.toml", "Cargo.lock", "rust-toolchain.toml"] {
                g.file(&mut d, f, 1500, m);
            }
            target_dir(g, &mut d);
            (&["rs"], Style::Snake)
        }
        67..76 => {
            for f in ["go.mod", "go.sum", "main.go"] {
                g.file(&mut d, f, 1500, m);
            }
            (&["go", "go", "_test.go"], Style::Snake)
        }
        76..92 => {
            for f in ["requirements.txt", "setup.py", "pyproject.toml", "manage.py", "settings.py"] {
                if g.r.odds(500) {
                    g.file(&mut d, f, 1500, m);
                }
            }
            venv(g, &mut d);
            (&["py", "py", "ipynb"], Style::Snake)
        }
        _ => {
            for f in ["Package.swift", "Podfile"] {
                g.file(&mut d, f, 1500, m);
            }
            if g.r.odds(500) {
                let mut pods = g.must(&mut d, "Pods");
                for _ in 0..g.r.range(5, 40) {
                    let n = format!("{}{}", g.ident(Style::Pascal), g.r.pick(&["", "Kit", "SDK", "Swift"]));
                    if let Some(mut pd) = g.dir(&mut pods, &n) {
                        source_tree(g, &mut pd, &["swift", "h", "m"], Style::Pascal, 40);
                    }
                }
            }
            (&["swift", "swift", "plist", "xib", "storyboard"], Style::Pascal)
        }
    };
    let size = g.r.skew(20, 2500) as usize;
    let mut src = g.must(&mut d, "src");
    source_tree(g, &mut src, exts, style, size);
    if g.r.odds(400) {
        let mut tests = g.must(&mut d, if kind < 55 { "__tests__" } else { "tests" });
        source_tree(g, &mut tests, exts, style, size / 8 + 2);
    }
    if g.r.odds(300) {
        let mut logs = g.must(&mut d, "logs");
        for k in 0..g.r.range(1, 20) {
            let n = format!("{}-{k}.log", g.r.pick(&["server", "app", "error", "debug", "build"]));
            let t = fresh(g);
            g.file(&mut logs, &n, g.r.size(10, 24), t);
        }
    }
}

/// A source tree of about `n` files in feature folders.
fn source_tree(g: &mut G, root: &mut Dir, exts: &[&str], style: Style, n: usize) {
    let mut dirs: Vec<Dir> = Vec::new();
    for d in [
        "components",
        "utils",
        "lib",
        "hooks",
        "api",
        "models",
        "services",
        "pages",
        "routes",
        "store",
        "types",
        "styles",
        "config",
        "handlers",
        "db",
    ] {
        if g.r.odds(350) {
            if let Some(x) = g.dir(root, d) {
                dirs.push(x);
            }
        }
    }
    let mut placed = 0;
    let mut guard = 0;
    while placed < n && guard < n * 4 {
        guard += 1;
        let m = g.r.mtime();
        let stem = match g.r.below(20) {
            0 => "index".to_string(),
            1 => "main".to_string(),
            2 => {
                if style as u8 == Style::Pascal as u8 {
                    "App".to_string()
                } else {
                    "mod".to_string()
                }
            }
            _ => g.ident(style),
        };
        let ext = g.r.pick(exts).to_string();
        let name = format!("{stem}.{ext}");
        let s = g.size_for(&name);
        let ok = if dirs.is_empty() || g.r.odds(150) {
            g.file(root, &name, s, m)
        } else {
            let k = g.r.below(dirs.len() as u64) as usize;
            if g.r.odds(120) && dirs.len() < 200 {
                // A component folder: Button/Button.tsx, Button/index.ts
                let sub = g.ident(Style::Pascal);
                if let Some(mut x) = g.dir(&mut dirs[k], &sub) {
                    g.file(&mut x, &format!("{sub}.{ext}"), s, m);
                    g.file(&mut x, "index.ts", 200, m);
                    placed += 2;
                    dirs.push(x);
                }
                continue;
            }
            g.file(&mut dirs[k], &name, s, m)
        };
        placed += ok as usize;
    }
}

fn git_dir(g: &mut G, p: &mut Dir, m: u32) {
    let mut git = g.must(p, ".git");
    for f in ["HEAD", "config", "description", "index", "packed-refs", "FETCH_HEAD", "ORIG_HEAD", "COMMIT_EDITMSG"] {
        g.file(&mut git, f, 300, m);
    }
    let mut hooks = g.must(&mut git, "hooks");
    for h in [
        "pre-commit",
        "pre-push",
        "commit-msg",
        "post-update",
        "pre-rebase",
        "prepare-commit-msg",
        "update",
        "fsmonitor-watchman",
    ] {
        g.file(&mut hooks, &format!("{h}.sample"), 1500, m);
    }
    let mut info = g.must(&mut git, "info");
    g.file(&mut info, "exclude", 240, m);
    let mut refs = g.must(&mut git, "refs");
    let mut heads = g.must(&mut refs, "heads");
    for b in ["main", "develop", "feature", "fix"].iter().take(g.r.range(1, 4) as usize) {
        g.file(&mut heads, b, 41, m);
    }
    let mut obj = g.must(&mut git, "objects");
    let mut pack = g.must(&mut obj, "pack");
    for _ in 0..g.r.range(1, 4) {
        let h = g.r.hex(40);
        g.file(&mut pack, &format!("pack-{h}.pack"), 9_000_000, m);
        g.file(&mut pack, &format!("pack-{h}.idx"), 200_000, m);
    }
    let loose = g.r.skew(0, 400);
    let mut fan: Vec<Option<Dir>> = (0..256).map(|_| None).collect();
    for _ in 0..loose {
        let b = g.r.below(256) as usize;
        if fan[b].is_none() {
            fan[b] = g.dir(&mut obj, &format!("{b:02x}"));
        }
        let h = g.r.hex(38);
        if let Some(f) = fan[b].as_mut() {
            g.file(f, &h, 400, m);
        }
    }
}

fn target_dir(g: &mut G, p: &mut Dir) {
    let mut t = g.must(p, "target");
    g.file(&mut t, "CACHEDIR.TAG", 177, NOW - 20 * DAY);
    let mut dbg = g.must(&mut t, if g.r.odds(500) { "debug" } else { "release" });
    let mut deps = g.must(&mut dbg, "deps");
    let crates = g.v.crates.clone();
    for _ in 0..g.r.range(30, 300) {
        let c = g.r.pick(&crates).replace('-', "_");
        let h = g.r.hex(16);
        let m = NOW - g.r.below(30 * DAY as u64) as u32;
        for e in ["d", "rlib", "rmeta"] {
            g.file(&mut deps, &format!("lib{c}-{h}.{e}"), g.r.size(10, 22), m);
        }
    }
    let mut build = g.must(&mut dbg, "build");
    for _ in 0..g.r.range(5, 40) {
        let c = g.r.pick(&crates).to_string();
        let h = g.r.hex(16);
        if let Some(mut b) = g.dir(&mut build, &format!("{c}-{h}")) {
            g.file(&mut b, "output", 300, NOW - 20 * DAY);
            g.file(&mut b, "invoked.timestamp", 0, NOW - 20 * DAY);
        }
    }
    let mut inc = g.must(&mut dbg, "incremental");
    for _ in 0..g.r.range(1, 8) {
        let n = format!("{}-{}", g.ident(Style::Snake), g.r.alnum(13));
        if let Some(mut i) = g.dir(&mut inc, &n) {
            for _ in 0..g.r.range(10, 150) {
                let f = format!("{}.o", g.r.alnum(25));
                g.file(&mut i, &f, 20_000, NOW - 5 * DAY);
            }
        }
    }
}

fn venv(g: &mut G, p: &mut Dir) {
    let mut v = g.must(p, ".venv");
    g.file(&mut v, "pyvenv.cfg", 200, NOW - 90 * DAY);
    let mut lib = g.must(&mut v, "lib");
    let mut py = g.must(&mut lib, "python3.12");
    let mut sp = g.must(&mut py, "site-packages");
    let pys = g.v.pys.clone();
    for _ in 0..g.r.range(10, 120) {
        let name = g.r.pick(&pys).to_string();
        let Some(mut pk) = g.dir(&mut sp, &name) else { continue };
        let ver = g.version();
        if let Some(mut di) = g.dir(&mut sp, &format!("{name}-{ver}.dist-info")) {
            for f in ["METADATA", "RECORD", "WHEEL", "INSTALLER", "top_level.txt"] {
                g.file(&mut di, f, 2000, NOW - 90 * DAY);
            }
        }
        let mut r = Rng::new(hash(&name));
        let files = r.skew(3, 900) as usize;
        std::mem::swap(&mut g.r, &mut r);
        let mut cache = g.must(&mut pk, "__pycache__");
        g.file(&mut pk, "__init__.py", 500, NOW - 90 * DAY);
        let mut subs: Vec<Dir> = Vec::new();
        for _ in 0..files {
            let stem = g.ident(Style::Snake);
            let into =
                if !subs.is_empty() && g.r.odds(500) { Some(g.r.below(subs.len() as u64) as usize) } else { None };
            let ok = match into {
                Some(k) => g.file(&mut subs[k], &format!("{stem}.py"), 4000, NOW - 90 * DAY),
                None => g.file(&mut pk, &format!("{stem}.py"), 4000, NOW - 90 * DAY),
            };
            if ok && into.is_none() {
                g.file(&mut cache, &format!("{stem}.cpython-312.pyc"), 6000, NOW - 90 * DAY);
            }
            if g.r.odds(40) && subs.len() < 30 {
                let sn = g.ident(Style::Snake);
                if let Some(mut s) = g.dir(&mut pk, &sn) {
                    g.file(&mut s, "__init__.py", 100, NOW - 90 * DAY);
                    subs.push(s);
                }
            }
        }
        std::mem::swap(&mut g.r, &mut r);
    }
}

// ---------------------------------------------------------------- regions

fn system(g: &mut G, sys: &mut Dir, until: u32) {
    let mut lib = g.must(sys, "Library");
    let mut fws = g.must(&mut lib, "Frameworks");
    let mut pfws = g.must(&mut lib, "PrivateFrameworks");
    let mut cs = g.must(&mut lib, "CoreServices");
    for a in [
        "Finder",
        "Dock",
        "SystemUIServer",
        "Spotlight",
        "Siri",
        "Setup Assistant",
        "Software Update",
        "ControlCenter",
        "NotificationCenter",
    ] {
        app(g, &mut cs, a, false, 20);
    }
    let mut k = 0;
    while g.more(until) {
        let n = fw_name(g);
        k += 1;
        if k % 5 == 0 {
            framework(g, &mut fws, &n, 12, g.r.range(5, 60) as usize);
        } else {
            framework(g, &mut pfws, &n, g.r.range(0, 8) as usize, 0);
        }
    }
    let mut app = g.must(sys, "Applications");
    for a in [
        "Safari",
        "Mail",
        "Messages",
        "Notes",
        "Calendar",
        "Photos",
        "Music",
        "TV",
        "Maps",
        "Preview",
        "TextEdit",
        "Utilities",
    ] {
        if let Some(mut x) = g.dir(&mut app, &format!("{a}.app")) {
            g.file(&mut x, "Contents", 0, NOW - 100 * DAY);
        }
    }
}

fn applications(g: &mut G, apps: &mut Dir, until: u32) {
    let names = g.v.apps.clone();
    let electron = g.v.electron.clone();
    for e in &electron {
        app(g, apps, e, true, 30);
    }
    for a in &names {
        if g.n() >= until {
            break;
        }
        let size = g.r.skew(5, 300) as usize;
        app(g, apps, a, false, size);
    }
    if let Some(mut x) = g.dir(apps, "Xcode.app") {
        let mut c = g.must(&mut x, "Contents");
        let mut dev = g.must(&mut c, "Developer");
        let mut plats = g.must(&mut dev, "Platforms");
        for p in ["iPhoneOS", "iPhoneSimulator", "MacOSX", "WatchOS", "AppleTVOS", "XROS"] {
            let Some(mut pl) = g.dir(&mut plats, &format!("{p}.platform")) else { continue };
            let mut d = g.must(&mut pl, "Developer");
            let mut sdks = g.must(&mut d, "SDKs");
            let Some(mut sdk) = g.dir(&mut sdks, &format!("{p}.sdk")) else { continue };
            let mut sl = g.must(&mut sdk, "System");
            let mut l = g.must(&mut sl, "Library");
            let mut f = g.must(&mut l, "Frameworks");
            while g.more(until) {
                let n = fw_name(g);
                framework(g, &mut f, &n, 0, g.r.range(3, 80) as usize);
                if g.r.odds(60) {
                    break;
                }
            }
        }
    }
}

fn library_root(g: &mut G, lib: &mut Dir, until: u32) {
    let mut prefs = g.must(lib, "Preferences");
    for _ in 0..80 {
        let n = format!("com.apple.{}.plist", g.ident(Style::Camel));
        g.file(&mut prefs, &n, 2000, g.r.mtime());
    }
    let mut dev = g.must(lib, "Developer");
    let mut clt = g.must(&mut dev, "CommandLineTools");
    let mut sdks = g.must(&mut clt, "SDKs");
    let mut sdk = g.must(&mut sdks, "MacOSX.sdk");
    let mut usr = g.must(&mut sdk, "usr");
    let mut inc = g.must(&mut usr, "include");
    let mut dirs: Vec<Dir> = Vec::new();
    while g.more(until) {
        if dirs.is_empty() || g.r.odds(60) {
            let n = g.ident(Style::Snake);
            if let Some(d) = g.dir(&mut inc, &n) {
                dirs.push(d);
            }
            continue;
        }
        let k = g.r.below(dirs.len() as u64) as usize;
        let n = format!("{}.h", g.ident(Style::Snake));
        g.file(&mut dirs[k], &n, 5000, NOW - 300 * DAY);
    }
    for d in
        ["Application Support", "Caches", "Fonts", "LaunchAgents", "LaunchDaemons", "Logs", "PrivilegedHelperTools"]
    {
        let mut x = g.must(lib, d);
        for _ in 0..g.r.range(3, 30) {
            let n = format!("com.{}.{}", g.word(), g.ident(Style::Camel));
            g.file(&mut x, &format!("{n}.plist"), 900, g.r.mtime());
        }
    }
}

fn homebrew(g: &mut G, opt: &mut Dir, until: u32) {
    let mut hb = g.must(opt, "homebrew");
    let mut cellar = g.must(&mut hb, "Cellar");
    let mut bin = g.must(&mut hb, "bin");
    let brews = g.v.brews.clone();
    for b in &brews {
        if g.n() >= until {
            break;
        }
        let Some(mut f) = g.dir(&mut cellar, b) else { continue };
        let v = g.version();
        let mut d = g.must(&mut f, &v);
        let m = g.r.mtime();
        g.file(&mut d, "INSTALL_RECEIPT.json", 2000, m);
        g.file(&mut d, "README.md", 3000, m);
        let mut bd = g.must(&mut d, "bin");
        g.file(&mut bd, b, 500_000, m);
        g.link(&mut bin, b, m);
        let mut share = g.must(&mut d, "share");
        let mut man = g.must(&mut share, "man");
        let mut man1 = g.must(&mut man, "man1");
        g.file(&mut man1, &format!("{b}.1"), 9000, m);
        let k = g.r.skew(0, 2500);
        if k > 20 {
            let mut lib = g.must(&mut d, "lib");
            let mut inc = g.must(&mut d, "include");
            for _ in 0..k {
                let s = g.ident(Style::Snake);
                g.file(&mut inc, &format!("{s}.h"), 4000, m);
                if g.r.odds(200) {
                    g.file(&mut lib, &format!("lib{s}.dylib"), 300_000, m);
                }
            }
        }
    }
}

fn usr_tree(g: &mut G, usr: &mut Dir) {
    let tools = g.v.tools.clone();
    let mut bin = g.must(usr, "bin");
    let mut man = g.must(usr, "share");
    let mut man1 = g.must(&mut man, "man");
    let mut m1 = g.must(&mut man1, "man1");
    for t in &tools {
        g.file(&mut bin, t, 100_000, NOW - 200 * DAY);
        g.file(&mut m1, &format!("{t}.1"), 5000, NOW - 200 * DAY);
    }
    let mut lib = g.must(usr, "lib");
    for _ in 0..300 {
        let n = format!("lib{}.dylib", g.ident(Style::Snake));
        g.file(&mut lib, &n, 200_000, NOW - 200 * DAY);
    }
    let mut local = g.must(usr, "local");
    g.must(&mut local, "bin");
}

fn private_tree(g: &mut G, private: &mut Dir, until: u32) {
    let mut etc = g.must(private, "etc");
    for f in [
        "hosts",
        "zshrc",
        "zprofile",
        "bashrc",
        "paths",
        "shells",
        "ssh_config",
        "sudoers",
        "resolv.conf",
        "passwd",
        "group",
        "profile",
    ] {
        g.file(&mut etc, f, 1000, NOW - 300 * DAY);
    }
    let mut var = g.must(private, "var");
    let mut db = g.must(&mut var, "db");
    let mut uuidtext = g.must(&mut db, "uuidtext");
    let start = g.n();
    let mut fan: Vec<Dir> = (0..256).filter_map(|b| g.dir(&mut uuidtext, &format!("{b:02X}"))).collect();
    while g.more(start + (until - start) / 12) {
        let b = g.r.below(fan.len() as u64) as usize;
        let h = g.r.hex(30).to_uppercase();
        g.file(&mut fan[b], &h, 30_000, g.r.mtime());
    }
    drop(fan);
    let mut log = g.must(&mut var, "log");
    for k in 0..200 {
        g.file(&mut log, &format!("system.log.{k}.gz"), 50_000, NOW - k * DAY);
    }
    let mut folders = g.must(&mut var, "folders");
    let mut xx = g.must(&mut folders, "nw");
    let user = format!("{}0000gn", g.r.alnum(22));
    let mut u = g.must(&mut xx, &user);
    // X: code-sign clones of big apps (whole copies of Chrome and friends).
    let mut x = g.must(&mut u, "X");
    for (k, a) in ["Google Chrome", "Microsoft Edge", "Google Chrome"].iter().enumerate() {
        let tag = format!("com.{}.code_sign_clone", a.to_lowercase().replace(' ', "."));
        let Some(mut c) = g.dir(&mut x, &tag) else { continue };
        for _ in 0..(k + 2) {
            let n = format!("code_sign_clone.{}", g.r.alnum(6));
            if let Some(mut cl) = g.dir(&mut c, &n) {
                app(g, &mut cl, a, true, 40);
            }
        }
    }
    // C: caches. T: temp, with unpacked package trees and build scratch.
    let mut c = g.must(&mut u, "C");
    for _ in 0..300 {
        let n = format!("com.apple.{}", g.ident(Style::Camel));
        if let Some(mut d) = g.dir(&mut c, &n) {
            for _ in 0..g.r.skew(0, 60) {
                let f = g.r.hex(16);
                g.file(&mut d, &f, 5000, g.r.mtime());
            }
        }
    }
    let mut t = g.must(&mut u, "T");
    let pkgs = g.v.pkgs.clone();
    while g.more(until) {
        match g.r.below(10) {
            0..4 => {
                let n = format!("npx-{}", g.r.hex(16));
                if let Some(mut d) = g.dir(&mut t, &n) {
                    let k = g.r.skew(5, 400) as usize;
                    node_modules(g, &mut d, k, false);
                }
            }
            4..6 => {
                let n = format!("tmp-{}-{}", g.r.range(1000, 99999), g.r.alnum(12));
                if let Some(mut d) = g.dir(&mut t, &n) {
                    let p = g.r.pick(&pkgs).to_string();
                    package(g, &mut d, &p, Some(g.r.mtime()));
                }
            }
            6..8 => {
                let n = format!(
                    "{}{}",
                    g.r.pick(&["TemporaryItems-", "playwright-", "vscode-", "node-compile-cache-", "go-build"]),
                    g.r.alnum(10)
                );
                if let Some(mut d) = g.dir(&mut t, &n) {
                    let mut sub: Vec<Dir> = Vec::new();
                    for _ in 0..g.r.skew(1, 3000) {
                        if sub.is_empty() || g.r.odds(30) {
                            let h = g.r.hex(2);
                            if let Some(s) = g.dir(&mut d, &h) {
                                sub.push(s);
                            }
                            continue;
                        }
                        let k = g.r.below(sub.len() as u64) as usize;
                        let f = format!("{}{}", g.r.hex(24), g.r.pick(&["", "-d", ".tmp", ".js", ".a", ".o"]));
                        g.file(&mut sub[k], &f, 9000, g.r.mtime());
                    }
                }
            }
            _ => {
                let n =
                    format!("{}.{}", g.ident(Style::Camel), g.r.pick(&["tmp", "log", "json", "txt", "lock", "sock"]));
                let m = g.r.mtime();
                g.file(&mut t, &n, 4000, m);
            }
        }
    }
}

fn home_tree(g: &mut G, home: &mut Dir) {
    let total_until = TOTAL;
    let home_start = g.n();
    let share = |pct: u32| home_start + (total_until - home_start) / 1000 * pct;
    for f in [
        ".zshrc",
        ".zprofile",
        ".zsh_history",
        ".gitconfig",
        ".npmrc",
        ".bash_profile",
        ".viminfo",
        ".CFUserTextEncoding",
        ".DS_Store",
        ".lesshst",
        ".python_history",
        ".node_repl_history",
    ] {
        let t = if f.contains("history") { NOW - 600 } else { g.r.mtime() };
        g.file(home, f, 3000, t);
    }
    let mut ssh = g.must(home, ".ssh");
    for f in ["config", "known_hosts", "id_ed25519", "id_ed25519.pub", "authorized_keys"] {
        g.file(&mut ssh, f, 400, g.r.mtime());
    }
    user_files(g, home);

    // Code: ~/Developer and ~/Work/<org>/<project>.
    let mut dev = g.must(home, "Developer");
    let mut work = g.must(home, "Work");
    let mut orgs: Vec<Dir> = Vec::new();
    for o in g.v.orgs.clone() {
        if let Some(d) = g.dir(&mut work, o) {
            orgs.push(d);
        }
    }
    let until = share(420);
    while g.more(until) {
        let name = project_name(g);
        if g.r.odds(400) {
            project(g, &mut dev, &name);
        } else {
            let k = g.r.below(orgs.len() as u64) as usize;
            project(g, &mut orgs[k], &name);
        }
    }
    drop(orgs);

    let mut lib = g.must(home, "Library");
    home_library(g, &mut lib, share(780));
    dot_dirs(g, home, total_until);
}

fn project_name(g: &mut G) -> String {
    match g.r.below(4) {
        0 => g.ident(Style::Kebab),
        1 => format!(
            "{}-{}",
            g.word(),
            g.r.pick(&["web", "api", "app", "server", "client", "cli", "sdk", "docs", "infra", "mobile"])
        ),
        2 => g.ident(Style::Pascal),
        _ => g.word().to_string(),
    }
}

/// Documents, Downloads, Desktop, Pictures, Movies, Music.
fn user_files(g: &mut G, home: &mut Dir) {
    let mut docs = g.must(home, "Documents");
    for _ in 0..1500 {
        let (stem, ext) = match g.r.below(10) {
            0 => (format!("Invoice-{}-{:02}", g.r.range(2019, 2026), g.r.range(1, 12)), "pdf"),
            1 => (format!("invoice_{:04}", g.r.below(3000)), "pdf"),
            2 => (format!("Budget {}", g.r.range(2018, 2026)), "xlsx"),
            3 => (
                format!(
                    "{} report {}",
                    g.r.pick(&["Quarterly", "Annual", "Monthly", "Sales", "Weekly"]),
                    g.r.range(2018, 2026)
                ),
                g.r.pick(&["pdf", "docx", "key"]),
            ),
            4 => (format!("Meeting notes {}", g.r.range(1, 300)), g.r.pick(&["md", "docx", "txt"])),
            5 => (
                g.r.pick(&["Resume", "CV", "resume", "Cover Letter", "Notes", "notes", "Todo", "Ideas"]).to_string()
                    + &format!(" {}", g.r.below(20)),
                g.r.pick(&["pdf", "docx", "md", "pages"]),
            ),
            _ => (
                g.ident(Style::Pascal),
                g.r.pick(&["pdf", "docx", "xlsx", "pptx", "txt", "md", "numbers", "pages", "csv"]),
            ),
        };
        let n = format!("{stem}.{ext}");
        let s = g.size_for(&n);
        let t = g.r.mtime();
        g.file(&mut docs, &n, s, t);
    }
    let mut dl = g.must(home, "Downloads");
    for _ in 0..1200 {
        let n = match g.r.below(8) {
            0 => format!("{}-{}.dmg", g.r.pick(&g.v.apps.clone()).replace(' ', ""), g.version()),
            1 => format!("{}.zip", g.ident(Style::Kebab)),
            2 => format!("{} ({}).pdf", g.ident(Style::Pascal), g.r.range(1, 9)),
            3 => format!("IMG_{}.HEIC", g.r.range(1000, 9999)),
            _ => format!(
                "{}.{}",
                g.ident(Style::Kebab),
                g.r.pick(&["pdf", "csv", "png", "jpg", "zip", "json", "pkg", "mp4"])
            ),
        };
        let s = g.size_for(&n);
        let t = g.r.mtime();
        g.file(&mut dl, &n, s, t);
    }
    let mut desk = g.must(home, "Desktop");
    for _ in 0..400 {
        let t = g.r.mtime();
        let n = if g.r.odds(850) {
            format!(
                "Screenshot {}-{:02}-{:02} at {}.{:02}.{:02}.png",
                g.r.range(2021, 2026),
                g.r.range(1, 12),
                g.r.range(1, 28),
                g.r.range(1, 12),
                g.r.below(60),
                g.r.below(60)
            )
        } else {
            format!(
                "Screen Recording {}-{:02}-{:02} at {}.{:02}.{:02}.mov",
                g.r.range(2021, 2026),
                g.r.range(1, 12),
                g.r.range(1, 28),
                g.r.range(1, 12),
                g.r.below(60),
                g.r.below(60)
            )
        };
        let s = g.size_for(&n);
        g.file(&mut desk, &n, s, t);
    }
    let mut pics = g.must(home, "Pictures");
    let mut lib = g.must(&mut pics, "Photos Library.photoslibrary");
    let mut orig = g.must(&mut lib, "originals");
    let mut fan: Vec<Dir> = (0..16).filter_map(|b| g.dir(&mut orig, &format!("{b:X}"))).collect();
    for _ in 0..6000 {
        let b = g.r.below(16) as usize;
        let n = format!("{}.{}", g.r.uuid(), g.r.pick(&["heic", "jpeg", "mov", "png"]));
        let s = g.size_for(&n);
        let t = g.r.mtime();
        g.file(&mut fan[b], &n, s, t);
    }
    drop(fan);
    for _ in 0..800 {
        let n = match g.r.below(3) {
            0 => format!(
                "IMG_{}{:02}{:02}_{:06}.jpg",
                g.r.range(2016, 2026),
                g.r.range(1, 12),
                g.r.range(1, 28),
                g.r.below(240000)
            ),
            1 => format!(
                "photo_{}-{:02}-{:02}_{:02}-{:02}-{:02}.jpg",
                g.r.range(2016, 2026),
                g.r.range(1, 12),
                g.r.range(1, 28),
                g.r.below(24),
                g.r.below(60),
                g.r.below(60)
            ),
            _ => format!("IMG_{}.JPG", g.r.range(1000, 9999)),
        };
        let s = g.size_for(&n);
        let t = g.r.mtime();
        g.file(&mut pics, &n, s, t);
    }
    let mut mov = g.must(home, "Movies");
    for _ in 0..60 {
        let n = format!("{}.{}", g.ident(Style::Pascal), g.r.pick(&["mov", "mp4", "m4v"]));
        let s = g.size_for(&n);
        let t = g.r.mtime();
        g.file(&mut mov, &n, s, t);
    }
    let mut mus = g.must(home, "Music");
    g.must(&mut mus, "Music");
}

fn home_library(g: &mut G, lib: &mut Dir, until: u32) {
    let start = g.n();
    let part = |pct: u32| start + (until - start) / 100 * pct;
    let mut prefs = g.must(lib, "Preferences");
    for _ in 0..600 {
        let n = format!("com.{}.{}.plist", g.word(), g.ident(Style::Camel));
        let t = g.r.mtime();
        g.file(&mut prefs, &n, 3000, t);
    }
    let mut logs = g.must(lib, "Logs");
    for _ in 0..300 {
        let n = format!("{}-{}.log", g.ident(Style::Kebab), g.r.range(1, 999));
        let t = if g.r.odds(300) { NOW - g.r.below(6 * DAY as u64) as u32 } else { g.r.mtime() };
        g.file(&mut logs, &n, g.r.size(10, 22), t);
    }
    // pnpm's content-addressed store: hex-named blobs in 256 fan-out dirs.
    let mut pn = g.must(lib, "pnpm");
    let mut store = g.must(&mut pn, "store");
    let mut v10 = g.must(&mut store, "v10");
    let mut files = g.must(&mut v10, "files");
    let mut fan: Vec<Dir> = (0..256).filter_map(|b| g.dir(&mut files, &format!("{b:02x}"))).collect();
    let mut index = g.must(&mut v10, "index");
    let pkgs = g.v.pkgs.clone();
    while g.more(part(40)) {
        let b = g.r.below(256) as usize;
        let n = format!("{}{}", g.r.hex(126), if g.r.odds(50) { "-exec" } else { "" });
        g.file(&mut fan[b], &n, 3000, NOW - g.r.below(300 * DAY as u64) as u32);
        if g.r.odds(30) {
            let n = format!("{}-{}@{}.json", g.r.hex(10), g.r.pick(&pkgs), g.version());
            g.file(&mut index, &n, 4000, NOW - 100 * DAY);
        }
    }
    drop(fan);
    // Application Support: editors, browsers, chat apps.
    let mut asup = g.must(lib, "Application Support");
    let mut code = g.must(&mut asup, "Code");
    let mut user = g.must(&mut code, "User");
    g.file(&mut user, "settings.json", 3000, NOW - 2 * DAY);
    g.file(&mut user, "keybindings.json", 800, NOW - 90 * DAY);
    let mut ws = g.must(&mut user, "workspaceStorage");
    for _ in 0..400 {
        let h = g.r.hex(32);
        if let Some(mut d) = g.dir(&mut ws, &h) {
            g.file(&mut d, "workspace.json", 100, g.r.mtime());
            g.file(&mut d, "state.vscdb", 40_000, g.r.mtime());
        }
    }
    let mut chrome = g.must(&mut asup, "Google");
    let mut ch = g.must(&mut chrome, "Chrome");
    let mut def = g.must(&mut ch, "Default");
    for f in ["Bookmarks", "History", "Cookies", "Preferences", "Favicons", "Login Data", "Top Sites", "Web Data"] {
        g.file(&mut def, f, 300_000, NOW - DAY);
    }
    let mut cache = g.must(&mut def, "Cache");
    let mut cd = g.must(&mut cache, "Cache_Data");
    let mut cc = g.must(&mut def, "Code Cache");
    let mut js = g.must(&mut cc, "js");
    let mut idb = g.must(&mut def, "IndexedDB");
    while g.more(part(55)) {
        match g.r.below(3) {
            0 => {
                let n = format!("{}_0", g.r.hex(16));
                g.file(&mut cd, &n, 20_000, g.r.mtime());
            }
            1 => {
                let n = format!("{}_0", g.r.hex(16));
                g.file(&mut js, &n, 20_000, g.r.mtime());
            }
            _ => {
                let n = format!("https_{}.{}_0.indexeddb.leveldb", g.word(), g.r.pick(&["com", "io", "app", "dev"]));
                if let Some(mut d) = g.dir(&mut idb, &n) {
                    for f in ["CURRENT", "LOCK", "LOG", "LOG.old", "MANIFEST-000001"] {
                        g.file(&mut d, f, 100, g.r.mtime());
                    }
                    for k in 0..g.r.range(1, 6) {
                        g.file(&mut d, &format!("{:06}.ldb", k * 3 + 5), 200_000, g.r.mtime());
                    }
                }
            }
        }
    }
    for a in ["Slack", "discord", "Notion", "Cursor", "Claude", "Spotify", "zoom.us", "Figma", "Postman", "Docker"] {
        if let Some(mut d) = g.dir(&mut asup, a) {
            for sub in ["Cache", "Code Cache", "GPUCache", "Local Storage", "Session Storage", "logs", "blob_storage"] {
                if let Some(mut s) = g.dir(&mut d, sub) {
                    for _ in 0..g.r.skew(0, 300) {
                        let n = format!("{}_0", g.r.hex(16));
                        g.file(&mut s, &n, 9000, g.r.mtime());
                    }
                }
            }
        }
    }
    // Caches, Containers, Developer (Xcode).
    let mut caches = g.must(lib, "Caches");
    while g.more(part(68)) {
        let n = format!("com.{}.{}", g.word(), g.ident(Style::Camel));
        if let Some(mut d) = g.dir(&mut caches, &n) {
            g.file(&mut d, "Cache.db", 400_000, g.r.mtime());
            let mut fs = g.must(&mut d, "fsCachedData");
            for _ in 0..g.r.skew(0, 800) {
                let u = g.r.uuid();
                g.file(&mut fs, &u, 30_000, g.r.mtime());
            }
        }
    }
    let mut cont = g.must(lib, "Containers");
    while g.more(part(78)) {
        let n = format!("com.apple.{}", g.ident(Style::Camel));
        let Some(mut d) = g.dir(&mut cont, &n) else { continue };
        g.file(&mut d, ".com.apple.containermanagerd.metadata.plist", 3000, g.r.mtime());
        let mut data = g.must(&mut d, "Data");
        let mut dl = g.must(&mut data, "Library");
        for sub in ["Caches", "Preferences", "Application Support", "Saved Application State", "Cookies", "WebKit"] {
            if let Some(mut s) = g.dir(&mut dl, sub) {
                for _ in 0..g.r.skew(0, 200) {
                    let f = format!(
                        "{}.{}",
                        g.ident(Style::Camel),
                        g.r.pick(&["plist", "db", "sqlite", "json", "binarycookies"])
                    );
                    g.file(&mut s, &f, 4000, g.r.mtime());
                }
            }
        }
    }
    let mut dev = g.must(lib, "Developer");
    let mut xc = g.must(&mut dev, "Xcode");
    let mut dd = g.must(&mut xc, "DerivedData");
    let mut sim = g.must(&mut dev, "CoreSimulator");
    let mut devices = g.must(&mut sim, "Devices");
    while g.more(until) {
        if g.r.odds(600) {
            let n = format!("{}-{}", g.ident(Style::Pascal), g.r.alnum(28));
            let Some(mut p) = g.dir(&mut dd, &n) else { continue };
            g.file(&mut p, "info.plist", 300, g.r.mtime());
            let mut b = g.must(&mut p, "Build");
            let mut i = g.must(&mut b, "Intermediates.noindex");
            for _ in 0..g.r.skew(20, 4000) {
                let f = format!(
                    "{}.{}",
                    g.ident(Style::Pascal),
                    g.r.pick(&["o", "d", "dia", "swiftdeps", "swiftmodule", "swiftdoc"])
                );
                g.file(&mut i, &f, 30_000, g.r.mtime());
            }
        } else {
            let u = g.r.uuid();
            let Some(mut dv) = g.dir(&mut devices, &u) else { continue };
            g.file(&mut dv, "device.plist", 900, g.r.mtime());
            let mut data = g.must(&mut dv, "data");
            let mut cont = g.must(&mut data, "Containers");
            let mut bundle = g.must(&mut cont, "Bundle");
            let mut app_dir = g.must(&mut bundle, "Application");
            let u2 = g.r.uuid();
            let mut ad = g.must(&mut app_dir, &u2);
            let an = g.ident(Style::Pascal);
            app(g, &mut ad, &an, false, 20);
        }
    }
}

fn dot_dirs(g: &mut G, home: &mut Dir, until: u32) {
    let start = g.n();
    let part = |pct: u32| start + (until - start) / 100 * pct;
    // nvm: node versions, each with npm's own node_modules and globals.
    let mut nvm = g.must(home, ".nvm");
    let mut vers = g.must(&mut nvm, "versions");
    let mut node = g.must(&mut vers, "node");
    for v in ["v18.20.4", "v20.17.0", "v22.9.0", "v16.20.2"] {
        let Some(mut d) = g.dir(&mut node, v) else { continue };
        let mut bin = g.must(&mut d, "bin");
        for b in ["node", "npm", "npx", "corepack"] {
            g.file(&mut bin, b, 90_000_000, NOW - 300 * DAY);
        }
        let mut lib = g.must(&mut d, "lib");
        let k = g.r.range(150, 500) as usize;
        node_modules(g, &mut lib, k, false);
        let mut inc = g.must(&mut d, "include");
        let mut nd = g.must(&mut inc, "node");
        for _ in 0..200 {
            let n = format!("{}.h", g.ident(Style::Snake));
            g.file(&mut nd, &n, 9000, NOW - 300 * DAY);
        }
    }
    // bun: global installs and its cache.
    let mut bun = g.must(home, ".bun");
    let mut inst = g.must(&mut bun, "install");
    let mut glob = g.must(&mut inst, "global");
    let k = g.r.range(300, 900) as usize;
    node_modules(g, &mut glob, k, false);
    let mut cache = g.must(&mut inst, "cache");
    let pkgs = g.v.pkgs.clone();
    while g.more(part(40)) {
        let n = format!("{}@{}@@@1", g.r.pick(&pkgs), g.version());
        if let Some(mut d) = g.dir(&mut cache, &n) {
            let p = g.r.pick(&pkgs).to_string();
            package(g, &mut d, &p, Some(NOW - 100 * DAY));
        }
    }
    // npm: _npx installs and the content cache.
    let mut npm = g.must(home, ".npm");
    let mut npx = g.must(&mut npm, "_npx");
    while g.more(part(55)) {
        let h = g.r.hex(16);
        if let Some(mut d) = g.dir(&mut npx, &h) {
            g.file(&mut d, "package.json", 200, g.r.mtime());
            let k = g.r.skew(5, 300) as usize;
            node_modules(g, &mut d, k, false);
        }
    }
    let mut cac = g.must(&mut npm, "_cacache");
    let mut content = g.must(&mut cac, "content-v2");
    let mut sha = g.must(&mut content, "sha512");
    let mut fan: Vec<Dir> = (0..256).filter_map(|b| g.dir(&mut sha, &format!("{b:02x}"))).collect();
    while g.more(part(62)) {
        let b = g.r.below(256) as usize;
        let h2 = g.r.hex(2);
        let h = g.r.hex(124);
        let mut sub = match g.dir(&mut fan[b], &h2) {
            Some(d) => d,
            None => continue,
        };
        for _ in 0..g.r.range(1, 30) {
            let h = format!("{h}{}", g.r.hex(2));
            g.file(&mut sub, &h, 40_000, g.r.mtime());
        }
    }
    drop(fan);
    // Rust: registry sources and toolchains.
    let mut cargo = g.must(home, ".cargo");
    let mut reg = g.must(&mut cargo, "registry");
    let mut src = g.must(&mut reg, "src");
    let mut idx = g.must(&mut src, "index.crates.io-1949cf8c6b5b557f");
    let crates = g.v.crates.clone();
    while g.more(part(78)) {
        let c = g.r.pick(&crates).to_string();
        let v = g.version();
        let Some(mut d) = g.dir(&mut idx, &format!("{c}-{v}")) else { continue };
        let m = NOW - g.r.below(400 * DAY as u64) as u32;
        for f in [
            "Cargo.toml",
            "Cargo.toml.orig",
            "README.md",
            "LICENSE-MIT",
            "LICENSE-APACHE",
            ".cargo_vcs_info.json",
            "Cargo.lock",
        ] {
            if f == "Cargo.toml" || g.r.odds(600) {
                g.file(&mut d, f, 3000, m);
            }
        }
        let mut s = g.must(&mut d, "src");
        let n = Rng::new(hash(&c)).skew(2, 400) as usize;
        g.file(&mut s, "lib.rs", 9000, m);
        let mut subs: Vec<Dir> = Vec::new();
        for _ in 0..n {
            let stem = g.ident(Style::Snake);
            if g.r.odds(60) && subs.len() < 30 {
                if let Some(mut x) = g.dir(&mut s, &stem) {
                    g.file(&mut x, "mod.rs", 3000, m);
                    subs.push(x);
                }
                continue;
            }
            match subs.len() {
                0 => g.file(&mut s, &format!("{stem}.rs"), 6000, m),
                l => {
                    let k = g.r.below(l as u64) as usize;
                    g.file(&mut subs[k], &format!("{stem}.rs"), 6000, m)
                }
            };
        }
        for t in ["tests", "benches", "examples"] {
            if g.r.odds(250) {
                let mut x = g.must(&mut d, t);
                for _ in 0..g.r.range(1, 20) {
                    let n = format!("{}.rs", g.ident(Style::Snake));
                    g.file(&mut x, &n, 4000, m);
                }
            }
        }
    }
    let mut rustup = g.must(home, ".rustup");
    let mut tc = g.must(&mut rustup, "toolchains");
    let mut st = g.must(&mut tc, "stable-aarch64-apple-darwin");
    let mut share = g.must(&mut st, "share");
    let mut doc = g.must(&mut share, "doc");
    let mut rust = g.must(&mut doc, "rust");
    let mut html = g.must(&mut rust, "html");
    let mut dirs: Vec<Dir> = Vec::new();
    while g.more(part(88)) {
        if dirs.is_empty() || g.r.odds(40) {
            let n = g.ident(Style::Snake);
            if let Some(d) = g.dir(&mut html, &n) {
                dirs.push(d);
            }
            continue;
        }
        let k = g.r.below(dirs.len() as u64) as usize;
        let n = format!(
            "{}.{}.html",
            g.r.pick(&["fn", "struct", "trait", "enum", "macro", "type", "constant"]),
            g.ident(Style::Snake)
        );
        g.file(&mut dirs[k], &n, 20_000, NOW - 60 * DAY);
    }
    drop(dirs);
    // Go modules and generic caches.
    let mut go = g.must(home, "go");
    let mut pkg = g.must(&mut go, "pkg");
    let mut md = g.must(&mut pkg, "mod");
    let mut gh = g.must(&mut md, "github.com");
    while g.more(part(96)) {
        let owner = g.ident(Style::Kebab);
        let mut o = match g.dir(&mut gh, &owner) {
            Some(o) => o,
            None => continue,
        };
        for _ in 0..g.r.range(1, 4) {
            let repo = format!("{}@v{}", g.ident(Style::Kebab), g.version());
            if let Some(mut r) = g.dir(&mut o, &repo) {
                for f in ["go.mod", "go.sum", "README.md", "LICENSE"] {
                    g.file(&mut r, f, 2000, NOW - 200 * DAY);
                }
                source_tree(g, &mut r, &["go", "go", "_test.go"], Style::Snake, g.r.skew(3, 500) as usize);
            }
        }
    }
    let mut cache = g.must(home, ".cache");
    while g.more(until) {
        let n =
            g.r.pick(&[
                "huggingface",
                "pip",
                "uv",
                "prisma",
                "node-gyp",
                "typescript",
                "puppeteer",
                "selenium",
                "yarn",
                "deno",
            ])
            .to_string();
        let mut d = match g.dir(&mut cache, &n) {
            Some(d) => d,
            None => {
                let n2 = format!("{n}-{}", g.r.alnum(6));
                match g.dir(&mut cache, &n2) {
                    Some(d) => d,
                    None => continue,
                }
            }
        };
        for _ in 0..g.r.skew(10, 3000) {
            let f = format!("{}{}", g.r.hex(20), g.r.pick(&["", ".json", ".tar.gz", ".whl", ".bin"]));
            g.file(&mut d, &f, 20_000, g.r.mtime());
        }
    }
}

// ---------------------------------------------------------------- vocab

const WORDS: &str = "app user users auth api config data model models view views controller service services util utils helper helpers index main test spec types hooks store state router route routes page pages layout header footer button input form modal list item items card table grid icon image logo theme style styles client server request response error errors logger log cache queue worker job task event events handler middleware schema migration seed db database query parser lexer token ast node tree graph map set array string number date time format validate validator transform stream buffer file files path url http socket session cookie jwt crypto hash encode decode compress zip build bundle plugin loader resolver context provider reducer action actions selector effect hook component container wrapper base core common shared lib internal public admin dashboard settings profile account login logout signup register password email notification message messages chat search filter sort pagination upload download export import report invoice payment order cart product price customer billing subscription plan feature flag metrics analytics tracking debug env release version update install setup init start stop run exec process thread pool lock channel signal timer scheduler cron retry fetch sync async promise observable subject emitter listener callback avatar badge banner breadcrumb checkbox dialog drawer dropdown menu navbar sidebar slider spinner switch tabs tag toast tooltip upload video audio player editor preview viewer canvas chart color font text label link tab panel section content wrapper overlay portal popover select option radio field group row column cell stack box flex container screen window frame scroll view draw render paint mount unmount effect memo ref key value pair entry record field meta info detail details summary overview home landing about contact pricing docs blog post posts comment comments like share follow feed timeline story stories media gallery photo photos album upload asset assets static vendor polyfill shim compat legacy util core runtime compiler emit diagnostics semantic syntax scanner printer checker binder program host watch module modules package packages workspace project projects task tasks issue issues label milestone release releases branch commit diff merge patch rebase tag tags remote origin";

const PKGS: &str = "react react-dom lodash express axios typescript webpack babel-loader eslint prettier jest mocha chai vite rollup esbuild next vue svelte angular rxjs moment dayjs date-fns uuid chalk commander yargs debug semver glob minimatch rimraf mkdirp fs-extra dotenv cors body-parser cookie-parser jsonwebtoken bcrypt mongoose sequelize prisma pg mysql2 redis ioredis socket.io ws graphql apollo-server zod yup joi ajv classnames clsx styled-components emotion tailwindcss postcss autoprefixer sass less core-js regenerator-runtime tslib source-map source-map-support acorn terser uglify-js browserslist caniuse-lite electron-to-chromium node-releases picocolors nanoid colorette cross-spawn which execa signal-exit graceful-fs micromatch braces fill-range to-regex-range is-number picomatch anymatch chokidar readdirp normalize-path glob-parent is-glob is-extglob fast-glob merge2 run-parallel queue-microtask reusify fastq camelcase decamelize kind-of isobject is-plain-object inherits safe-buffer string_decoder readable-stream util-deprecate once wrappy inflight balanced-match brace-expansion concat-map path-is-absolute ms supports-color has-flag ansi-styles color-convert color-name escape-string-regexp strip-ansi ansi-regex string-width emoji-regex is-fullwidth-code-point wrap-ansi cliui y18n get-caller-file require-directory punycode tr46 webidl-conversions whatwg-url node-fetch form-data asynckit combined-stream delayed-stream mime-types mime-db iconv-lite safer-buffer qs side-channel call-bind get-intrinsic has-symbols function-bind hasown es-errors es-define-property gopd define-data-property set-function-length object-inspect call-bind-apply-helpers dunder-proto es-object-atoms get-proto math-intrinsics json5 minimist resolve path-parse is-core-module supports-preserve-symlinks-flag lru-cache yallist convert-source-map gensync globals js-tokens loose-envify jsesc picocolors @jridgewell escalade update-browserslist-db electron playwright puppeteer storybook three d3 chart.js leaflet mapbox-gl firebase aws-sdk stripe twilio sharp jimp canvas pdfkit xlsx csv-parse papaparse marked markdown-it highlight.js prismjs katex mermaid immer redux zustand mobx jotai recoil swr react-query react-router react-hook-form formik framer-motion lottie-web swiper i18next intl-messageformat luxon numeral big.js decimal.js bignumber.js crypto-js tweetnacl elliptic bn.js hash.js sha.js pbkdf2 protobufjs long grpc nats kafkajs amqplib bullmq agenda node-cron winston pino morgan bunyan helmet compression multer busboy formidable sanitize-html dompurify jsdom cheerio htmlparser2 parse5 entities domhandler domutils css-select css-what nth-check boolbase";

const SCOPES: &str = "@babel @types @eslint @typescript-eslint @jest @vitejs @rollup @esbuild @swc @next @vue @angular @nestjs @aws-sdk @google-cloud @azure @radix-ui @mui @emotion @tanstack @trpc @prisma @sentry @opentelemetry @storybook @testing-library @tailwindcss @floating-ui @headlessui @heroicons @react-native @expo @apollo @graphql-tools @octokit @smithy @img";

const LOCALES: &str = "Base en en_GB en_AU fr fr_CA de es es_419 es_MX it ja ko zh_CN zh_TW zh_HK pt_BR pt_PT ru nl sv da fi nb pl tr cs hu el he ar th vi id ms uk ro sk hr ca hi sl sr lt lv et fa bn ta te mr gu kn ml am sw fil";

const FW_PRE: &str = "Core AV UI App Web Metal Media Network Security Foundation Quartz Image Audio Video Game Photo Map Health Home Cloud Store Contacts Calendar Speech Vision Sound Text Font Color Sync Device Accessibility Intents Siri Spotlight Finder Safari Mail Messages Notes Music TV Books Pass Wallet Shortcuts Focus Widget Live Share Screen Remote Family Fitness Weather Stocks News Podcasts Reminders Clock Camera Files Keychain Login Setup Update Backup Time Disk Storage Memory Power Thermal Display Graphics Input Touch Pencil Haptic Motion Location Bluetooth Wifi Cellular Phone Call Voice Dictation Translation Language Locale Calendar Event Reminder Alarm Notification Badge Banner Alert Sheet Menu Toolbar Sidebar Window Scene Session Activity Handoff Continuity AirPlay AirDrop iCloud CloudKit GameCenter";

const FW_SUF: &str = "Kit Foundation Services Core UI Support Framework Utilities Daemon Agent Internal Shared Server Client Engine Manager Store Provider Bridge Extension Plugin Runtime Model Data Sync Cache Helper";

const APPS: &str = "Google Chrome, Safari Technology Preview, Firefox, Arc, Brave Browser, Xcode Previews, Pages, Numbers, Keynote, GarageBand, iMovie, Final Cut Pro, Logic Pro, Affinity Designer, Pixelmator Pro, Sketch, Transmit, Tower, Fork, Sublime Text, BBEdit, Nova, TablePlus, Proxyman, Charles, Paw, Raycast, Alfred 5, Rectangle, Bartender 5, CleanShot X, Things 3, OmniFocus 4, Fantastical, Bear, Craft, Obsidian, Day One, 1Password 7, Bitwarden, NordVPN, Tunnelblick, Docker, OrbStack, UTM, Parallels Desktop, VMware Fusion, The Unarchiver, Keka, AppCleaner, IINA, VLC, Spotify Helper, Audio Hijack, Loopback, Kindle, Calibre, Zotero, Microsoft Word, Microsoft Excel, Microsoft PowerPoint, Microsoft Outlook, OneDrive, Dropbox, Google Drive, Box Sync, Zoom, Webex, Skype, Telegram, WhatsApp, Signal, Messenger, Steam, Epic Games Launcher, Blender, Unity Hub, Godot, Android Studio, IntelliJ IDEA, PyCharm, WebStorm, GoLand, CLion, RustRover, DataGrip, Rider, Sourcetree, GitHub Desktop, Insomnia, MongoDB Compass, Sequel Ace, DBeaver, Wireshark, Little Snitch, Lulu, Amphetamine, Magnet, Moom, Hammerspoon, Karabiner-Elements, iTerm, Warp, Alacritty, kitty, WezTerm, Ghostty";

const ELECTRON: &str = "Visual Studio Code, Slack, Discord, Notion, Cursor, Claude, Figma, Postman, Microsoft Teams, Obsidian Helper, Spotify, Linear, Loom";

const CRATES: &str = "serde serde_json serde_derive tokio tokio-macros futures futures-util anyhow thiserror clap clap_derive log env_logger tracing tracing-subscriber rand rand_core rand_chacha regex regex-syntax aho-corasick memchr itertools once_cell lazy_static bytes hyper http reqwest url percent-encoding idna unicode-normalization unicode-ident proc-macro2 quote syn libc cfg-if bitflags smallvec parking_lot lock_api crossbeam crossbeam-channel crossbeam-utils rayon rayon-core num-traits num-integer chrono time base64 sha2 digest generic-array typenum hex uuid toml indexmap hashbrown ahash getrandom ring rustls webpki tower tower-http axum actix-web warp sqlx diesel rusqlite tempfile walkdir ignore globset notify memmap2 byteorder flate2 miniz_oxide zstd tar zip image png jpeg-decoder wgpu winit egui bevy nom pest logos ropey tree-sitter";

const PYS: &str = "numpy pandas requests urllib3 certifi idna charset_normalizer six setuptools pip wheel packaging pyparsing python_dateutil pytz attrs click jinja2 markupsafe werkzeug flask django fastapi starlette pydantic pydantic_core uvicorn httpx httpcore anyio sniffio h11 typing_extensions sqlalchemy alembic psycopg2 redis celery boto3 botocore s3transfer jmespath pyyaml toml tomli filelock platformdirs virtualenv pytest pluggy iniconfig coverage black isort mypy ruff matplotlib seaborn scipy scikit_learn joblib threadpoolctl torch torchvision transformers tokenizers huggingface_hub safetensors tqdm regex sentencepiece protobuf grpcio openai anthropic tiktoken langchain aiohttp yarl multidict frozenlist aiosignal async_timeout pillow opencv_python networkx sympy mpmath jupyter ipython ipykernel traitlets tornado zmq nbformat nbconvert notebook";

const BREWS: &str = "git node python@3.12 python@3.11 openssl@3 ca-certificates readline sqlite xz zstd lz4 icu4c libyaml pkgconf cmake ninja gettext pcre2 gmp mpfr libmpc isl gcc llvm rust go deno bun ffmpeg imagemagick libpng jpeg-turbo libtiff webp giflib freetype fontconfig harfbuzz cairo pango glib gdk-pixbuf librsvg ghostscript poppler tesseract leptonica opencv protobuf grpc abseil re2 c-ares libuv libevent libssh2 curl wget gnupg pinentry gnutls nettle libtasn1 p11-kit unbound jq yq ripgrep fd bat eza fzf zoxide starship tmux neovim vim htop btop tree watch coreutils findutils gnu-sed gawk grep make autoconf automake libtool m4 bison flex postgresql@16 mysql redis mongodb-community nginx caddy terraform kubectl helm k9s awscli azure-cli gh lazygit git-lfs pre-commit shellcheck shfmt hadolint act colima lima qemu";

const TOOLS: &str = "ls cp mv rm cat echo grep sed awk find xargs sort uniq head tail wc cut tr tee touch chmod chown ln mkdir rmdir ps kill top df du mount tar gzip gunzip zip unzip curl ssh scp rsync git make cc clang ld ar nm strip otool lipo codesign security plutil defaults launchctl diskutil hdiutil sw_vers system_profiler pmset caffeinate open pbcopy pbpaste say screencapture sips mdfind mdls xattr dscl dig nslookup ping traceroute netstat ifconfig route arp lsof fs_usage dtrace sample spindump leaks vmmap heap malloc_history zsh bash sh python3 perl ruby swift xcrun xcodebuild";

const ORGS: &str = "acme globex initech umbrella hooli freelance side oss clients sandbox playground archive";
