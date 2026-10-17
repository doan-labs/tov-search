#!/usr/bin/env python3
"""Compare an engine's answers with an oracle file, offline.

    diff_oracle.py <oracle.tsv> <engine-output>   (the engine fed every prefix of the trace)
"""
import pathlib
import sys

oracle, out_path = sys.argv[1:]
trace = pathlib.Path(oracle).stem
blocks = []
for line in open(oracle, encoding="utf-8", errors="surrogateescape"):
    if line.startswith("@@ "):
        blocks.append([line[3:].rstrip("\n"), ""])
    else:
        blocks[-1][1] += line
got = open(out_path, encoding="utf-8", errors="surrogateescape").read()
assert got.startswith("ready\n")
answers, cur = [], ""
for line in got[len("ready\n"):].splitlines(keepends=True):
    if line == "\n":
        answers.append(cur)
        cur = ""
    else:
        cur += line
bad = [k for k, (q, want) in enumerate(blocks) if k >= len(answers) or answers[k] != want]
print(f"{trace}: {len(blocks) - len(bad)}/{len(blocks)} keystrokes match the oracle")
for k in bad[:8]:
    q, want = blocks[k]
    w, g = want.splitlines(), answers[k].splitlines() if k < len(answers) else []
    first = next((i for i in range(max(len(w), len(g))) if (w[i:i+1] != g[i:i+1])), None)
    print(f"  {q!r}: line {first}: want {w[first] if first is not None and first < len(w) else None!r} got {g[first] if first is not None and first < len(g) else None!r}")
sys.exit(1 if bad else 0)
