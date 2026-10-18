#!/usr/bin/env python3
"""Check content search against the Rust app on a real disk file: both index
home's file contents into a new content index (`!content <dir>`), the
segment files must come out byte for byte the same (but for files that
changed while the check ran), then both answer the same content searches
(no time budget), compared byte for byte.

    grepcheck.py <disk-file> <home> [--linux] [--tov build/engine-simd-par]
                 [--rust ../rust/target/release/examples/realcheck] [--work /tmp/grepcheck]

Exits 1 on any difference.
"""
import argparse
import json
import os
import pathlib
import shutil
import struct
import sys
import time

HERE = pathlib.Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
from samecheck import run  # noqa: E402

EXTRA = ["grep:fn main", "grep:TODO", "grep:import", "grep:return", "grep:struct", "grep:Main", "grep:the limit:5",
         "grep:return limit:200", "grep:ab", "grep:a", "grep:", "grep:struct !test", "ext:rs grep:unsafe", "ext:md grep:install",
         "ext:py grep:self", "in:~/tov-product grep:fold", "in:/etc grep:localhost", "in:/usr/share/doc grep:license",
         "grep:münchen", "grep:kontakt", "grep:\"hello world\"", "regex:fn\\s+\\w+_score", "regex:TODO|FIXME",
         "regex:impl\\s+\\w+\\s+for", "regex:[A-Z]{3,}_[A-Z]{3,}", "regex:^use ", "regex:(foo|bar)baz",
         "regex:[0-9]{4}-[0-9]{2}", "regex:fn (main|run)\\(", "regex:a.c", "regex:\\bstatic\\b", "regex:(", "sym:main",
         "sym:Query", "sym:new", "sym:Engine", "sym:render", "sym:Config", "sym:foo.bar", "symbol:parse", "content:hello",
         "mtime:<30d grep:the", "size:<1kb grep:a", "kind:file grep:main", "path:src grep:pub", "re:\\.rs$ grep:let"]


def sections(b):
    """A segment file's sections (rust/src/content.rs lens and layout)."""
    nd, nt, pl, pa, bl, lo = struct.unpack("<6Q", b[8:56])
    n1 = (nd + 1) * 4
    lens = [nt * 4, (nt + 1) * 4, pl, n1, pa, nd * 8, nd * 4, nd * 4, nd, n1, bl * 8, n1, lo * 8]
    out, at = [], 4096
    for n in lens:
        out.append(b[at:at + n])
        at = (at + n + 63) & ~63
    return nd, out


def changed_docs(t, r):
    """Paths of the docs whose table entries (size, mtime, rank, filters)
    differ between two segments of the same docs; None if the docs differ."""
    (nd, a), (nr, b) = sections(t), sections(r)
    if nd != nr or a[3:5] != b[3:5]:
        return None
    po = struct.unpack(f"<{nd + 1}I", a[3])
    out = set()
    for d in range(nd):
        per = [(5, 8), (6, 4), (8, 1)]
        if any(a[s][d * w:(d + 1) * w] != b[s][d * w:(d + 1) * w] for s, w in per):
            out.add(a[4][po[d]:po[d + 1]].decode(errors="replace"))
        for off, words in ((9, 10), (11, 12)):
            ao, bo = struct.unpack_from("<2I", a[off], 4 * d), struct.unpack_from("<2I", b[off], 4 * d)
            if a[words][ao[0] * 8:ao[1] * 8] != b[words][bo[0] * 8:bo[1] * 8]:
                out.add(a[4][po[d]:po[d + 1]].decode(errors="replace"))
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("disk")
    ap.add_argument("home")
    ap.add_argument("--linux", action="store_true")
    ap.add_argument("--tov", default=str(HERE.parent / "build/engine-simd-par"))
    ap.add_argument("--rust", default=str(HERE.parent.parent / "rust/target/release/examples/realcheck"))
    ap.add_argument("--work", default="/tmp/grepcheck")
    a = ap.parse_args()
    d = json.load(open(HERE.parent.parent / "rust/bench/realdisk/queries-dev.json"))
    queries = [x["q"] for x in d["grep"]] + EXTRA
    work = pathlib.Path(a.work)
    shutil.rmtree(work, ignore_errors=True)
    start = time.time() - 1
    extra = ["linux"] if a.linux else []
    tov, _ = run([a.tov, a.disk, a.home] + extra, [f"!content {work}/tov"] + queries)
    rust, _ = run([a.rust, "engine", a.disk, a.home] + extra, [f"!content {work}/rust"] + queries)
    bad = 0
    tf = sorted(p.name for p in (work / "tov").iterdir())
    rf = sorted(p.name for p in (work / "rust").iterdir())
    if tf != rf:
        print(f"segment files differ: tov {tf}, rust {rf}")
        bad += 1
    else:
        for name in tf:
            t, r = (work / "tov" / name).read_bytes(), (work / "rust" / name).read_bytes()
            if t == r:
                continue
            docs = changed_docs(t, r) if name.endswith(".fsc") else None
            live = docs is not None and all(os.path.exists(p) and os.lstat(p).st_mtime >= start for p in docs)
            print(f"{name} differs in {len(docs) if docs is not None else '?'} docs: {sorted(docs or [])[:5]}" +
                  (" (all changed during the check)" if live else ""))
            bad += 0 if live else 1
    print(f"indexed: tov {tov[0].decode().strip()}, rust {rust[0].decode().strip()} files; {len(tf)} index files, {bad} differ")
    diff = [i for i in range(1, len(queries) + 1) if tov[i] != rust[i]]
    print(f"{len(queries)} searches, {len(diff)} differ")
    for i in diff[:5]:
        print(f"--- {queries[i - 1]!r}\ntov:\n{tov[i].decode(errors='replace')[:2000]}rust:\n{rust[i].decode(errors='replace')[:2000]}")
    raise SystemExit(1 if bad or diff or tov[0] != rust[0] else 0)


if __name__ == "__main__":
    main()
