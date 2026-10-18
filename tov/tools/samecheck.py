#!/usr/bin/env python3
"""Feed the same queries to the Tov engine and the Rust one on a real disk
file, and compare every answer byte for byte.

    samecheck.py <disk-file> <home> [--linux] [--tov build/engine-simd-par]
                 [--rust ../rust/target/release/examples/realcheck] [--queries file.json]...

Queries: every name query and every typing prefix in each --queries file
(default ../rust/bench/realdisk/queries-dev.json), plus a few filters.
Prints the first differences; exits 1 if any.
"""
import argparse
import json
import pathlib
import subprocess
import time

HERE = pathlib.Path(__file__).resolve().parent.parent
EXTRA = ["ext:rs main", "type:dir src", "kind:dir build", "in:~ readme", "!test main.c", "^Make", "file$", "'exact",
         "size:>1mb ext:png", "mtime:<1w", "~ config", "ext:json package", "a", "zz", ".git", "node_modules index.js",
         r"re:^[a-z]+_[0-9]+\.c$", "path:drivers.*usb", r"re:\d{4}", r"re:^\.", "re:^bash path:etc", r"config re:\.json$",
         r"path:^/home.*\.md$", "re:a.*b ext:txt", r"path:src/.*\.rs$ main", r"re:^[A-Z][a-z]+\.tov$", "re:readme !git",
         "path:lib limit:200", "re:("]


def run(cmd, queries):
    p = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE)
    t = time.time()
    assert p.stdout.readline() == b"ready\n", f"{cmd[0]}: no ready"
    ready = time.time() - t
    answers = []
    for q in queries:
        p.stdin.write(q.encode() + b"\n")
        p.stdin.flush()
        lines = []
        while (line := p.stdout.readline()) not in (b"\n", b""):
            lines.append(line)
        answers.append(b"".join(lines))
    p.stdin.close()
    p.wait()
    return answers, ready


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("disk")
    ap.add_argument("home")
    ap.add_argument("--linux", action="store_true")
    ap.add_argument("--tov", default=str(HERE / "build/engine-simd-par"))
    ap.add_argument("--rust", default=str(HERE.parent / "rust/target/release/examples/realcheck"))
    ap.add_argument("--queries", action="append")
    a = ap.parse_args()
    queries = []
    for f in a.queries or [str(HERE.parent / "rust/bench/realdisk/queries-dev.json")]:
        d = json.load(open(f))
        queries += [x["q"] for x in d.get("names", [])]
        queries += [k for t in d.get("typing", []) for k in t]
    queries += EXTRA
    extra = ["linux"] if a.linux else []
    tov, tr = run([a.tov, a.disk, a.home] + extra, queries)
    rust, rr = run([a.rust, "engine", a.disk, a.home] + extra, queries)
    bad = [i for i in range(len(queries)) if tov[i] != rust[i]]
    print(f"{len(queries)} queries, {len(bad)} differ (ready: tov {tr:.1f} s, rust {rr:.1f} s)")
    for i in bad[:5]:
        print(f"--- {queries[i]!r}\ntov:\n{tov[i].decode(errors='replace')}rust:\n{rust[i].decode(errors='replace')}")
    raise SystemExit(1 if bad else 0)


if __name__ == "__main__":
    main()
