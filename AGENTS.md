# Working guide for coding agents

tov-search is whole-disk fuzzy file search. The main engine is written in
Tov (TypeScript compiled to C) with hand-written C kernels swapped in for
hot functions; a Rust engine is the reference it must match byte for byte.
Start with [README.md](README.md), then read the doc for your task.

## Where to read

| Task | Read |
|---|---|
| Find where something lives, understand the index or a keystroke | [docs/architecture.md](docs/architecture.md) |
| Build, write or change a kernel, change parsing, scoring or layout | [docs/kernels.md](docs/kernels.md) |
| Prove a change is correct, CI | [docs/testing.md](docs/testing.md) |
| Measure speed, prove a change faster, compare with fsearch and fff | [docs/benchmarking.md](docs/benchmarking.md) |
| Why the Rust reference is built the way it is, past experiments | [docs/rust-architecture.md](docs/rust-architecture.md) |
| The engine protocol and disk format | [rust/bench/PROTOCOL.md](rust/bench/PROTOCOL.md) |
| Short Tov how-to | [tov/README.md](tov/README.md) |
| Cut a release, update the Homebrew install (two steps, don't skip the second) | [rust/README.md#release](rust/README.md#release) |

## Must stay true

- Answers are byte-identical to the saved oracle on both traces: hits,
  scores, order and framing. No approximate scores or other tie breaks.
- The benchmark is frozen: harness, traces, disk generator, dataset
  fingerprint and saved oracles. Never re-bless the oracle to fit a change.
- No special-casing trace queries, synthetic names or the dataset size.
  Memory and startup stay within 1.5x of `main`. Prefer simple over tiny
  gains; removing complexity at equal speed is a win.
- A speed change is kept only with A/B proof: at least 8/9 winning pairs and
  at least 1% better, on dev, checked on holdout. It must also hold up per
  query on holdout and a fresh `compare.py gen` trace: geometric mean at most
  1.00, no more queries slower than faster. Never tune on a random trace.
  See benchmarking.md.
- A kernel may change only its declared `writes:`. Generated build output is
  never edited by hand.
- Don't `switch` on strings in Tov: the pinned compiler frees them early.
  Add new compiler bug repros under `tov/bugs/`, then work around them.

## Git workflow

- Never open, push for, comment on or close a PR unless the owner asks for
  that PR in so many words. Plans, notes and failures stay local. Commit on
  a local branch and wait.
- When asked, branch from `main`; open PRs as drafts (`gh pr create
  --draft`). CI skips drafts. Mark ready only once the work is complete and
  `tov/check.sh` passes.
- Fill in the PR template: what happened, and real proof of work.
- Squash-merge; never force-push. Merge `main` into shared branches rather
  than rebasing. Keep discarded experiment branches as history.
- Commits: plain summary, then a short body with the change and the measured
  result when there is one. No AI attribution in commits or PRs.
- Never commit machine names, hostnames, IPs or personal setup details.
- Size work by files, steps and checks, never human hours or minutes.
