#!/usr/bin/env python3
"""Arbitrary-disk differential check: candidate engine vs reference engine.

Usage: diff-check.py <candidate> <reference> [--big]

Writes synthetic FSDISK01 disks to ./.diff-check/, types query chains
(every prefix of each line, like the bench harness), and byte-compares
each engine's full stdout. Any difference is a correctness defect.

Disks cover: empty/root-only, dotted/leading-dot names, long names,
non-ASCII, hidden/symlink/other kinds, deep paths, huge sizes, future/old
mtimes, a deterministic pseudo-random disk, and a >SELECTIVE fanout disk.
Chains cover: positive multi-token, negation, exact/prefix/suffix modes,
ext/type/kind/in/size/mtime/re/path/limit filters, limit changes mid-chain,
regex changes mid-chain, typos, non-ASCII, empty and invalid queries.
"""
import pathlib
import random
import struct
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
WORK = ROOT / ".diff-check"
NOW = 1_700_000_000

FILE, DIR, LINK, OTHER, HIDDEN = 0, 1, 2, 3, 4


def _req(cond, msg):
    """Assert-free strict check: still raises under `python -O`."""
    if not cond:
        raise AssertionError(msg)


def disk(records):
    """records: list of (parent_idx_or_FFFFFFFF, kind, size, mtime, name_bytes)."""
    chunks = [b"FSDISK01" + struct.pack("<QI", len(records), NOW)]
    for parent, kind, size, mtime, name in records:
        chunks.append(struct.pack("<IBQIH", parent, kind, size, mtime, len(name)) + name)
    return b"".join(chunks)


def write(name, blob):
    WORK.mkdir(exist_ok=True)
    p = WORK / (name + ".bin")
    p.write_bytes(blob)
    return p


def run(engine, disk_path, keys):
    out = subprocess.check_output([engine, disk_path], input=("\n".join(keys) + "\n").encode(), timeout=600)
    _req(out.startswith(b"ready\n"), f"{engine} did not print ready")
    body = out[len(b"ready\n"):]
    # Strict framing: per key, consume row lines until a REQUIRED blank
    # terminator; an empty answer is exactly one newline. Missing
    # terminator, dropped query, or trailing bytes all fail loud.
    lines = body.split(b"\n")
    answers, i = [], 0
    for _ in keys:
        rows = []
        while i < len(lines) and lines[i] != b"":
            rows.append(lines[i])
            i += 1
        # A terminator must be a real blank line: the split artifact's
        # final "" (from a trailing newline) does not count.
        _req(i < len(lines) - 1,
             f"{engine}: answer for key {len(answers)} lacks blank-line terminator")
        i += 1  # consume terminator
        answers.append(b"\n".join(rows))
    _req(i == len(lines) - 1 and lines[i] == b"",
         f"{engine}: {len(lines) - i} stray line(s) after {len(keys)} answers")
    return answers


def validate_canonical(recs, name):
    """Canonical disk shape: record0 is the root (parent 0, kind&3==DIR);
    every later record's parent is an earlier DIR record (parent-before-
    child) so reconstructed physical paths exist and are unique."""
    _req(recs, f"{name}: empty record list")
    p0, k0 = recs[0][0], recs[0][1]
    _req(p0 == 0 and (k0 & 3) == DIR, f"{name}: root not canonical (parent={p0:#x} kind={k0})")
    paths = [b""]
    for i, (parent, kind, _size, _mtime, nm) in enumerate(recs[1:], start=1):
        _req(parent < i, f"{name}: record {i} parent {parent} not before child")
        _req((recs[parent][1] & 3) == DIR, f"{name}: record {i} parent {parent} kind {recs[parent][1]} not DIR")
        path = paths[parent] + b"/" + nm if paths[parent] else nm
        paths.append(path)
    _req(len(set(paths[1:])) == len(paths) - 1, f"{name}: duplicate physical path(s)")


def prefixes(chain):
    """Every keystroke of every line of a chain, in order."""
    keys = []
    for line in chain:
        for i in range(1, len(line) + 1):
            keys.append(line[:i])
    return keys


def build_disks():
    ds = {}

    # Empty disk: just the canonical root (parent 0 = self).
    ds["empty"] = write("empty", disk([(0, DIR, 0, NOW, b"")]))

    # STRESS EXTRA (non-canonical): root-only disk with the 0xFFFFFFFF
    # parent sentinel a real disk writer could emit. Kept labeled.
    ds["noncanon_emptyroot"] = write(
        "noncanon_emptyroot", disk([(0xFFFFFFFF, DIR, 0, NOW, b"")]))

    # Adversarial names under root and a couple of dirs.
    names = [
        b".", b"..", b"...", b"..abc", b".abc", b".abc.", b"foo..abc",
        b"a" + b"x" * 150 + b"bcd",  # very long name -> negative scores
        b"x" * 300,
        b"main.rs", b"mian.rs", b"MAIN.RS", b"Main.Rs",
        b"caf\xc3\xa9", b"caf\xc3\xa9.rs", b"caf\xc3\xa9 ".rstrip(),
        ".txt".encode(), "файл.rs".encode(), "🚀rocket.rs".encode(),
        b"a b c d e f", b" spaced name ", b" ", b"   ",
        b"0abc", b"9z", b"a0", b"123456789",
        b"UPPER", b"MiXeD", b"with_underscore", b"with-dash", b"with.dot.dots",
        b"trailing.", b".hidden", b"~tilde", b"semi;colon", b"quote'quote",
        b"node_modules", b"index.js", b"index.ts", b"index",
        b"src", b"test", b"manifest.json", b"manifesto",
        b"a" * 80, b"a" * 81,  # len-penalty boundary 80
        b"x" * 10 + b".rs", b"ab cd", b"ab  cd", b"ab.cd",
    ]
    recs = [(0, DIR, 0, NOW, b"")]
    recs += [(0, DIR, 0, NOW, b"d"), (0, DIR, 0, NOW, b".dotdir"), (0, DIR, 0, NOW, b"Library")]
    for i, nm in enumerate(names):
        parent = [0, 1, 2, 3][i % 4]
        recs.append((parent, FILE, i * 7, NOW - i * 3600, nm))
    # size/mtime extremes
    recs.append((0, FILE, 0, 0, b"zero.bin"))
    recs.append((0, FILE, 2**40, NOW + 10_000_000, b"future.bin"))  # mtime in the "future"
    recs.append((0, FILE, 2**31 + 5, 1, b"huge.bin"))
    recs.append((0, LINK, 0, NOW, b"alink"))
    recs.append((0, OTHER, 0, NOW, b"adev"))
    recs.append((0, FILE | HIDDEN, 5, NOW, b"hid"))
    # deeper path: /Users/bench/Developer/x
    di = len(recs)
    recs += [(0, DIR, 0, NOW, b"Users"), (di, DIR, 0, NOW, b"bench"), (di + 1, DIR, 0, NOW, b"Developer"), (di + 2, DIR, 0, NOW, b"x")]
    recs += [(di + 3, FILE, 100, NOW, b"main.rs"), (di + 3, FILE, 200, NOW, b"index.ts")]
    validate_canonical(recs, "adversarial")
    ds["adversarial"] = write("adversarial", disk(recs))

    # Pseudo-random disk: ~60k entries, mixed names/kinds/flags/depths.
    rng = random.Random(12345)
    recs = [(0, DIR, 0, NOW, b"")]
    words = [b"main", b"index", b"src", b"test", b"config", b"node", b"lib",
             b"photo", b"screenshot", b"docker", b"compose", b"webpack",
             b"readme", b"license", b"Makefile", b"utils", b"invoice",
             b"settings", b"module", b"handler", b"server", b"client"]
    exts = [b"rs", b"ts", b"js", b"json", b"md", b"txt", b"png", b"log", b"py"]
    dirs = [0]
    used = {0: set()}  # parent idx -> child names, for unique paths
    for i in range(60000):
        if rng.random() < 0.06 and len(dirs) < 4000:
            nm = rng.choice(words) + str(rng.randrange(100)).encode()
            parent = rng.choice(dirs)
            while nm in used[parent]:
                nm += b"~d"
            used[parent].add(nm)
            recs.append((parent, DIR, 0, NOW - rng.randrange(10**8), nm))
            dirs.append(len(recs) - 1)
            used[len(recs) - 1] = set()
            continue
        parts = []
        for _ in range(rng.randrange(1, 4)):
            w = rng.choice(words)
            if rng.random() < 0.3:
                w += str(rng.randrange(200)).encode()
            if rng.random() < 0.1:
                w = w.upper()
            if rng.random() < 0.05:
                w = b"." + w
            parts.append(w)
        sep = rng.choice([b"", b"-", b"_", b" ", b"."])
        nm = sep.join(parts)
        if rng.random() < 0.7:
            nm += b"." + rng.choice(exts)
        kind = rng.choice([FILE, FILE, FILE, FILE, LINK, OTHER, FILE | HIDDEN])
        parent = rng.choice(dirs)
        # Unique physical paths: suffix dupes within one parent.
        while nm in used.setdefault(parent, set()):
            nm = nm + b"~" + str(rng.randrange(10**4)).encode()
        used[parent].add(nm)
        recs.append((parent, kind, rng.randrange(2**20), NOW - rng.randrange(10**8), nm))
    validate_canonical(recs, "random60k")
    ds["random60k"] = write("random60k", disk(recs))

    # Fanout disk: many entries share few distinct names (forces full/byname).
    recs = [(0, DIR, 0, NOW, b"")]
    parents = [0]
    for i in range(40):
        recs.append((0, DIR, 0, NOW, f"dir{i}".encode()))
        parents.append(len(recs) - 1)
    taken = set()
    for i in range(300000):
        nm = (b"common" if i % 7 == 0 else b"file%05d" % i) + (b".rs" if i % 3 else b".txt")
        p = rng.choice(parents)
        # Unique physical paths despite the intentional name fanout:
        # a (parent, name) that already exists gets a ~fN suffix (keeps
        # the 'common'/'fileNNNNN' prefix fanout the disk stresses).
        base, n = nm, 1
        while (p, nm) in taken:
            n += 1
            nm = base + b"~f" + str(n).encode()
        taken.add((p, nm))
        recs.append((p, FILE, i, NOW - (i % 10**6), nm))
    validate_canonical(recs, "fanout300k")
    ds["fanout300k"] = write("fanout300k", disk(recs))

    # Seed-boundary disk: 7 names x 10k dirs, >60k matches -> byname path.
    # Entry ids interleave j*7+l so the merge's seed floor lands inside
    # the true top-limit: the boundary seed key must re-enter, not drop.
    recs = [(0, DIR, 0, NOW, b"")]
    for j in range(10000):
        recs.append((0, DIR, 0, NOW, f"d{j}".encode()))
    for j in range(10000):
        for l in range(7):
            recs.append((1 + j, FILE, 10, NOW, b"z" + b"abcdefg"[l:l + 1]))
    validate_canonical(recs, "seedbound")
    ds["seedbound"] = write("seedbound", disk(recs))

    # 64/128-boundary disk: exactly-128-byte names whose only matches live
    # beyond byte 64 (plus 64/65/127/129 controls). Guards masked-window
    # hi-half handling: a hi mask of 0 silently drops every hit past
    # byte 63. Chars used are also chosen so folded-case hits exist only
    # in the hi half ('z' at 64, 'Z' query folding onto it is covered by
    # 'ZZZ'-style chains below).
    bnames = [
        b"x" * 64 + b"needle" + b"x" * 58,    # 128B, 'needle' only at 64..70
        b"k" * 64 + b"Z" + b"k" * 63,         # 128B, single 'Z' at byte 64
        b"p" * 63 + b"ab" + b"p" * 63,        # 128B, 'ab' spans 63..65
        b"x" * 63 + b"needle" + b"x" * 58,    # 127B, match starts at 63
        b"x" * 65 + b"needle" + b"x" * 58,    # 129B, above the window gate
        b"k" * 58 + b"needle",                # 64B, lo-half edge
        b"k" * 64 + b"n",                     # 65B, first hi-half byte
        b"s" * 124 + b"tail",                 # 128B, suffix at 124..128
        b"needle" + b"x" * 121,               # 128B, prefix match at 0
        b"w" * 64 + b"abcdefghijklmnop" + b"w" * 48,  # 128B, len-16 token >64
        b"w" * 64 + b"abcdefghijklmnopq" + b"w" * 47, # 128B, len-17 token >64
        b"q" * 128,                           # 128B, matches nothing
        b"camelhump" * 16,                    # 128B repeated word
    ]
    _req(len(bnames[0]) == 128 and len(bnames[3]) == 127 and len(bnames[4]) == 129,
         "bounds128 fixture name lengths wrong")
    recs = [(0, DIR, 0, NOW, b"")]
    recs += [(0, FILE, 100 + i, NOW - 200 - i, nm) for i, nm in enumerate(bnames)]
    validate_canonical(recs, "bounds128")
    ds["bounds128"] = write("bounds128", disk(recs))

    # Negation-extension straddle disks (negbit narrowing, u/16 branch).
    # The SAME extension chain `!te -> !tes -> !tesq` runs on every disk;
    # the narrow path picks the sparse set-bit eval iff
    # prev_pop * 16 <= u.  Four disks straddle the boundary:
    #   negsparse 4/1600 (sparse), negedge 100/1600 (== u/16, sparse side),
    #   negover 101/1600 (dense side), negdense 400/1600 (dense).
    # Counts are asserted from the generated name list, not assumed.
    for dname, te_count, total in [("negsparse", 4, 1600), ("negedge", 100, 1600),
                                   ("negover", 101, 1600), ("negdense", 400, 1600)]:
        names = []
        for i in range(te_count):
            # 'test' contains "tes"; 'item' contains "te" only.
            names.append((b"test" if i % 2 == 0 else b"item") + b"%05d" % i)
        fills = [b"alpha", b"bravo", b"cab", b"dir", b"flux", b"grid", b"hub",
                 b"ion", b"jar", b"kit", b"lag", b"mon", b"nap", b"ox", b"pan",
                 b"qua", b"ray", b"sky", b"urn", b"via", b"wax", b"yak", b"zig"]
        i = 0
        while len(names) < total:
            names.append(fills[i % len(fills)] + b"%05d" % (i // len(fills)))
            i += 1
        tes = sum(1 for nm in names if b"tes" in nm)
        te = sum(1 for nm in names if b"te" in nm)
        _req(te == te_count, f"{dname}: 'te' match count {te} != {te_count}")
        _req(tes == (te_count + 1) // 2, f"{dname}: 'tes' match count {tes} unexpected")
        _req(len(set(names)) == len(names), f"{dname}: duplicate names")
        recs = [(0, DIR, 0, NOW, b"")]
        recs += [(0, FILE, i + 1, NOW - i, nm) for i, nm in enumerate(names)]
        validate_canonical(recs, dname)
        ds[dname] = write(dname, disk(recs))

    # neguni: unicode/multibyte + 127/128/129-byte names + mode families.
    unames = [
        "caf\u00e9".encode(), "caf\u00e9cake".encode(), "\u00fcnix".encode(),
        "\u00fcnsere".encode(), "a\u00fcns".encode(), "n\u00fcns".encode(),
        "\u00e5ngstr\u00f6m".encode(), "x\u00e5b".encode(), "\u00e5bc".encode(),
        "h\u00e5b".encode(), "\u00fcnn".encode(), "\u00e5".encode(),
        b"x" * 62 + "\u00fc".encode() + b"x" * 63,   # 127B, 2-byte char at 62-63
        b"x" * 62 + "\u00fc".encode() + b"x" * 64,   # 128B
        b"x" * 62 + "\u00fc".encode() + b"x" * 65,   # 129B
        b"k" * 62 + "\u00e5".encode() + b"k" * 64,   # 128B, \u00e5 at 62-63
        b"code", b"inode", b"xnode", b"abmode", b"dcode", b"mynode",
        b"plainde", b"wonode", b"nodemcu", b"nodular", b"nod",
        b"abc", b"xabc", b"yxabc", b"abcy", b"xabcy", b"zabcz",
        b"test", b"testi", b"besti", b"xesti", b"testa", b"bestin",
        b"xestin", b"testin", b"zesti", b"tosti", b"tests", b"text",
        b"foo", b"bar", b"plain", b"random", b"!x", b"~!test",
    ]
    _req(len(unames[12]) == 127 and len(unames[13]) == 128 and len(unames[14]) == 129,
         "neguni boundary names wrong length")
    _req(len(set(unames)) == len(unames), "neguni: duplicate names")
    recs = [(0, DIR, 0, NOW, b"")]
    recs += [(0, FILE, 10 + i, NOW - i * 3600, nm) for i, nm in enumerate(unames)]
    validate_canonical(recs, "neguni")
    ds["neguni"] = write("neguni", disk(recs))
    validate_canonical([(0, DIR, 0, NOW, b"")], "empty")
    return ds


CHAINS = [
    # negative-score carry repro chains
    ["ab c", "ab d"],
    ["a", "a d", "ab d"],
    # fast1/limit/filter cache interplay
    ["a", "a limit:1", "a limit:20000", "a", "kind:file a", "kind:dir a", "a"],
    ["a", "limit:120 a", "limit:0 a", "limit:1 a", "limit:100 a", "limit:200 a", "a limit:50"],
    ["limit:0", "limit:1", "limit:100", "limit:5000"],
    # negations
    ["!n", "!t", "!te", "!tes", "!test", "!test m", "!test main"],
    ["main", "!test main", "main !test", "!test !main"],
    ["!node_modules index.js"],
    # modes
    ["'t", "'te", "'test", "'main", "'a"],
    ["^a", "^src", "^main", "^"],
    ["a$", "rs$", ".json$", "$"],
    # filters
    ["ext:", "ext:r", "ext:rs", "ext:rs i", "ext:rs index"],
    ["ext:md readme", "ext:jpg,txt a", "ext:zzzz a"],
    ["type:image", "type:image s", "type:image screenshot", "type:app a"],
    ["kind:dir", "kind:dir node_modules", "kind:file a", "kind:link", "kind:other"],
    ["in:", "in:/", "in:~", "in:~/Developer", "in:~/Developer main", "in:/nonexistent-path-xyz a"],
    ["size:>10mb", "size:>10mb m", "size:<1 a", "size:0 a", "size:>0 a", "size:1..100 a"],
    ["mtime:<7d", "mtime:<7d log", "mtime:>1y a", "mtime:0 a"],
    ["re:", "re:m", "re:main", "re:^index", "re:[", "re:a.*b"],
    ["path:", "path:src", "path:Users", "path:a/b"],
    # multi-token fuzzy + typos
    ["src index", "docker compose", "photo 2024", "webpack config", "budget xlsx"],
    ["mian.rs", "min", "manifest", "manif"],
    # leading dots / specials
    [".", "..", ".a", ".zshrc", " ", "  ", "\t"],
    ["a/b", "a//b", "a/"],
    # non-ASCII
    ["caf", "café", "", "🚀"],
    # empty query + repeats + cache churn
    ["", " ", "a", "", "a b", ""],
    ["z", "zz", "zzzz", "zzzzzz", "x", "x "],
    # mixed tokens/filters churn (name-cache reuse across changing filters)
    ["index", "ext:rs index", "index ext:ts", "ext:md index", "index", "!lib index"],
    ["a size:>1mb", "size:>1mb a", "a", "a mtime:<1d", "mtime:<1d a"],
    # 5+ letter typo toggles across chains
    ["manifest", "manifes", "manife", "manifestx", "manifest x"],
    # limit moves both directions mid-chain (paging safety)
    ["a limit:5", "a limit:50", "a limit:5", "a limit:500", "a limit:2"],
    # pos-empty filter transitions: ext/re/neg/kind flip mid-chain so the
    # selective/walk paths and name-cache invalidation all get exercised
    ["ext:rs", "re:m", "!test", "ext:", "re:", "!n", "kind:dir", "ext:md",
     "re:x", "!l", "kind:file", "ext:rs i", "re:main !t", "!x re:a"],
    # pos-empty cache transitions: identical name-side predicates under
    # different stat filters/limits must hit the posneg cache; changed
    # name-side keys must miss it. Covers limit 0/1/50, kind/scope/
    # path_re/ext/neg/name_re/mode on both sides of a repeat.
    ["!n", "kind:file !n", "kind:dir !n", "size:>1mb !n", "mtime:<7d !n",
     "limit:1 !n", "limit:50 !n", "limit:0 !n", "scope:/ !n", "!n",
     "re:x", "re:x limit:1", "kind:file re:x", "path:a re:x",
     "size:>10mb re:x", "re:x", "!test", "kind:file !test", "limit:0 !test",
     "limit:1 !test", "!test m", "!test", "ext:rs", "ext:rs limit:0",
     "ext:rs limit:1", "kind:dir ext:rs", "ext:rs", "kind:file",
     "kind:dir", "kind:file", "size:>0", "mtime:>0", "kind:file"],
    # neg-mode transitions: fuzzy/exact/prefix/suffix neg tokens flip
    # mode mid-chain, same text — posneg cache keys must distinguish
    ["!test", "!^test", "!test$", "!test", "!^test", "kind:file !test",
     "!test$", "!test", "size:>1mb !test$", "a !test", "!test a", "!a !test"],
    # byname seed-boundary: >limit matches, boundary key == seed floor
    ["z", "z limit:1", "z limit:2", "z limit:49", "z limit:50", "z limit:51",
     "z limit:60", "z limit:100", "z limit:120", "z limit:0", "z limit:20000",
     "limit:50 z", "limit:1 z", "za", "zb", "zg", "z limit:5 z"],
    # 64/128-byte boundary chain: exercises fuzzy/exact/prefix/suffix and
    # negation on exactly-128B names with required chars only >byte 64,
    # q lengths 0/1/2/16/17, plus a cache transition re-query.
    ["needle", "'needle", "^needle", "tail$", "needle$",
     "abcdefghijklmnop", "abcdefghijklmnopq", "z", "Z", "ab", "x", "n", "q",
     "needle z", "!needle", "!tail", "'z", "^n", "tail$ z", "camelhump",
     "hump", "cam", "needle", "needle z", "size:>0", "ext:txt", ""],
]

# RAW chains: COMPLETE queries sent verbatim (no prefix expansion). Prefix
# typing can never produce some adjacent-query relations the posneg cache
# sees in real use — e.g. a full `!x$` query is always preceded by its own
# non-suffix prefixes, so typed streams never exercise suffix-mode
# narrowing, non-prefix containment, or mode-flip transitions between
# complete queries. Each chain's expected narrowing/rebuild pattern is
# documented inline (negbit test plan gaps a-f + meta controls).
RAW_CHAINS = [
    # gap a: suffix-mode neg extension — adjacent full queries.
    ["!e$", "!te$", "!ste$", "!teste$"],          # narrow, narrow, narrow
    ["!teste$", "!ste$", "!te$", "!e$"],          # shrink -> all rebuilds
    ["!e$ !te$", "!te$ !ste$", "!ste$ !teste$"],  # pairwise suffix narrows
    ["!de$ !ode$", "!ode$ !de$"],                 # reject then reject
    # gap b: Exact non-prefix containment + reverse/self rejects.
    ["!abc", "!xabc", "!yxabc", "!xabc", "!abc"],
    ["!ab", "!cab", "!abc"],                      # narrow then reject
    ["!abc !d", "!xabc !de", "!abc !d"],
    ["!abc", "!acb", "!abc"],                     # same-len non-match rejects
    # gap c: Prefix-mode neg chains + mode flips.
    ["!^n", "!^no", "!^nod", "!^node"],           # all narrow
    ["!^node", "!^nod", "!^no", "!^n"],           # shrink -> rebuilds
    ["!^no", "!no", "!no$", "!no", "!^no"],       # mode flips -> rebuilds
    ["!'no", "!no", "!'no"],                      # ' == Exact semantics
    # gap e: token-count guards, equal-pair, positive-token guard.
    ["!a", "!a !b", "!a", "!a !b"],               # count 1->2->1->2 rebuilds
    ["!a !b", "!ab !b", "!a !bb"],                # extend first / second
    ["!a !b", "!x y !z", "!a !b"],                # pos query leaves cache
    ["!a !b", "", "!a !b"],                       # empty query leaves cache
    # stale-entry discipline (meta): rebuild replaces, next must not
    # narrow from the wrong stored key.
    ["!test", "!xyz", "!testi", "!xyz", "!test"],
    # gap f: unicode / byte-boundary extensions (byte-wise relations).
    ["!\u00e5", "!\u00e5b", "!\u00e5bc", "!\u00e5b"],
    ["!\u00fcn", "!\u00fcns", "!\u00fcnse", "!\u00fcns"],
    # u/16 straddle chains (same on negsparse/negedge/negover/negdense).
    ["!te", "!tes", "!te", "!tesq", "!te"],
    ["!item", "!items", "!item"],
    # gap g: fuzzy-negation syntax — parses as ordinary positive fuzzy on
    # stock engines (harmless identical answers); only mutant builds give
    # `~!` a fuzzy-negated meaning.
    ["~!test", "~!testi", "~!testin", "~!test"],
    # pure rejects: different text, different count, shrink.
    ["!abc", "!def", "!abd", "!abc"],
    ["!a !b !c", "!a !b", "!a !b !c"],
    # stat/limit invariance on the same name key (hits, not narrows).
    ["!te", "kind:file !te", "size:>0 !te", "limit:1 !te", "!tes", "limit:1 !tes"],
    # name_re interplay: re differs -> rebuild; same re+extends -> narrow.
    ["re:t !te", "re:t !tes", "re:x !te", "!te"],
]


def main():
    args = sys.argv[1:]
    if len(args) < 2:
        print("usage: diff-check.py CAND REF [--big]")
        sys.exit(2)
    cand, ref = args[0], args[1]
    disks = build_disks()
    if "--big" not in args:
        disks = {k: v for k, v in disks.items() if k != "fanout300k"}
    chains = [(f"chain{i}", prefixes(c)) for i, c in enumerate(CHAINS)]
    chains += [(f"raw{i}", keys) for i, keys in enumerate(RAW_CHAINS)]
    fails = 0
    for dname, dpath in disks.items():
        for label, keys in chains:
            a = run(cand, dpath, keys)
            b = run(ref, dpath, keys)
            if a == b:
                print(f"ok   {dname} {label} ({len(keys)} keys)")
                continue
            fails += 1
            # Show the first answer block that differs.
            for k in range(max(len(a), len(b))):
                x = a[k] if k < len(a) else b"<missing>"
                y = b[k] if k < len(b) else b"<missing>"
                if x != y:
                    print(f"FAIL {dname} {label} key {k} {keys[k] if k < len(keys) else '?'!r}")
                    print(f"  cand: {x[:300]!r}")
                    print(f"  ref : {y[:300]!r}")
                    break
    print("FAILURES:", fails)
    sys.exit(1 if fails else 0)


if __name__ == "__main__":
    main()
