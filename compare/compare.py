#!/usr/bin/env python3
"""Time several search engines on the same keystrokes.

  compare.py mkdisk <dir> <out.bin>     write <dir> as a disk file, at /Users/bench/<name>
  compare.py gen <bin> <out.txt> [--seed N] [--count N]   random queries from the disk's real names
  compare.py run <trace> --disk <bin> [--dir <dir>] --engine name=path ... --dir-engine name=path ...

Every engine speaks bench/PROTOCOL.md: start with one argument, print
"ready", then answer one query per stdin line, ending with a blank line.
--engine gets the disk file, --dir-engine gets the directory itself.

A trace line is a query, optionally a tab and the file it should find (a path
relative to the corpus root). Every prefix of the query is timed, as if typed.
Queries with mtime: are skipped: not every engine can pin its clock to the
disk's. The engines run PASSES times in rotated order; the table shows the
median pass and the spread. Output is markdown tables.

With two or more engines, a second table compares each engine with the first
one query at a time: the geometric mean of per-query time ratios, so one slow
query can't carry the total, how many queries got faster or slower, and the
mean per query kind. gen writes a fresh trace of every query kind, so a
change can be checked on queries nobody has tuned against.
"""

import math
import os
from array import array
import random
import re
import statistics
import struct
import subprocess
import sys
import time

ROUNDS = 5  # timed rounds per pass, after one warmup
PASSES = 3
BAND = 0.05  # a query is faster or slower only past this ratio change; less is noise


def mkdisk(src, out):
    recs = [(0, 1, 0, 0, b"")]  # the root
    queue = []

    def push(parent, name, kind, size, mtime):
        hidden = 4 if name.startswith(b".") else 0
        recs.append((parent, kind | hidden, size, mtime, name))
        return len(recs) - 1

    parent = 0
    for name in (b"Users", b"bench"):
        parent = push(parent, name, 1, 0, 0)
    queue.append((push(parent, os.fsencode(os.path.basename(os.path.abspath(src))), 1, 0, 0), src))
    while queue:  # breadth-first, so parents come before children
        parent, path = queue.pop(0)
        for e in sorted(os.scandir(path), key=lambda e: e.name):
            st = e.stat(follow_symlinks=False)
            kind = (
                2
                if e.is_symlink()
                else 1
                if e.is_dir(follow_symlinks=False)
                else 0
                if e.is_file(follow_symlinks=False)
                else 3
            )
            i = push(parent, os.fsencode(e.name), kind, st.st_size, int(st.st_mtime))
            if kind == 1:
                queue.append((i, e.path))
    buf = bytearray(b"FSDISK01" + len(recs).to_bytes(8, "little") + int(time.time()).to_bytes(4, "little"))
    for parent, kind, size, mtime, name in recs:
        buf += parent.to_bytes(4, "little") + bytes([kind]) + size.to_bytes(8, "little")
        buf += mtime.to_bytes(4, "little") + len(name).to_bytes(2, "little") + name
    with open(out, "wb") as f:
        f.write(buf)
    print(f"{out}: {len(recs)} entries")


SAFE = re.compile(rb"[A-Za-z0-9][A-Za-z0-9._-]*")  # a name every query parser reads as one word


def gen(disk, out, seed, count):
    """Write <count> random queries built from real file and folder names."""
    b = open(disk, "rb").read()
    n = int.from_bytes(b[8:16], "little")
    parent, kind, at = array("I", bytes(4 * n)), bytearray(n), array("Q", bytes(8 * n))  # at: offset of each name
    p = 20
    for i in range(n):
        parent[i], kind[i], _, _, ln = struct.unpack_from("<IBQIH", b, p)
        at[i] = p + 19
        p += 19 + ln
    name = lambda i: b[at[i] : at[i] + int.from_bytes(b[at[i] - 2 : at[i]], "little")]
    rng = random.Random(seed)

    def pick():
        """A random visible file under the home folder: (its name, the folders below home)."""
        while True:
            i = rng.randrange(1, n)
            if kind[i] != 0:  # plain visible file
                continue
            dirs = []
            j = parent[i]
            while j:
                dirs.append(name(j))
                j = parent[j]
            dirs = dirs[::-1][2:]  # drop Users, bench
            nm = name(i)
            if dirs and all(SAFE.fullmatch(d) for d in dirs + [nm]) and len(nm.split(b".")[0]) >= 3 and typed(nm):
                return nm.decode(), [dirs[0].decode()] + [
                    d.decode() for d in dirs[1:] if typed(d)
                ]  # top folder first, for in:

    def typed(s):
        """Names people type: not hashes, uuids or temp folders."""
        # ponytail: a digit-share cutoff also drops names like IMG_2023; a word list would keep them.
        return len(s) <= 40 and sum(c in b"0123456789" for c in s) <= len(s) // 4

    def frag(s):
        """What someone types for s: its first few letters."""
        return s[: rng.randint(3, max(3, min(len(s), 10)))]

    def typo(s):
        if len(s) < 5:
            return s
        k = rng.randrange(len(s) - 1)
        return s[:k] + s[k + 1] + s[k] + s[k + 2 :]

    size = lambda: rng.choice("<>") + rng.choice(["1kb", "100kb", "1mb", "10mb"])
    kinds = {
        "name": lambda nm, st, ext, d: nm,
        "start": lambda nm, st, ext, d: frag(st),
        "typo": lambda nm, st, ext, d: typo(st),
        "folder word": lambda nm, st, ext, d: f"{frag(rng.choice(d))} {frag(st)}",
        "ext": lambda nm, st, ext, d: f"ext:{ext} {frag(st)}" if ext else f"ext:txt {frag(st)}",
        "type": lambda nm,
        st,
        ext,
        d: f"type:{rng.choice(['image', 'video', 'audio', 'doc', 'code', 'archive'])} {frag(st)}",
        "kind": lambda nm, st, ext, d: f"kind:dir {frag(rng.choice(d))}",
        "in": lambda nm, st, ext, d: f"in:~/{d[0]} {frag(st)}",
        "size": lambda nm, st, ext, d: f"size:{size()} {frag(st)}",
        "negate": lambda nm, st, ext, d: f"!{rng.choice(pick()[1])} {frag(st)}",
        "mode": lambda nm, st, ext, d: rng.choice(
            [f"^{frag(st)}", f"{nm[-rng.randint(3, len(nm)):]}$", f"'{frag(st)}"]
        ),
    }
    order = [k for _ in range(-(-count // len(kinds))) for k in kinds][:count]
    rng.shuffle(order)
    with open(out, "w") as f:
        f.write(f"# compare.py gen {os.path.basename(disk)} --seed {seed} --count {count}\n")
        for k in order:
            nm, d = pick()
            st, _, ext = nm.rpartition(".") if "." in nm[1:] else (nm, "", "")
            f.write(kinds[k](nm, st, ext, d) + "\n")
    print(f"{out}: {count} queries, seed {seed}")


def kind_of(q):
    """The query kind, read from the query itself, so any trace can be broken down."""
    words = q.split()
    for w in words:
        if ":" in w and w.split(":")[0] in ("ext", "type", "kind", "in", "size"):
            return w.split(":")[0]
    if any(w.startswith("!") for w in words):
        return "negate"
    if any(w[0] in "^'" or w.endswith("$") for w in words):
        return "mode"
    return "words" if len(words) > 1 else "word"


def trace(path):
    rows = []
    for line in open(path, encoding="utf-8"):
        line = line.rstrip("\n")
        if line.strip() and not line.startswith("#") and "mtime:" not in line:
            q, _, want = line.partition("\t")
            rows.append((q, want))
    return rows


def run_engine(cmd, keys):
    """Per-key median ms, the answers, startup s, peak RSS MB, unstable keys."""
    t = time.perf_counter()
    p = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE)
    if p.stdout.readline() != b"ready\n":
        sys.exit(f"{cmd[0]}: no ready line")
    startup = time.perf_counter() - t
    lines = [k.encode() + b"\n" for k in keys]
    times = [[] for _ in keys]
    raw = [[] for _ in keys]  # every round's response, decoded after timing
    for r in range(ROUNDS + 1):
        for i, q in enumerate(lines):
            got = []
            t0 = time.perf_counter_ns()
            p.stdin.write(q)
            p.stdin.flush()
            while (line := p.stdout.readline()) not in (b"\n", b""):
                got.append(line)
            t1 = time.perf_counter_ns()
            if line == b"":
                sys.exit(f"{cmd[0]}: exited early on {keys[i]!r}")
            if r:
                times[i].append(t1 - t0)
            raw[i].append(b"".join(got))
    p.stdin.close()
    _, _, ru = os.wait4(p.pid, 0)
    rss_mb = ru.ru_maxrss / (1 << 20 if sys.platform == "darwin" else 1 << 10)
    unstable = sum(len(set(rs)) > 1 for rs in raw)
    answers = [[rel(l.partition("\t")[2]) for l in rs[0].decode(errors="replace").splitlines()] for rs in raw]
    return [statistics.median(t) / 1e6 for t in times], answers, startup, rss_mb, unstable


def rel(path):
    """/Users/bench/<corpus>/a/b -> a/b; fff's paths are already relative."""
    return path.split("/", 4)[4] if path.startswith("/Users/bench/") and path.count("/") >= 4 else path


def run(argv):
    path, args = argv[0], argv[1:]
    opt = lambda f: args[args.index(f) + 1] if f in args else None
    engines = [
        (*a.split("=", 1), flag)
        for flag in ("--engine", "--dir-engine")
        for i, a in enumerate(args)
        if i and args[i - 1] == flag
    ]
    rows = trace(path)
    keys, final, spans = [], [], []
    for q, want in rows:
        spans.append((q, len(keys), len(keys) + len(q)))
        keys += [q[: i + 1] for i in range(len(q))]
        final.append((len(keys) - 1, want))
    graded = all(w for _, w in final)
    if graded and opt("--dir"):
        missing = [w for _, w in final if not os.path.isfile(os.path.join(opt("--dir"), w))]
        if missing:
            sys.exit(f"wanted files missing from the corpus: {missing}")

    results = {name: [] for name, _, _ in engines}
    for n in range(PASSES):  # rotate the order, so drift doesn't always hit the same engine
        for name, binary, flag in engines[n % len(engines) :] + engines[: n % len(engines)]:
            results[name].append(run_engine([binary, opt("--disk" if flag == "--engine" else "--dir")], keys))

    print(
        f"{len(keys)} keystrokes from {os.path.basename(path)}. Total = sum of per-key medians over "
        f"{ROUNDS} rounds; median of {PASSES} passes in rotated order, (min-max). One machine, one run: not an A/B.\n"
    )
    head = ["engine", "total ms", "p50 key ms", "startup s", "peak RSS MB", "unstable keys"]
    if graded:
        head += ["found first", "found in top 10"]
    print("| " + " | ".join(head) + " |")
    print("|---" * len(head) + "|")
    for name, _, _ in engines:
        runs = results[name]
        totals = sorted(sum(r[0]) for r in runs)
        mid = min(runs, key=lambda r: abs(sum(r[0]) - totals[len(totals) // 2]))
        med, answers, startup, rss, _ = mid
        cells = [
            name,
            f"{totals[len(totals) // 2]:.1f} ({totals[0]:.1f}-{totals[-1]:.1f})",
            f"{statistics.median(med):.3f}",
            f"{startup:.2f}",
            f"{rss:.0f}",
            str(max(r[4] for r in runs)),
        ]
        if graded:
            for n in (1, 10):
                cells.append(f"{sum(w in answers[k][:n] for k, w in final)}/{len(final)}")
        print("| " + " | ".join(cells) + " |", flush=True)
    if len(engines) > 1:
        per_query(results, [name for name, _, _ in engines], spans)


def per_query(results, names, spans):
    """Each engine against the first, one query at a time (all its keystrokes)."""
    # A query's time: the median over passes of the sum of its keystrokes' medians.
    ms = {e: [statistics.median(sum(r[0][s:t]) for r in results[e]) for _, s, t in spans] for e in names}
    kinds = sorted({kind_of(q) for q, _, _ in spans})
    gm = lambda rs: math.exp(statistics.fmean(math.log(r) for r in rs)) if rs else float("nan")
    base = names[0]
    print(
        f"\nPer query, against {base}: time ratio, so below 1 is faster. Geometric mean over "
        f"{len(spans)} queries; faster/slower means past ±{BAND:.0%}.\n"
    )
    head = ["engine", "geo mean", "faster", "slower"] + kinds
    print("| " + " | ".join(head) + " |")
    print("|---" * len(head) + "|")
    for e in names[1:]:
        ratio = [a / b for a, b in zip(ms[e], ms[base])]
        cells = [e, f"{gm(ratio):.3f}", str(sum(r < 1 - BAND for r in ratio)), str(sum(r > 1 + BAND for r in ratio))]
        cells += [f"{gm([r for r, (q, _, _) in zip(ratio, spans) if kind_of(q) == k]):.2f}" for k in kinds]
        print("| " + " | ".join(cells) + " |", flush=True)


if __name__ == "__main__":
    cmd = sys.argv[1] if len(sys.argv) > 1 else ""
    if cmd == "mkdisk" and len(sys.argv) == 4:
        mkdisk(sys.argv[2], sys.argv[3])
    elif cmd == "gen" and len(sys.argv) >= 4:
        a = sys.argv[4:]
        flag = lambda f, d: int(a[a.index(f) + 1]) if f in a else d
        gen(sys.argv[2], sys.argv[3], flag("--seed", time.time_ns() % 10**9), flag("--count", 44))
    elif cmd == "run" and len(sys.argv) > 2:
        run(sys.argv[2:])
    else:
        sys.exit(__doc__)
