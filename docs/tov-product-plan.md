# Plan: make Tov the product

## Where things stand (2026-10-18)

- What users install today (the v0.1.0 release and the Homebrew install) is the
  **Rust** program: disk scan, live updates, background program, command line,
  content search. Every merged PR since the first commit (#1-#4) changed Rust
  or CI; `tov/` is unchanged since `d6de9c3`.
- The **Tov** engine (`tov/`, ~3,400 lines plus C kernels) is a search core
  only. It reads a prepared disk file (`FSDISK01`, see
  [PROTOCOL.md](../rust/bench/PROTOCOL.md)), builds its index and answers one
  query per input line. It has no disk scan, no live updates, no background
  program, no `re:`/`path:` filters and no content search.
- Speed on the frozen benchmark (Linux, 8 CPUs): Tov 2,278 ms dev, Rust 817 ms,
  so Tov is about 2.8x slower than Rust. Rust is about 1.0x upstream
  fsearch on a real disk (`realdisk.py full`, 2026-10-18, score 1.018).
- On hold: PR #5 (README and `install.sh`). The install script works, but
  points at the Rust release.

## Goal

`tov-search` as shipped is built from Tov: Tov logic, with C only where Tov
can't reach the operating system or needs SIMD and threads. Then Rust is
deleted.

## Rules for the whole plan

- **Tov does the logic.** C enters only through `// hook:` kernels (speed,
  checked against their Tov bodies in shadow builds) and `// hook(raw):`
  functions (OS calls: folders, file info, sockets, file watching, memory
  maps). Each raw hook does one OS call and nothing else.
- **Rust is frozen.** No new features or speed work in `rust/`. It stays only
  as the answer key until the deletion phase.
- **Same answers as Rust**, byte for byte, at every phase: on the frozen
  traces (the oracle) and on a real-disk snapshot fed to both.
- Every phase ships as its own draft PR with proof, per [AGENTS.md](../AGENTS.md).
- Builds: macOS arm64 on the Mac, Linux x86_64 and arm64 on the Linux
  workstation. No CI runners.

## Phases

### 1. Command line on a real disk (name search)

What lands:
- `tov-search index`: scans the disk and writes the index.
- `tov-search <query>`: loads it and prints the top hits as paths.

Steps:
1. Raw hooks to list a folder with each entry's kind, size and mtime:
   `getattrlistbulk` on macOS, `getdents64` + `statx` on Linux. Parallel
   across folders on the existing `par` thread pool.
2. The scan writes the same `FSDISK01` records the engine already reads, so
   `readDisk` and `build` are reused unchanged. Skip rules (blocked folders,
   mounts) ported from `rust/src/walk.rs`.
3. Path output for hits (folder chain to a string), and a `main` that takes
   arguments instead of the benchmark protocol (the protocol mode stays for
   the bench).

Check: scan the same tree with both programs, then compare `tov-search <q>`
with Rust's answers for every query in `bench/realdisk/queries-dev.json` (name
rows). The oracle still passes.

### 2. Background program and fast startup

What lands: `tov-search serve` keeps the index in memory and answers JSON
lines on a unix socket, with the same protocol as `rust/src/server.rs`, so
`realdisk.py` and the bench clients work unchanged. The command line starts it
on first use. `tov-search install [--login]` sets up launchd or systemd.

Steps: raw hooks for socket listen/accept/read/write; a small JSON writer in
Tov (the request side needs only a few fields); save the built index and load
it with a memory map (raw hook), so startup doesn't rebuild it.

Check: `realdisk.py` against the Rust daemon: same answers on all name rows.
Startup and memory recorded.

### 3. Live updates

What lands: the index follows disk changes, both while running and after a
restart.

Steps:
1. Raw hooks: an FSEvents stream on macOS, and inotify plus a periodic
   catch-up on Linux. Both report changed folder paths and event ids.
2. Port the change model from `rust/src/live.rs` and `engine.rs` into Tov:
   added entries in a side table, deleted ones marked, both merged into
   search, and compaction into a new index when the side table grows.
3. Replay from the saved event id on start; relist changed folders when
   events were dropped.

Check: a scripted test that creates, renames and deletes files and folders,
then compares answers with a fresh scan. Searches stay correct with deleted
entries present (the bug fixed in Rust in #4).

### 4. Missing filters

`re:` and `path:` (and `regex:` for content) through the system regex
library in a raw hook. Decide on the dialect: POSIX extended, or a small
matcher kept compatible with what the Rust version accepts.

Check: the regex rows of the real-disk queries match Rust.

### 5. Content search

The largest piece (`rust/src/content.rs`, ~2,600 lines): the index of text
file contents, `grep:`, `regex:`, `sym:`, background indexing while idle,
and updates when files change. Port it in Tov, with C kernels for the byte
scanning.

Check: all grep rows of the real-disk queries match Rust.

### 6. Speed

Close the gap to Rust and fsearch, now in Tov kernels. Start from what made
Rust fast: name bitsets, floor pruning, the ranked path, and narrowing while
typing (see [rust-architecture.md](rust-architecture.md) and PRs #3 and #4).
Each change passes the A/B rule in AGENTS.md.

Check: `realdisk.py full` with Tov against fsearch. Target: score at or below
1.0, idle CPU and memory within 1.5x of fsearch.

### 7. Ship

Release v0.2.0 built from Tov: macOS arm64, Linux x86_64 and arm64; update
the Homebrew formula with `rust/tools/brew.sh`; merge `install.sh` (PR #5)
pointing at it; update the READMEs.

Check: a clean install on the Mac and on the Linux workstation with both brew
and `install.sh`; `tov-search install --login` survives a reboot.

### 8. Delete Rust

Remove `rust/src`. Decide what happens to the Rust benchmark harness
(`rust/bench`) and the saved oracles: keep the harness as a frozen tool, or
rewrite it (the oracle files are data, so a rewrite can still check them).
Update AGENTS.md and the docs to drop the "Rust reference" rule.

## Decisions needed from you

1. Index format: keep `FSDISK01` plus a saved built index (fewest new parts),
   or design a new one.
2. Regex dialect for `re:`/`path:`/`regex:`.
3. Keep or rewrite the Rust benchmark harness after phase 8.
4. Whether to ship a Tov release after phase 2 (name search only, no content
   search) or wait for phase 5.

## Known risks

- Tov compiler bugs: one is known (`tov/bugs/`, `switch` on strings), and
  more are likely in new kinds of code. Each gets a repro in `tov/bugs/`
  and a workaround.
- The pinned Tov version (`tov/build.sh`); a newer one may be needed for
  missing features.
- The raw hook layer has to stay small, or the product is C in disguise.
