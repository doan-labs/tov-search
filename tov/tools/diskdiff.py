#!/usr/bin/env python3
"""Compare two disk files (bench/PROTOCOL.md) as sets of entries.

    diskdiff.py <a.disk> <b.disk>

Prints the entry counts, then up to 20 entries only in each (path, kind,
size, mtime). Exits 1 if they differ. The record order may differ.
"""

import struct
import sys


def entries(path):
    b = open(path, "rb").read()
    assert b[:8] == b"FSDISK01", f"{path}: not a disk file"
    n, _now = struct.unpack_from("<QI", b, 8)
    paths = [b""]
    out = {}
    p = 20 + 19
    for _ in range(1, n):
        parent, kind, size, mtime, ln = struct.unpack_from("<IBQIH", b, p)
        name = b[p + 19 : p + 19 + ln]
        full = paths[parent] + b"/" + name
        paths.append(full)
        out[full] = (kind, size, mtime)
        p += 19 + ln
    return out


def main():
    a, b = entries(sys.argv[1]), entries(sys.argv[2])
    print(f"{len(a)} entries in {sys.argv[1]}, {len(b)} in {sys.argv[2]}")
    only_a = sorted(k for k in a if k not in b)
    only_b = sorted(k for k in b if k not in a)
    changed = sorted(k for k in a if k in b and a[k] != b[k])
    for label, ks in (("only in a", only_a), ("only in b", only_b)):
        print(f"{label}: {len(ks)}")
        for k in ks[:20]:
            print(f"  {k.decode(errors='replace')} {a.get(k) or b.get(k)}")
    print(f"different kind/size/mtime: {len(changed)}")
    for k in changed[:20]:
        print(f"  {k.decode(errors='replace')} a={a[k]} b={b[k]}")
    sys.exit(1 if only_a or only_b or changed else 0)


if __name__ == "__main__":
    main()
