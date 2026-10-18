#!/usr/bin/env python3
"""Stress differential for the pruning/caching paths: one big random disk
(many distinct names that share letters, varied stats), typed query chains
of every shape, candidate vs reference, byte-compared.

Usage: stress-check.py CAND REF [seed] [entries]
"""
import random
import struct
import subprocess
import sys
import pathlib

NOW = 1_780_000_000
SYL = ["re", "ad", "me", "src", "in", "dex", "con", "fig", "pho", "to", "web", "pack", "doc", "ker", "test", "main",
       "Read", "ME", "x", "a", "b", "e", "s", "t", "2024", "20", "1", "0", "_", "-", ".", " ", "lib", "App", "tsx"]
EXTS = ["", ".md", ".rs", ".ts", ".json", ".txt", ".png", ".log", ".app", ".DS_Store"]


def rand_name(r):
    if r.random() < 0.08:
        return "".join(r.choice("0123456789abcdef") for _ in range(r.choice([40, 64, 100, 126, 128, 130])))
    n = "".join(r.choice(SYL) for _ in range(r.randint(1, 5)))
    if r.random() < 0.1:
        n = "." + n
    n = n.strip() or "x"
    return n + r.choice(EXTS)


def build(seed, n):
    r = random.Random(seed)
    recs = [(0, 1, 0, NOW, b"")]
    dirs = [0]
    for name in [b"Users", b"bench"]:
        recs.append((dirs[-1], 1, 0, NOW, name))
        dirs.append(len(recs) - 1)
    pool = [rand_name(r).encode() for _ in range(n // 3)]
    while len(recs) < n:
        parent = r.choice(dirs[-200:]) if r.random() < 0.7 else r.choice(dirs)
        name = r.choice(pool) if r.random() < 0.6 else rand_name(r).encode()
        name = name.replace(b"/", b"_")
        isdir = r.random() < 0.12
        kind = (1 if isdir else r.choice([0, 0, 0, 2])) | (4 if r.random() < 0.05 else 0)
        mtime = NOW - int(r.expovariate(1 / 3e6)) if r.random() < 0.9 else NOW + 100
        recs.append((parent, kind, r.randint(0, 1 << 34), mtime, name))
        if isdir:
            dirs.append(len(recs) - 1)
    chunks = [b"FSDISK01" + struct.pack("<QI", len(recs), NOW)]
    for parent, kind, size, mtime, name in recs:
        chunks.append(struct.pack("<IBQIH", parent, kind, size, mtime, len(name)) + name)
    return b"".join(chunks)


def chains(seed):
    r = random.Random(seed + 1)
    words = ["readme", "rdme", "raedme", "readm", "src", "index", "config", "confgi", "photo 2024", "webpack c",
             "docker co", "test main", "main", "a", "x", "e", ".", "re", "2024", "20", "app", "lib", "tsx", "ME",
             "0", "dex", "fig", "packer", "Read", "readme.md", "ad me", "'read", "'te", "^src", "^App", "md$",
             ".json$", "'2024", "^.", "'x", "s", "r"]
    out = []
    for w in words:
        out.append(w)
        out.append(w + " limit:5")
        out.append("limit:200 " + w)
    for _ in range(60):
        out.append(" ".join(r.choice(words) for _ in range(r.randint(1, 3))))
    return out


def run(engine, disk, keys):
    out = subprocess.check_output([engine, disk], input=("\n".join(keys) + "\n").encode(), timeout=1200)
    assert out.startswith(b"ready\n"), engine
    return out


def main():
    cand, ref = sys.argv[1], sys.argv[2]
    seed = int(sys.argv[3]) if len(sys.argv) > 3 else 1
    n = int(sys.argv[4]) if len(sys.argv) > 4 else 300_000
    p = pathlib.Path("/tmp") / f"stress-{seed}-{n}.bin"
    if not p.exists():
        p.write_bytes(build(seed, n))
    keys = []
    for c in chains(seed):
        keys += [c[: i + 1] for i in range(len(c))]
        keys.append(c)  # a repeat reuses the cached table
    a, b = run(cand, str(p), keys), run(ref, str(p), keys)
    if a == b:
        print(f"ok seed={seed} n={n} keys={len(keys)}")
        return
    al, bl = a.split(b"\n\n"), b.split(b"\n\n")
    for k, (x, y) in enumerate(zip(al, bl)):
        if x != y:
            # Approximate when empty answers shift the split; still a FAIL.
            print(f"FAIL seed={seed} near key {k} {keys[k] if k < len(keys) else '?'!r}")
            print("  cand:", x[:400])
            print("  ref :", y[:400])
            break
    sys.exit(1)


if __name__ == "__main__":
    main()
