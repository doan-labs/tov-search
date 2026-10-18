# Architecture

Where everything lives, how the disk becomes an index, and what one
keystroke does in `tov/engine.tov` and the modules in `tov/src/`.

## Files

| Path | Role |
|---|---|
| `tov/engine.tov` | Engine entry: raw stdin hook, answer/output, `main`. |
| `tov/src/*.tov` | The engine, split by concern: consts, bytes, disk, prior, masks, index, rankedindex, query, score, names, exts, memos, negs, topk, entries, ranked, covers, search, plus `app`, `live` and the content modules (see Content index). Hook bodies define the behavior their C replacements must keep. |
| `tov/kernels/io/readLine.c` | Required raw stdin hook. Flushes stdout; shadow query diagnostics. |
| `tov/kernels/simd/` | `prefilter.c`, `findFolded.c`, `rfindFolded.c`, `trailingZeros64.c`: vector mask rejection, folded byte search, zero-safe bit scan. |
| `tov/kernels/par/` | `prefilter.c`, `scoreNamesLoop.c`, `markNegs.c`, `allMemos.c`, `fullPass.c`, `gramCount.c`, `gramFill.c`: parallel replacements. Name scoring is a C port of Tov's scoring helpers. |
| `tov/kernels/simd.h`, `par.h`, `par.c` | Shared SIMD operations and pthread pool. CPU/OS-specific kernel code goes here. |
| `tov/inject.py` | Parses hook contracts, validates generated C, emits typed interfaces and replacement/shadow shims. |
| `tov/build.sh`, `tov/check.sh` | Compiler pipeline; complete correctness gate. `tov/build/` is generated output. |
| `tov/tools/diff_oracle.py`, `tov/bugs/` | Offline transcript comparison; minimal Tov compiler bug repros. |
| `tov/tools/diff-check.py`, `stress-check.py`, `ab2.py` | Two builds on synthetic disks and query chains, byte-compared; interleaved A/B timing. |
| `bench/` | **Frozen spec:** harness, disk generator/fingerprint, traces, saved oracle answers, `PROTOCOL.md`. |
| `compare/` | Timing against fsearch and fff. |
| `.github/workflows/tov.yml` | Linux compiler/platform matrix; pinned Tov install. |

## Oracle

- `bench/oracle/` is the byte-level answer spec.
- Old notebooks and dumps are at `history/pre-cleanup`, not in the working
  tree.

## Startup

```
main()  ->  readDisk()  ->  build()  ->  allocate query tables  ->  extIndexBuild()  ->  print "ready"  ->  answer stdin until EOF
```

The engine reads a disk snapshot. It does not read the real filesystem.

## Disk format

From the [protocol](../bench/PROTOCOL.md), little-endian:

```
header   FSDISK01 | n:u64 | now:u32
record   parent:u32 | kind:u8 | size:u64 | mtime:u32 | name_len:u16 | name
```

- Record 0 is the empty-name root. Parents come before children.
- `now` is the ranking clock. `HOME` is `/Users/bench`. Never use the wall
  clock.
- `readDisk()` fills record-order `rParent`, `rKind`, `rSize`, `rMtime`,
  `rOff`, `rLen` over `buf`.
- `build()` groups children by parent and sorts them by raw name bytes
  (`sortKids()`/`recLess()`).

## ID spaces

Four separate coordinate systems. Don't mix them.

| ID | Count | Arrays |
|---|---|---|
| disk record | header `n` | `rParent`, `rKind`, `rSize`, `rMtime`, `rOff`, `rLen` (freed after interning) |
| entry | `n` | `entName`, `entKind`, `entParent`, `entSize`, `entMtime` |
| directory | `d` | `dirEntry`, `dirStart`, `dirLen`, `dirEnd`, `dirPrior` |
| name | `u` | `unames`/`uoff`, `umask`, `neOff`, `peak` |

- `entParent` holds a **directory ID**.
- `dirEntry` maps directory ID to entry ID. Parents have lower directory IDs.

## Entry order

Entries are **directory blocks in DFS order**. A directory's immediate
children form one sorted block; child directory blocks follow.

```
entry   0    1    2    3     4     5
name    ""   a/   b/   a/x   a/y   b/z
block   -    root root a/    a/    b/       (entry 0 is the root)
```

- `dirStart..dirStart+dirLen` is the immediate-child block.
- `dirStart..dirEnd` is the whole subtree.
- `lookup()` binary-searches the child block, then tries a case-insensitive
  fallback.
- `in:` becomes a half-open subtree range, excluding the scoped directory
  itself.

## Names and masks

- Names are interned in entry order into `unames`. Name `id` is
  `uoff[id]..uoff[id+1]`.
- **Keep the 64 trailing padding bytes.** SIMD loads may read past a name's
  end, then mask off lanes outside its range.
- Temporary disk/record arrays are freed after interning.
- `umask` is one `u64` per name (`charBit()`, `startBit()`, `nameMask()`):

```
bits 0..40    character classes
bits 41..63   hashed word starts
```

Masks allow false positives, never false negatives. They don't prove a
match.

## Ranked index

`rankedIndexBuild()` eagerly builds:

- signed name keys, capped length/stem, folded heads
- maximum parent priors
- key/head/short lists
- 41 class counts
- `rankedStartFit`: start hashes 41..63 transposed into `23 × ceil(u/64)`
  name-ID bitmaps

The parallel kernel and the scalar body produce byte-identical index arrays.

## Extension lists

`extIndexBuild()` (`tov/src/exts.tov`) groups name ids by extension: the
bytes after a name's last dot.

- An extension of up to 7 bytes packs into one `u64` key:
  `(length + 1) << 56` plus its folded bytes. Longer ones share one list.
- A hash table maps keys to groups. `extIds` holds each group's ids in
  ascending order, at `extOff[g]..extOff[g+1]`.
- Names with no dot are in no list.

## Static scores

- `dirPrior` folds `priorAdjust()` down the tree: home/location boosts,
  cache/bundle penalties, clamped to [-100, 60].
- `encDec()` makes `entSize` the oracle's lossy encoded size.
  Filtering raw disk sizes instead would change answers.
- `statOrder` holds entries 1..n-1, stable, descending by
  `dirPrior[entParent] + rankTweaks(nameFlags, kind, mtime)`.
  `statOrdered` stores those scores. The root is not searchable.
- `neOff[id]..neOff[id+1]` indexes the entries in `ne` with that name. Each
  list is stat-descending, entry IDs ascending within equal stats. `neStat`
  holds each entry's static score; `peak` holds the name's maximum. This
  serves selective queries and bounded one-token merges without scanning all
  entries.
- `namePrior` is the maximum parent `dirPrior` for a name.

Why the first `ne` entry's prior is also a safe bound:

```
stat        = prior + rankTweaks
rankTweaks <= 10 + 25·app − 8·dot     (non-.app names: <= 10; hidden and dot penalties only lower it)
```

The first entry's stat is no less than any entry's, and the maximum tweak is
no less than its own. The bound carries through the common length penalty
and placement in `key`/`cheap`.

## Keystroke flow

```
answer -> parse -> search -> [ranked walk] -> prefilter -> scoreNamesLoop -> memos -> entry pass -> scoreEntry/push -> output
```

1. **Parse.** `answer()` calls `parse()` (`splitWords()`, `filter()`,
   `pushToken()`). Invalid queries print `error\n\n`. Tokens carry byte text,
   mode, negation and masks. At most eight positive tokens fit in `hBits`.
   `search()` resolves scope, flattens positives then negatives into
   `tText/tOff/tMode/tMask/tLoose/tStart`, and extensions into `xText/xOff`.

   Two query shapes skip the name table entirely:

   - No name terms at all (only kind, size, mtime, scope or limit): every
     entry's score is its static score, so `statOrderPass()` walks
     `statOrder` best first, applies `passes()` and stops after `topK`
     hits. The cached name table stays as it was.
   - Only negated words (no positive word, no extension): `loadNegs()`
     (`tov/src/negs.tov`) runs the `markNegs()` hook, which sets a bit in
     `negBits` for each name a negation matches, redone only when the
     tokens change. `statOrderPass()` skips entries whose name or a
     folder above it is marked — `dirNeg()` folds and caches that per
     directory — and stops after `topK` hits.
2. **Cache.** `nameKey()` depends only on tokens and extensions.
   - Same key: reuse the name table and memos, even if kind, size, date,
     scope or limit changed.
   - Otherwise `gen` advances. If `narrows()` proves the new query can only
     narrow the last one, `cand` and `presBuf` swap and only previously
     present names are rescored.
   - Crossing the five-byte typo threshold must invalidate narrowing.
     Backspaces and changed negations need fresh work.
   - An eligible single fuzzy token may first try the ranked walk — kind,
     size and mtime filters allowed (see [Ranked walk](#ranked-walk)).
3. **Prefilter.** Without narrowing, `prefilter(qNpos + qNneg)` collects
   names fitting **any** token, including negatives needed for ancestor
   exclusion.
   - An `ext:`/`type:` query with at most one positive word and no
     negation takes its candidates from `extCand()` (`tov/src/exts.tov`)
     instead: the ascending ids in the per-extension name lists. That
     query kind uses only names that pass `extOk()` and reads no folder
     memos, so the lists skip only names that could never be used.
   - With two or more positive words, `restrictCand()`
     (`tov/src/covers.tov`) keeps only names in the smallest cover of an
     earlier word, plus every folder name. A word's cover is the names
     that fit it plus every name under a folder whose name matches it;
     every hit lies in it. Covers are cached for the last 8 words (text
     and mode) and skipped when their folder subtrees hold more than 2M
     entries. A narrowed table stays inside the same cover.
   - With no positives — only an extension query with a negated word gets
     here — `scoreNamesLoop()` scans all names.
4. **Score names.** `scoreNamesLoop()` applies `fitsT()`, `tokenScore()`,
   `tokenMatches()`, `extOk()`, `nameFlags()`.
   - Fuzzy scoring uses `findFolded()` and `rfindFolded()`. Fuzzy tokens of
     five bytes or more try one edit at word starts, with a 60-point penalty.
   - Scores accumulate with `clamp16()`.

   | Table | Holds |
   |---|---|
   | `hStamp == gen` | marks live slots |
   | `hScore`, `hBits`, `hFlags`, `hBest0..hBest3` | scores, positive matches, flags, ancestor-score inputs |
   | `okBuf/okCount` | usable names |
   | `presBuf/presCount` | usable plus partial and negated matches |
   | `okEntries` | entry count of usable names |

5. **Directory memos.** `qNeedDirs` is true for multiple positives or any
   negation. `setMemo()` folds a directory's name with its parent's memo:
   `mBits == -1` excludes a negated subtree; otherwise bits combine and
   `mBest0..mBest3` keep the best nonnegative ancestor scores. `mStamp` uses
   the same `gen`.
6. **Entry pass.** A query with no name terms, or only negated ones, never
   reaches this step: step 1 answers it with a best-first `statOrderPass()`
   walk that stops after `topK` hits. Otherwise one route per query:

   | Case | Route |
   |---|---|
   | `okEntries <= 60000` | Walk matching names' `ne` lists, restrict to scope, `memoOf()` lazily, then `scoreEntry()` and `push()`. |
   | Table restricted to a cover, at most 60000 entries | `coveredPass()` visits the cover's folder subtrees and the outside entries whose own name has the word, with lazy memos. |
   | Larger, no positive tokens | Walk `statOrder` best first, apply entry filters and negation memos lazily, stop when the remaining static score/index bound can't beat the top-k floor. |
   | Larger, one positive token, no negation | Bucket matching names by `hScore + peak`, merge their stat-descending `ne` lists, prune names and list tails whose score/index bound can't beat the top-k floor. |
   | Other large queries | `allMemos()` when needed, then `fullPass(lo, hi)`. It writes the best entries to `kScore/kIdx`; `search()` moves them to `topScore/topIdx`. |

7. **Score entries and output.** `scoreEntry()`:
   - applies entry filters
   - requires every positive token to match the name or an ancestor, and some
     positive to match the own name
   - adds inherited scores at 3/4 for the first four tokens, then `dirPrior`
     and `rankTweaks()`

   `better()` orders by score descending, then entry ID ascending. `push()`
   keeps `topK` (default 50, set by `limit:`). `putPath()` rebuilds paths.
   Each response is `score\tpath\n` rows then a blank line. `readLine`
   flushes before waiting for input.

## Ranked walk

Before the table path, an eligible single fuzzy token may use the
ranked walk. kind, size and mtime filters are allowed; the
walk skips entries that fail `passes()`.

```
class-count estimate
  -> head/short candidates in progressive score-bound batches
  -> complete start-fit set
  -> remaining names that can beat the floor
```

| Ranked result | Effect |
|---|---|
| `Some` | Answer comes from the walk. Skips `scoreNamesLoop()`. Sets `partialTable`. |
| `None` | Cache state untouched. Continue on the normal path. |

A ranked `Some` is not a reusable name table. Current-generation `h*` slots
may still describe a previous query and don't form a complete
`okBuf`/`presBuf` table.

- `rankedTry()` (`tov/src/ranked.tov`) sets `partialTable` through
  `setPartial()` (`tov/src/search.tov`) and returns before `search()` enters
  its normal cache branch.
- `prevKey`/`prevTokens`, `presBuf`/`presCount` and `okBuf`/`okCount` stay
  the last complete `scoreNamesLoop()` table.
- `narrows()` checks `partialTable`, and the cache branch forces a rebuild
  when it is set. That guard is required before any reader below treats the
  state as a complete table.

| State | Read by |
|---|---|
| `prevKey` | `narrows()`, cache-key test |
| `presBuf` | narrowing swap in the cache branch |
| `okBuf` | `fullPass()`, `byNamePass()`, entry fan-out |
| `hStamp` | `present()` |
| `hFlags`, `hBits`, `hBest0..hBest3` | `setMemo()` |
| `hScore`, `hBits`, `hFlags` | `scoreEntry()` (called by `fullPass()`/`byNamePass()`) |
| `hFlags` | no-positive entry path |

The ranked stop and incomplete-settle comparisons are strict `>`
boundaries, as the oracle needs. Equality coverage:

| Boundary | Status |
|---|---|
| `floor == base + cover + 29` | Reached by the deterministic `mkdisk` probe (109 == 56 + 24 + 29), only in the complete pass. Did not exercise the incomplete-settle comparison. |
| `floor == top - 1 - 256` | No fixture reached it. |

Both equality mutant sites remain untested at equality.

## Content index

`grep:`, `regex:` and `sym:` search file contents. Not in the benchmark.

- Format `BFCSEG02`: each segment is an immutable file in
  `<data dir>/content/` (4 KiB header, then 13 aligned sections: trigram
  keys and posting lists, doc table, paths, bloom filters). `.dead` files
  tombstone docs that changed or went away; `manifest` lists the segments.
- Segments are mapped read-only (`mapFile`). `segs.tov` opens them and diffs
  folders against the docs held; `plan.tov` turns a pattern into a trigram
  AND/OR; `csearch.tov` picks candidates per segment, drops those the bloom
  filters rule out, and reads the best-ranked files fresh from disk until
  enough match. Results never show stale content.
- Build and merge run in a child process of the background program:
  `content-build` writes a todo file's batches as new segments
  (`segbuild.tov`, `gramCount`/`gramFill`), `content-merge` merges eight
  segments of a size tier into one. The parent adds or swaps segments in
  the manifest only after the child exits, so searches go on meanwhile.
- A changed folder is synced once it has been quiet for 2 s, or 5 min after
  its first change.

## Hooks

The `reads:`/`writes:` comments next to each hook are the exact interface.
Read them before changing a hook.

| Hook | Inputs and outputs | Replacement |
|---|---|---|
| `readLine` (raw) | stdin to query string; `"\n"` sentinel at EOF | Pasted C body; required even for plain builds. |
| `findFolded`, `rfindFolded` | `unames`, byte bounds, folded byte; return index or -1; no writes | SIMD byte compares in `simd.h`. |
| `trailingZeros64` | `bits`; returns the least-significant set-bit index, or 64 for zero; no writes | `__builtin_ctzll` with a zero guard; LTO lowers it to the CPU's native bit-scan sequence. |
| `prefilter` | Name/token masks and `u`; writes ascending `cand[:return]` | SIMD scan; parallel version scans 32 chunks and concatenates in order. |
| `rankedIndexBuild` | Entry/name arrays, parent priors, bytes, masks; writes per-name ranking info, key/head/short lists, class counts, start-fit bitmaps | Per-name parallel work, chunk histograms, stable chunk-ordered scatters; scalar Tov body is the shadow reference. |
| `scoreNamesLoop` | Candidates (`cand` when `fromCand`, else ids `0..count`), name bytes/masks, flattened query, `neOff`, `gen`; writes h* tables, `okEntries`, `presCount`, `okBuf[:return]`, `presBuf[:presCount]` | Disjoint name slots in 16K chunks; ordered list concatenation. |
| `allMemos` | Directory topology, name hits, `gen`; writes m* tables | Cached tree plan: fold upper dirs first, then independent subtrees of at most 4096 dirs. |
| `markNegs` | Name masks/bytes, flattened negated tokens; writes `negBits` (one bit per name) | Parallel over 64-name words; a C port of `fitsT()` and `tokenMatches()`. |
| `fullPass` | Scope bounds, usable names, entry fields, memos, filters, ranking state; writes `kScore[:return]`, `kIdx[:return]` | Generation-cached usable-name bitset; 64 entry slices with local top-k, merge packed score/index keys. |
| `gramCount` | Content files `gBuf`/`gStart`/`gLen`/`gText`; writes 4 counts per file in `gCnt` (trigram keys, definition keys, short and long bloom words) | Per file in parallel, with the hashing in `grams.h`. |
| `gramFill` | The same plus `gCnt`, `gAt`; writes `gTri`, `gSym`, `gBloom`, `gLong` at each file's slots | Per file in parallel; emits exactly the counts `gramCount` gave. |
| `readDocs`, `docsChanged` (raw) | Request of paths (and sizes); per file its bytes, or a changed byte | Every core; regular files only, links not followed. |
| `mapFile`, `unmapFile` (raw) | A segment file as a read-only `u8[]` | `mmap` behind an immortal array header. `unmapFile` keeps the header page mapped: Tov still reads it after release. |
| `grepRe`, `grepReNext` (raw) | Compile the content regex; next match at or after an offset | System POSIX regex. |
| `spawnBg`, `childDone`, `background` (raw) | Start this program as a child with args; its exit status; lower this process's priority | `fork`/`exec`, `waitpid`, `setpriority` (macOS also QoS). |

## Thread pool

- `par_for()` uses one pthread pool on macOS and Linux. The caller also
  works.
- Call it from one thread at a time. Never call it recursively from a
  worker.
- `allMemos.c` assumes topology never changes.
- `fullPass.c` rebuilds its bitset when the generation changes and regrows
  it when `u` grows.
