# Rust reference: architecture and history

How the Rust engine works, how it got faster without changing any answer,
and why. Written after PR #7 (main `2d584e7`, 2026-10-11).

The old lab notebooks, result ledgers, upstream comparison dumps and
checkers named below are no longer in the tree. Tag `history/pre-cleanup`
has all of them.

## Goal and rules

tov-search starts from upstream **fsearch** (imported at `af9476d`), a
whole-disk fuzzy file search for macOS. Goal: answer every keystroke faster
with byte-identical results (same hits, scores, order), on any disk, not just
the benchmark's.

Rules (`docs/program.md`):

- `bench/` is frozen: harness, traces, oracles, dataset. Nobody edits it.
- No special-casing trace queries or the synthetic disk (no hard-coded
  answers, no keying on its names or entry count).
- Results match the reference engine exactly.
- Memory and startup stay under 1.5x the baseline.
- A tiny gain that adds hundreds of lines of hacks is not kept.

## Results

### Per-keystroke speed

`bench run dev` types 409 keystrokes: every prefix of every query in
`bench/traces/dev.txt`. Score = sum of each keystroke's median latency, in
ms. Lower is better. The disk is synthetic: 10,000,541 entries, 3.5M
distinct names, 719K folders.

| Machine | Start | Now (main `2d584e7`) | Change |
|---|---|---|---|
| Intel box (8c/16t, AVX2) | 3100 (main `5ac1518`) | 1279 | **−59%** |
| Apple M1 Pro (10 cores) | 1346 (main `5ac1518`) | ~618 (AB median) | **−54%** |

- PR #7 alone, on the M1 Pro against main `698ae60`: −32% (9/9 pairs).
- `5ac1518` is the first Rust merge, already ~40% faster than upstream.
- Upstream fsearch (`04a402e`) measured 5098 on the first-run VM. That is a
  different machine, so it can't be compared with the table.

### Other measurements

| What | Before | After |
|---|---|---|
| Filter-only search that finds little, e.g. `size:>5gb` (Intel) | 57.7 ms | 0.1 ms |
| Same on M1 Pro | 21.6 ms | 0.0 ms |
| Holdout trace (queries never tuned on), Intel | 2848 | 1281 (after keep 11) |
| Holdout, M1 Pro | 718 (after PR #3) | 552 (PR #7) |
| Memory, Intel, 10M entries | 1767 MB peak | 1767 MB peak (unchanged) |
| Startup, Intel | ~3.5 s | ~3.6 s |
| Startup, M1 Pro | ~1.6 s | ~1.6 s |

### Fresh-start check (no warm-up pass, Intel, after PR #7)

| Trace | main `698ae60` | PR #7 |
|---|---|---|
| holdout, total of first pass | 1742 ms | 1203 ms (−31%) |
| dev, total of first pass | 2115 ms | 1343 ms (−37%) |

```
first query after `ready`    ~2 ms
worst single keystroke       ~65 ms on both versions
cold vs warm pass            ~3% apart
```

Nothing is saved between runs. The in-memory caches only remember the last
keystroke and the last 8 words.

### Memory over time (Intel, PR #7)

| Point | Memory |
|---|---|
| Building the index (peak) | 1.77 GB |
| Ready | 1.16 GB |
| After 1 pass of dev + holdout | 1.26 GB |
| After 10 passes (~7.8K keystrokes) | 1.26 GB (flat) |

Memory scales with entry count, so a typical laptop disk needs a fraction of
this.

## Architecture

### Crate layout

One crate holds two products.

```
                ┌─────────────── macOS only ───────────────┐
 disk ──walk.rs (getattrlistbulk)──► Index ──► Live ◄── fsevents.rs
                                      │          │
                          content.rs (trigrams)  │
                                      └──► engine.rs ──► server.rs (unix socket, JSON lines)
                                                    └──► main.rs (CLI `tov-search`)

                ┌─────────────── any platform ─────────────┐
 disk file ──bin/engine.rs──► Index ──► Live ──► query.rs (Searcher) ──► stdout
```

| Part | Files | What it does |
|---|---|---|
| macOS app (`tov-search` binary and library) | `walk.rs`, `fsevents.rs`, `content.rs`, `engine.rs`, `server.rs`, `main.rs`, all `#[cfg(target_os = "macos")]` | Crawls the disk once, then stays live from FSEvents. Content search uses a trigram index. A daemon serves JSON lines. |
| Name engine | `index.rs`, `live.rs`, `query.rs`; `src/bin/engine.rs` wraps it in the bench protocol | What the benchmark measures and all optimization targeted. Builds on Linux too. |
| Tools | `src/bin/prof.rs` | Per-keystroke profiler; `PROF_TSV` gives per-shape breakdowns |
| | `src/bin/dump.rs` | Scratch statistics |

### Index (`index.rs`)

One flat blob, mmap-able as is.

- **Directory blocks in depth-first order.** A folder's children are
  contiguous and sorted. A folder's whole subtree is one range
  `dir_start..dir_end`, so `in:~/x` is a range bound, not a filter.
- **Interned names.** 10M entries share 3.5M distinct names. Each name is
  stored once with a 64-bit char-class mask. Queries score names, not
  entries.
- **Per entry:** parent, kind, size, mtime, name id.
- `name_ents`: each name maps to its entries.

### Live state (`live.rs`)

```
Live = base Index (immutable)
     + bitset of dead base entries
     + small overlay (BTreeMap) of entries added since
```

On the Mac, each FSEvents directory event lists that folder and diffs it.
The diff is idempotent, so replays and duplicates are harmless.

`Live::new` precomputes what a query would otherwise recompute every
keystroke. Most speed-up state lives here:

| Field | What it holds | Used by |
|---|---|---|
| `stat[i]` | Entry's static score: parent folder prior + name/kind/mtime tweaks | every scan |
| `order`, `ostat`, `o_en` | Entries sorted by `stat` desc, with their stats and name ids in that order | walks that stop early |
| `o_ne`, `o_nestat`, `peak` | Each name's entry list re-sorted by stat; `peak` = its best entry's stat | `byname` merge, pruning bounds |
| `ubase` | `peak − length penalty` (i16) | word-at-a-time pruning |
| `char_fit[b]` | Bitset of names whose mask has char class `b` (transposed masks) | exact fit-sets by AND, no rescan |
| `first_cls`, `first_fit`, `stem_pos`, `name_len8` | First char class, length, stem position per name | one-char queries scored without reading text |
| `ext_names` | Extension → names carrying it | `ext:` / `type:` |
| `oblk` | Size/mtime/kind min-max per 256-entry run of `order` | skipping runs in filter-only walks |
| `dir_names`, `dir_name` | Names any folder carries; per-folder name id | covers, dir memos |
| `names_cache`, `posneg_cache`, `cover_cache` | Reuse between keystrokes ([Caches between keystrokes](#caches-between-keystrokes)) | `names()`, pos-empty walks, covers |

### Query language and scoring (`query.rs`, the spec)

- **Words** are fuzzy subsequence matches.
  - Words of 5+ letters forgive one typo, at a cost of 60.
  - Modes: `'exact`, `^prefix`, `suffix$`, `!negate`.
- **Filters:** `ext:` `type:` `kind:` `in:` `size:` `mtime:` `re:` `path:`
  `limit:` `grep:` `regex:` `sym:`. The last three are content search,
  macOS only.
- **Name score** (fzf-style):

  ```
  matched char            +16
  open a gap              −3
  each gap char           −1
  boundary                +8
  camelCase               +7
  consecutive run         +4
  match starts name/stem  placement bonus
  ```

- **Entry score** = token scores (from the entry's own name or a folder on
  its path) + `stat[i]`. Ties break by entry index.
- **Answer** = top `limit` (default 50).

### Search pipeline

```
search(q) = search_base(q) ∪ search_overlay(q) → sort → top limit

search_base:
  scope_range (in:)  → [lo, hi)
  no positive words?
     ext filter      → names() → selective  | walk (stat order)
     else            → ok/neg name bitsets (posneg_cache) → walk (stat order, oblk run skipping)
  positive words:
     names(q)        → Scored { NameTable, cover? }          (step 1: per distinct name)
     route           (step 2: per entry)
       ok_entries ≤ 60K       → selective  (only entries carrying a matching name)
       table has a cover      → covered    (only the cover's subtree ranges + outside matches)
       one positive word      → byname     (merge stat-sorted per-name lists, floor breaks)
       else                   → full       (one parallel pass over all entries + dir memo)
```

| Step | How it works |
|---|---|
| `walk` | Goes down `order`, so the first `limit` survivors are the answer. Negations are checked lazily: only visited entries' folder chains (keep 10). Size/mtime/kind filters skip whole 256-entry runs via `oblk` (PR #5). |
| `byname` | Used when one positive word means an entry scores exactly name + stat. Merges the matching names' stat-sorted lists in parallel chunks. Skips a name whose best entry can't beat the floor. Each list stops when its stat drops below the floor. With negations it builds folder memos lazily per piece (keep 13). |
| `covered` (keep 12) | Every multi-word hit lies inside the cover of the word the table was restricted to (see [Scoring names](#scoring-names-names--score_names), step 6). Visits only that cover's folder subtree ranges, plus outside entries whose own name matches. Uses lazy `memo_of` folder memos. |
| `full` | Visits every entry. `dir_tokens` memos tell which words a folder above already covers. |
| `top_k` | Parallel top-k over pieces (threads × 8). Keys pack `score<<32 \| !index`, so one u64 compare orders by score, then index. |

### Scoring names (`names()` / `score_names`)

The hot step: up to 3.5M names per keystroke.

1. **Reuse.**
   - Same query as last time: return its table.
   - Query only narrows the last one (typing): rescan only names the last
     table kept (`bits` ∪ `pruned`), and carry scores for tokens already
     answered.
   - Otherwise: score from scratch.
2. **Parallel chunks** of 32K names (rayon).
   - A token's fit-set comes from `char_fit` ANDs, so names that can't match
     are never read.
   - Matches are stored sparse: per-chunk `ok_ids`, plus a 256 KB
     membership bitset.
3. **SIMD scorers.**

   ```
   names ≤64 bytes                          u64 position masks (mask64)
   names 65–128 bytes, query 2–16 chars     fused_masks
   everything else                          scalar memchr reference
   ```

   - AVX2 (x86, Intel lane) and NEON (aarch64, Mac lane) versions.
   - Loads happen in place from the mmap, with no per-name copy. A guard
     keeps a 64-byte read from crossing the name's page.
   - Built with `target-cpu=native`.
4. **Floor pruning.** Only when `prunable()`: one positive word, no filters,
   scope, overlay or dead entries.
   - Each chunk keeps a TopK of real best-entry keys and publishes rises to
     one shared `AtomicU64` floor.
   - Per 64-name word, a SIMD `below()` tests `ubase` against the floor, so
     whole words are skipped without reading text.
   - Survivors get a per-name upper bound from packed metadata.
   - Skipped names go to a `pruned` bitset, so later rescans still see them.
   - The floor is seeded before the pass from the last table's top names and
     the top-stat entries' names (keep 9).
5. **Lazy one-word tables.** A one-word table without name filters stores
   only `ok_ids`, with no 12 B/name dense array, and rebuilds a hit on
   demand. Small ones flatten `ok_ids` into a sparse copy (keep 8).
6. **Covers for multi-word queries** (keep 11).
   - A word's cover = names that fit it, plus every name in the subtrees of
     folders whose name matches it. A hit must lie in every positive word's
     cover.
   - The table scores only names in the smallest earlier word's cover, plus
     all folder names (they can still be a folder on a hit's path).
   - Excluded names go to `pruned`, so a backspace that drops the word
     rescans them.
   - Covers are cached for the last 8 words. They aren't built past 2M
     entries (e.g. `src`).

### Caches between keystrokes

All in memory, small and bounded. None is saved between runs.

| Cache | Holds | Size |
|---|---|---|
| `names_cache` | Last query's scored name table | 1 entry; dropped with spare buffers when idle |
| `posneg_cache` | Last no-positive-word query's ok/negated name bitsets | 1 entry |
| `cover_cache` | Covers of the last few words | 8 entries |

A typed character is usually the last query plus one letter. So "narrow from
the last table" is the core trick. It is also why a fresh process is nearly
as fast as a warm one: the cache refills after one keystroke.

## Acceptance gates

Every keep passed all of these:

```
1. build       native lib tests pass; other arch compiles
2. bench run   oracle: PASS; memory and startup within caps
3. AB9         B faster in ≥8 of 9 pairs AND median change ≤ −1.0%
4. holdout     every ~5 keeps; must pass and keep improving
5. random      random-disk checks, if the change alters which names or entries get looked at
6. unit test   one that fails when the change is broken
```

- **Build:** `cargo check --target aarch64-unknown-linux-gnu` on Intel and
  `x86_64-unknown-linux-gnu` on the Mac.
- **AB9:** `bench ab dev bin/engine.best bin/engine --pairs 9`. Interleaved
  pairs on the same machine. An earlier run used 6/7, then 7/9.
- **Holdout:** if dev improves but holdout doesn't, that is overfitting.
- **Random disks:**
  - `tools/diff-check.py`: 403 runs on generated disks vs main, 0 failures
    required;
  - `stress-check`: 300K–1M-entry random disks, typed chains with typos;
  - `cache-check`: cached vs cold answers.
- **Unit test:** workers mutate the new code on purpose and confirm the test
  catches it.

### Unit tests (10)

- `carry_preserves_negative_fuzzy_scores`
- `carry_preserves_negative_single_character_scores`
- `floor_pruned_cache_respects_entry_filters_and_larger_pages`
- `packed_dot_scores_match_generic_for_leading_dots`
- `typed_chains_match_cold_searches`
- `cover_restricted_chains_match_unrestricted`
- `stat_filter_walk_matches_brute_force`
- `mask64_matches_memchr_reference` (200K random cases)
- `mask64_boundary_edges`
- `below_and_floor_score`

Run them with `RUSTFLAGS="-C target-cpu=native" cargo test --release --lib`.
Without the flag the SIMD code isn't compiled, so fewer tests run.

### Benchmark locks and flags

- All builds and benchmarks take a lock: `flock /tmp/bfs-bench.lock` on
  Intel, `lockf -k /tmp/bfs-mac-bench.lock` on the Mac. A build overlapping
  an AB pair skews it.
- Always pass `--engine` to `bench run`. Otherwise it runs the main
  checkout's binary (the bench path is set at compile time).

## Lanes and PRs

### Lanes

One branch per machine type, because AVX2 and NEON code can't be tested on
the other chip:

```
lane/intel    Ubuntu, AVX2, 16 threads    rounds r1–r3; each round first re-gates the last one's in-flight commit
lane/mac      Apple M1 Pro
```

Lanes merge `origin/main` in, never rebase. Keeps go to `main` through PRs
with `--match-head-commit`.

### PR timeline

| PR | What | Result |
|---|---|---|
| #1 | C++ engine, 14 keeps from two lanes | **Closed.** Rust was faster on both machines (Intel 1358 vs 2469; M1 Pro 1321 vs 1912), and the Mac variant wasn't the certified one |
| #2 | Rust engine, 5 certified keeps (Intel + Apple lanes) | Merged → `5ac1518` |
| #4 | Intel lane: AVX2 scorers, per-chunk `ok_ids`, lazy tables, floor pruning, byname with negations | Merged: Intel −32%, Mac −25% |
| #3 | Mac lane: NEON 128-byte masks, in-place loads | Merged: M1 Pro −10.5% on top of #4 |
| #5 | Stat-filter run skipping (`oblk`) | Merged: filter-only searches ~0.1 ms |
| #6 | Repo rearrange: `tools/`, `tests/`, `docs/`, `archive/`; `.firecrawl` untracked | Merged, no deletions |
| #7 | Intel r3: floor seeding, lazy dir negation, token covers, cover-only scan, lazy memos | Merged: Intel −30%, M1 Pro −32% |

## Decisions

| Decision | Why |
|---|---|
| **Rust over C++ and Zig** | C++ and Zig engines were built in parallel. Rust won on both machines and kept the reference semantics in one codebase. Zig hadn't passed the oracle when stopped and used 4.5 GB. |
| **Freeze the bench; byte-identical answers** | The only way "faster" means anything. Any answer drift is a bug, not a trade-off. |
| **AB9 with ≥8/9 and ≤−1%** | The box is noisy (self-vs-self ±1.5%). One run lies; 9 interleaved pairs with a median threshold don't. |
| **Holdout trace + random disks** | Guards against tuning to the 409 dev keystrokes or the synthetic disk. The holdout kept improving with dev every round. |
| **One lane per machine** | AVX2 and NEON work can't be tested on the other chip. Separate lanes let each move at full speed. Shared logic was re-ported, not blindly merged. |
| **Re-implement Mac pruning on Intel instead of merging** | lane/mac's keep 1 conflicted with the AVX2 scorers and couldn't be tested on x86. Porting the design onto the Intel code was safer. Later, Mac keep 1 was redone in main's shape (PR #3). |
| **"Count bytes, not cycles"** | Measured: the names phase is memory-bandwidth bound on the box (15 GB/s on 1 thread, 16 GB/s on 16). Fuzzy scoring was ~10% of it. Wins came from writing fewer bytes per candidate (lazy tables, per-chunk `ok_ids`, pruning), not faster math. |
| **Score distinct names, not entries** | 3.5M names vs 10M entries, and each name's work is reused across every entry carrying it. |
| **Precompute in `Live::new`** | Each keystroke saves far more than startup pays. Through all Intel lane keeps, startup stayed within ~0.1 s and peak memory at 1767 MB. |
| **Safety by construction for pruning** | Floors only ever come from real entries' keys, so they are valid lower bounds whatever the thread timing. Pruned names are kept in a bitset so a backspace can still see them. Seed bounds use index 0, so they sort above any real key at the same score. |
| **Covers only from earlier words** | While typing the last word, its cover would change every keystroke. Earlier words' covers are built once per chain and cached. |
| **`target-cpu=native`** | A large measured win: −14% metric on its own in the first run (`b56734c`). Cost: a binary built on one CPU can crash on an older one, so build on the machine that runs it. |
| **Filter-only searches via run summaries** | 256-entry min/max blocks over the stat order cost <1 MB and turn "find nothing" from a full gather into a skip. |
| **Delete, but tag** | Old logs, notebooks and one-off tools left the tree; tag `history/pre-cleanup` keeps them one checkout away. |
| **Keep all branches** | Discarded experiments and closed PRs stay on origin as a record of what didn't work. |
| **Repo private by default** | Made public briefly for an external benchmark, then private again. The history scan found no secrets. |

## Rejected experiments

| Idea | Result | Lesson |
|---|---|---|
| Static-order walk for one-word queries (`cd22514`) | 9137 vs ~3900 | Random gathers kill it; sequential order matters more than early stop |
| Lazy name scoring in the walk (`5208a0d`) | 23781 | Upper bounds never beat multi-char floors, so it scanned deep |
| Seed the walk floor from top names (`c030d69`, run 1) | Slower | Wrong place. Seeding the pruned name table's floor later worked (keep 9) |
| mmap disk + `MADV_WILLNEED` (`8aa386e`) | 5/9 | Within noise |
| Sparse bitset rank lookup (`7e08670`) | +1.5% | Binary search over cumulative rank was not the bottleneck |
| Multi-word scan over the smallest cover (`196aaf7`) | 3/9, +0.9% | Building the cover per query cost more than it saved. Fixed by caching covers per word and restricting **names** first (keeps 11–12) |
| Finer name chunks (16K), more top-k pieces | No gain | Scheduling wasn't the limit; bandwidth was |
| Prefetching name rows | No gain | Same reason |

## Branch map

| Branch | What |
|---|---|
| `main` | The product |
| `lane/intel`, `lane/mac` | Active optimization lanes |
| `perf/filter-blocks`, `chore/rearrange` | Merged single-purpose PRs |
| `release/rust-engine` (merged #2), `release/cpp-engine` (closed #1) | Lane unification branches |
| `run/20261009-1011` | First autonomous run (see `docs/HANDOFF.md`) |
| `exp/intel-rust`, `exp/mac-rust`, `exp/intel-cpp`, `exp/mac-cpp`, `exp/cpp-engine`, `exp/zig-engine` | Per-language, per-machine experiments |
| `exp/scout`, `exp/scout2` | Idea scouting |
| `cand/*` | Candidates awaiting certification in the first run |
| `probe/*` | Small Mac probes |
| `wip/mac-filtered-prune` | Unfinished pruning for filtered queries |

## Known limits and next steps

| Item | Status |
|---|---|
| Binary portability | Native builds can crash on older CPUs. Runtime CPU detection (one build, pick AVX2/NEON at startup) would fix it. |
| Real-disk numbers | Everything here uses a synthetic disk loaded from a file. The macOS app's first crawl (`walk.rs`) and FSEvents path were not re-measured. A real head-to-head with upstream fsearch and fff on real folders is still to do. |
| Linux | Name search only. Content search, live updates and the daemon are macOS-only. |
| Next Intel work | Restrict `ext:`/`type:` filtered queries (written, tests pass, stashed in the lane/intel worktree, not yet gated); then size/mtime-filtered one-word queries. |
| Filtered pruning (`wip/mac-filtered-prune`) | Extend floor pruning to ext/regex/kind/scope/size/mtime queries. |
| Cleanup | The `PROF_*` stage timers and walk debug output in `query.rs` could move behind a feature flag. |
