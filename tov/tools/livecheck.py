#!/usr/bin/env python3
"""Check live updates against the Rust app: snapshot the disk, change a test
folder (add, rename, delete, modify, replace a file by a folder), apply the
changed folders in both engines (`!apply <folder>`), then compare answers
byte for byte.

    livecheck.py <dir> [--linux] [--tov build/engine-simd-par]
                 [--rust ../rust/target/release/examples/realcheck]

<dir> must be a real path (no links on the way: /private/tmp, not /tmp).
Takes a whole-disk scan. Exits 1 on any difference.
"""
import argparse
import os
import pathlib
import shutil
import subprocess
import sys
import time

HERE = pathlib.Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
from samecheck import run  # noqa: E402

QUERIES = ["lcfoo", "lcbar", "lcbaz", "lcnew", "lcq", "in:{d} lc", "ext:md lc", "lcfoo !bar", "deep lcfoo", "kind:dir lc",
           "kind:file lcq", "size:>1kb lc", "'lcfoo", "^lcb", "md$", "lcfoo limit:3", "lcfoo limit:200", "main.rs", "readme",
           "config", "index.js"]


def write(p, text):
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(text)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("dir")
    ap.add_argument("--linux", action="store_true")
    ap.add_argument("--tov", default=str(HERE.parent / "build/engine-simd-par"))
    ap.add_argument("--rust", default=str(HERE.parent.parent / "rust/target/release/examples/realcheck"))
    a = ap.parse_args()
    d = pathlib.Path(a.dir)
    shutil.rmtree(d, ignore_errors=True)
    for i in range(30):
        write(d / f"lcfoo_{i}.txt", "x" * i)
    write(d / "deep/er/lcfoo_deep.txt", "x")
    write(d / "gone/lcbar_gone.md", "x")
    write(d / "lcbar.md", "x")
    write(d / "lcq", "x")
    snap = d.parent / "livecheck.disk"
    subprocess.run([a.rust, "scan", "/", str(snap)], check=True)

    time.sleep(1.1)  # so changed files get a new mtime
    for i in range(0, 30, 2):
        (d / f"lcfoo_{i}.txt").unlink()  # the best lcfoo hits are gone
    for i in range(30, 40):
        write(d / f"lcfoo_{i}.txt", "y")
    write(d / "lcbar.md", "y" * 5000)  # modified
    shutil.rmtree(d / "gone")
    os.rename(d / "deep", d / "deeper")
    write(d / "new/lcnew/lcbaz.md", "x")
    (d / "lcq").unlink()
    write(d / "lcq/inside.txt", "x")  # a file replaced by a folder
    changed = [d, d / "deeper", d / "deeper/er", d / "new", d / "new/lcnew", d / "lcq"]

    lines = [f"!apply {c}" for c in changed] + [q.format(d=d) for q in QUERIES]
    home = os.environ["HOME"]
    extra = ["linux"] if a.linux else []
    tov, _ = run([a.tov, str(snap), home] + extra, lines)
    rust, _ = run([a.rust, "engine", str(snap), home] + extra, lines)
    snap.unlink()
    bad = [i for i in range(len(lines)) if tov[i] != rust[i]]
    hits = sum(r.count(str(d).encode()) for r in rust)
    print(f"{len(lines)} lines ({len(changed)} folder updates), {len(bad)} differ; {hits} hits in the test folder")
    for i in bad[:5]:
        print(f"--- {lines[i]!r}\ntov:\n{tov[i].decode(errors='replace')}rust:\n{rust[i].decode(errors='replace')}")
    raise SystemExit(1 if bad else 0)


if __name__ == "__main__":
    main()
