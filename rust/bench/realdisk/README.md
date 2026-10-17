# Real-disk benchmark: this repo vs upstream

Daemons live on the same Mac: upstream fsearch at `b88f4e9` (`up`), our best
build so far (`best`) and the build under test (`cand`). Every request goes
to two of them in turn, over the socket the app uses, so machine drift hits
both alike. A request counts only when both return the identical answer.

This is the number that decides whether we beat upstream. The synthetic
benchmark in `bench/` stays as a fast correctness and regression check.
Agents running the never-stop loop follow [`program.md`](program.md).

## Run

```
python3 tools/realdisk.py setup         # once: upstream, Linux corpus (~2 GB), builds, private holdout
python3 tools/realdisk.py quick         # glance: up vs this build, one round
python3 tools/realdisk.py full --aa     # checkpoint vs upstream: + typing pauses, idle, CPU, memory, launch
python3 tools/realdisk.py full --holdout   # same on the private query set
python3 tools/realdisk.py race --self   # noise floor (best vs best)
python3 tools/realdisk.py race          # best vs this build: keep (exit 0) / discard (1) / crash or wrong (2)
python3 tools/realdisk.py promote       # this build becomes best
```

Everything lives in `~/fsbench` (override with `FSBENCH`). Each run writes
`~/fsbench/out/<time>-<mode>/` with `scoreboard.md` and raw JSON, and appends
its verdict to `~/fsbench/results.tsv`. `realdisk.py score <dir>` re-prints a
scoreboard. Daemons stop when a run ends, crash or not.

Before a `full` run: plugged in, Low Power Mode off, no other heavy work, the
Mac idle for a minute. The terminal needs Full Disk Access.

## Reading the scoreboard

| row | what it is |
|---|---|
| `name` exact/typo/word/multi/filter | one name search, back to back |
| `key` first/next | typing, every keystroke a request, no pause |
| `key120`, `typing gap120` | typing with 120 ms between keys: per key, and the whole word |
| `idle0.05` … `idle30` | a search after that many seconds of quiet |
| `grep` rare/common/regex/sym/scoped/scan | search inside files |

Times are the geomean of each request's median, `B/A < 1` means B is
faster, and the winner column treats anything within the noise margin as a
tie. The score is the geomean of the row ratios. Below the table come
background CPU while idle, memory, launch time and ready times. Background
CPU is part of the verdict: staying warm by burning a core is not a win.

## Query sets

- `queries-dev.json` is visible; tune against it. It is the set the earlier
  Mac comparison used (docs/compare/), so numbers stay comparable.
- `~/fsbench/holdout.json` comes from a random seed nobody sees, written by
  `setup`. Agents never open it. For a claim you post, generate your own on
  your Mac: `realdisk.py holdout <your seed> <file>`, then
  `full --queries <file>`.

## Locked

`examples/e2e.rs`, `tools/realdisk.py` and `queries-dev.json` are pinned by
`LOCK.sha256`. Runs refuse to start (verdict `crash`) if they change. After
an intended harness change, update the lock in the same commit:
`shasum -a 256 <the three files>`.
