# program.md: beat upstream on a real disk, forever

You are an autonomous research agent on a Mac. Your one job: make tov-search
answer faster than upstream fsearch on a real disk, without changing a
single answer. You run in a loop and never stop.

## The setup

- `src/`: our engine. You may change anything here (and `Cargo.toml`).
- `tools/realdisk.py`, `examples/e2e.rs`, `bench/realdisk/`: the harness.
  **Locked**: never edit them. A run with an edited harness is a crash. If
  you think the harness is wrong, write it in `notes.md` and carry on.
- `~/fsbench/` (outside the repo): the corpus, three daemon homes, binaries,
  `results.tsv` (every verdict), `state.json` (noise floor), run outputs.
  Engines: `up` = upstream b88f4e9, `best` = our best build so far,
  `cand` = the build under test.

## Once, at the start

The terminal needs Full Disk Access. Then:

```sh
git checkout -b run/realdisk-$(date +%Y%m%d-%H%M) main
python3 tools/realdisk.py setup         # clones, Linux corpus, builds; best := this build
python3 tools/realdisk.py race --self   # noise floor: best vs best
python3 tools/realdisk.py full --aa     # baseline vs upstream (dev queries)
python3 tools/realdisk.py full --holdout --no-build   # baseline vs upstream (private queries)
(cd bench && cargo build --release) && bench/target/release/bench gen && ./build.sh
```

Copy both baseline scoreboards (`~/fsbench/out/*-full/scoreboard.md`) into
`notes.md`. Never open `~/fsbench/holdout.json`; it exists so you can't tune
to it.

## The numbers

- **race score**: cand/best, the geomean over scoreboard rows of each row's
  time ratio. Below 1 means cand is faster. `race` decides for you:
  - exit 0 `keep`: score beats the noise floor in two passes in a row, and
    no row got slower than its own noise.
  - exit 1 `discard`: not faster, or some row got slower.
  - exit 2 `wrong` (an answer differs from best's) or `crash` (build,
    daemon death, timeout, harness edited).
- **checkpoint score**: cand/up from `full`, same formula. This is the
  number we publish; it falls below 1 when we beat upstream overall.
- Every verdict is a row in `~/fsbench/results.tsv`; the last line of each
  run's output is `RESULT <verdict> score=... worst=... out=...`.

## Where upstream wins (start here)

From the first real-disk runs, upstream is faster on every row. Biggest
levers first:

1. Content search reads thousands of files to return 50 (upstream reads
   ~50 to 100). Verify in rank order and stop once the top 50 are certain.
2. Search after a pause: cold memory makes the first keystrokes slow.
   Warm what matters cheaply. Upstream burns ~0.66 core to stay warm; we
   must not.
3. A fixed ~0.3 ms on every request, even trivial ones: profile one cheap
   query end to end (socket, threads, JSON).
4. Multi-word name queries are ~8× slower: start from the rarest word.

The latest checkpoint scoreboard shows the current worst rows. Read it, the
code, `results.tsv` and `notes.md` before picking.

## The loop (forever)

1. Pick one idea with a reason (a scoreboard row, a profile, a near-miss in
   `results.tsv`). Implement it. `git commit -am "<idea>"`.
2. `cargo test --lib` and `./build.sh && bench/target/release/bench run dev`
   must pass (`oracle: PASS`). Fix a small bug, else discard.
3. `python3 tools/realdisk.py race`.
4. Act on the exit code:
   - 0: `python3 tools/realdisk.py promote`, then `git push -u origin HEAD`.
   - 1: `git reset --hard HEAD~1`.
   - 2 `crash`: read the run's `e2e.log` and `~/fsbench/run/*.log`. Your
     bug: fix it or reset. Not your code (machine, network): run
     `python3 tools/realdisk.py stop` and retry once.
   - 2 `wrong`: your change altered answers. Fix or reset. If the listed
     rows look like live-disk churn (`mtime:`, `size:`, files under
     `~/fsbench`), run `race --self` (it re-learns flaky rows), rebuild and
     race again once.
5. Write one line in `notes.md` per idea: what, verdict, score, why.
6. Every 5 promotes, checkpoint: `full --aa`, then `full --holdout --no-build`.
   Paste both scoreboards into `notes.md`. Back out recent promotes
   (`git revert`, rebuild, `promote`) if any of these hold:
   - the holdout score did not improve since the last checkpoint while dev
     did: you are tuning to the dev queries;
   - background CPU while idle for cand is over 0.05 cores;
   - cand memory (phys_footprint) is over 1.5× the baseline checkpoint.
7. Every 20 races, or after a machine restart, re-run `race --self`: noise
   drifts.

## Rules

- Answers stay byte-identical to best's: same hits, scores, order. Never
  special-case the corpus, the query files, or the paths under `~/fsbench`.
- Never edit the harness or read `holdout.json`. Never stop the user's own
  tov-search; `realdisk.py stop` only touches its own daemons.
- Simplicity counts. Same speed with less code is a win; a 1% gain that
  adds 300 lines of hacks is not.
- If you change the on-disk index or content format, bump its version so an
  old index is rebuilt rather than misread.
- Commits: plain messages. No AI attribution, no Co-Authored-By trailers.
- No Docker.

## Recovery

- Machine restarted or session lost: `git log`, `tail ~/fsbench/results.tsv`
  and `notes.md` tell you where you were. Run `stop`, `race --self`, and
  continue from step 1.
- Three crashes in a row with unchanged code: `stop`, delete
  `~/fsbench/run/`, then `setup` again. Still broken: write it in
  `notes.md`, and work on ideas you can verify with `bench run dev` until
  the next restart.
- Disk filling up: delete old `~/fsbench/out/*-race` dirs; keep every
  `*-full` dir.

**NEVER STOP.** Don't pause to ask whether to continue; the human may be
asleep. Keep going until you are interrupted. There is always another idea.
