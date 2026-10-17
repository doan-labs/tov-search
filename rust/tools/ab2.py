#!/usr/bin/env python3
"""Interleaved A/B adjudicator: same keystroke to both engines back-to-back.

Drift and cache-warmth effects hit both engines within ~ms of each other, so
per-keystroke time differences are far more robust than bench ab's ~90s pairs.
Compares per-keystroke medians across rounds like the bench metric does.

Usage: python3 ab2.py <engineA> <engineB> [rounds] [trace-file]
Keystrokes default to the dev trace expanded like bench does.
"""
import re
import statistics
import subprocess
import sys
import time

DISK = "bench/data/disk.bin"


def keys(trace="bench/traces/dev.txt"):
    out = []
    for line in open(trace):
        line = line.rstrip("\n")
        if not line or line.startswith("#"):
            continue
        for i in range(1, len(line) + 1):
            out.append(line[:i])
    return out


class Eng:
    def __init__(self, path):
        self.p = subprocess.Popen([path, DISK], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
        self.path = path
        assert self.p.stdout.readline() == b"ready\n"

    def query(self, q):
        t0 = time.perf_counter_ns()
        self.p.stdin.write(q.encode() + b"\n")
        self.p.stdin.flush()
        out = []
        while True:
            l = self.p.stdout.readline()
            if l == b"\n" or l == b"":
                break
            out.append(l)
        return time.perf_counter_ns() - t0, b"".join(out)


def main():
    engA, engB = sys.argv[1], sys.argv[2]
    rounds = int(sys.argv[3]) if len(sys.argv) > 3 else 3
    ks = keys(sys.argv[4] if len(sys.argv) > 4 else "bench/traces/dev.txt")
    A, B = Eng(engA), Eng(engB)
    # One warmup pass per engine, untimed, to equalize caches/paging.
    for q in ks:
        A.query(q)
        B.query(q)
    ta = [[] for _ in ks]
    tb = [[] for _ in ks]
    wrong = 0
    for r in range(rounds):
        for i, q in enumerate(ks):
            if (r + i) % 2 == 0:
                (a, ra), (b, rb) = A.query(q), B.query(q)
            else:
                (b, rb), (a, ra) = B.query(q), A.query(q)
            if ra != rb:
                wrong += 1
                print(f"RESP DIFF key {i} {q!r}\n A: {ra[:200]}\n B: {rb[:200]}")
            ta[i].append(a)
            tb[i].append(b)
    med = [(statistics.median(ta[i]), statistics.median(tb[i]), ks[i]) for i in range(len(ks))]
    sa = sum(m[0] for m in med)
    sb = sum(m[1] for m in med)
    wins = sum(1 for a, b, _ in med if b < a)
    print(f"A={engA}\nB={engB}")
    print(f"sum-of-medians: A {sa/1e6:.1f}ms  B {sb/1e6:.1f}ms  delta {(sb-sa)/sa*100:+.2f}%  B-wins {wins}/{len(ks)}")
    # Min-of-rounds per key: interference only adds time, so min is a much
    # more noise-robust comparator on this VM than the median.
    mn = [(min(ta[i]), min(tb[i])) for i in range(len(ks))]
    msa = sum(m[0] for m in mn)
    msb = sum(m[1] for m in mn)
    mwins = sum(1 for a, b in mn if b < a)
    print(f"sum-of-mins:    A {msa/1e6:.1f}ms  B {msb/1e6:.1f}ms  delta {(msb-msa)/msa*100:+.2f}%  B-wins {mwins}/{len(ks)}")
    # noise estimate: keystroke-level delta distribution
    ds = sorted(b - a for a, b, _ in med)
    p = len(ds)
    print(f"per-key delta us: p10 {ds[p//10]/1e3:.0f} p50 {ds[p//2]/1e3:.0f} p90 {ds[9*p//10]/1e3:.0f}")
    if wrong:
        print(f"!! {wrong} response mismatches")
        sys.exit(2)


main()
