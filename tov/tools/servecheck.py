#!/usr/bin/env python3
"""Send the same JSON requests to the Tov background program and to the
Rust app's request handling on the same disk file, and compare the reply
lines byte for byte (took_us set to 0).

    servecheck.py <socket> <disk-file> <home> [--linux]
                  [--rust ../rust/target/release/examples/realcheck] [--queries file.json]...

The Tov program must be serving that disk file (its saved index). Requests:
every name query and typing prefix as {"q": ...}, plus fields, limits, ids
and errors. Prints the first differences; exits 1 if any.
"""
import argparse
import json
import pathlib
import re
import socket
import subprocess

HERE = pathlib.Path(__file__).resolve().parent.parent
EXTRA = [
    {"q": "main", "ext": "rs"}, {"q": "main", "ext": "rs,toml", "kind": "file"}, {"q": "readme", "in": "~"},
    {"q": "config", "limit": 3}, {"q": "config", "limit": 200}, {"q": "x", "limit": "5"}, {"q": "x", "limit": -1},
    {"q": "index", "id": 7}, {"q": "index", "id": "abc"}, {"q": "index", "id": {"b": [1, 2], "a": None}},
    {"op": "ping"}, {"op": "ping", "id": 3}, {"op": "nope"}, {"q": "size:>1mb ext:png"}, {"q": "kind:bogus"},
    {"q": "type:image cat"}, {"q": "in:/etc hosts"}, {"q": ""}, {"q": "main", "size": 5}, {"q": "a\tb"},
    {"q": "été"}, {"q": "mtime:<1w"},
]


def tov(sock, line):
    with socket.socket(socket.AF_UNIX) as s:
        s.connect(sock)
        s.sendall(line.encode() + b"\n")
        return s.makefile("rb").readline()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("sock")
    ap.add_argument("disk")
    ap.add_argument("home")
    ap.add_argument("--linux", action="store_true")
    ap.add_argument("--rust", default=str(HERE.parent / "rust/target/release/examples/realcheck"))
    ap.add_argument("--queries", action="append")
    a = ap.parse_args()
    reqs = []
    for f in a.queries or [str(HERE.parent / "rust/bench/realdisk/queries-dev.json")]:
        d = json.load(open(f))
        reqs += [{"q": x["q"]} for x in d.get("names", [])]
        reqs += [{"q": k} for t in d.get("typing", []) for k in t]
    lines = [json.dumps(r) for r in reqs + EXTRA]
    rust = subprocess.run([a.rust, "serve", a.disk, a.home] + (["linux"] if a.linux else []),
                          input="".join(x + "\n" for x in lines).encode(), capture_output=True).stdout.split(b"\n")
    bad = 0
    for i, line in enumerate(lines):
        got = re.sub(rb'"took_us":\d+', b'"took_us":0', tov(a.sock, line).rstrip(b"\n"))
        if got != rust[i]:
            bad += 1
            if bad <= 5:
                print(f"--- {line}\ntov:  {got[:400]!r}\nrust: {rust[i][:400]!r}")
    print(f"{len(lines)} requests, {bad} differ")
    raise SystemExit(1 if bad else 0)


if __name__ == "__main__":
    main()
