# tov-search

Find any file on your disk by typing a few letters. It searches millions of
files in about a millisecond, and still finds the file if you make a typo.

It started as a fork of [fsearch](https://github.com/noahdunnagan/fsearch) by
Noah Dunnagan.

| engine | written in | role |
|---|---|---|
| Tov | [Tov](https://tov.sh) (TypeScript compiled to C), with the slowest parts replaced by hand-written C that uses SIMD and every CPU core | the main engine |
| Rust | Rust | the reference: Tov must give exactly the same answers |

## Folders

| folder | what it is |
|---|---|
| [`tov/`](tov/) | the main engine and its C kernels |
| [`rust/`](rust/) | the Rust reference, the macOS app, the benchmark |
| [`compare/`](compare/) | timing against [fsearch](https://github.com/noahdunnagan/fsearch) and [fff](https://github.com/dmtrKovalenko/fff) in CI |
| [`docs/`](docs/) | how it all works, in detail |

## Build and check

```sh
curl -fsSL https://tov.sh/install.sh | sh                              # install Tov
(cd rust/bench && cargo build --release && target/release/bench gen)   # make the test disk
tov/check.sh                                                           # build, check, time
```

## Search

### Index

The disk is loaded once into one big table:

```
table   folder by folder, so everything inside a folder sits together
names   many files share a name (index.js), so each name is stored and scored once
```

### Matching

- Each word you type is fuzzy: its letters must appear in order in the name.
- Word starts and letters in a row score higher.
- Words of 5+ letters forgive one typo: `mian.rs` finds `main.rs`.
- Files in your own folders rank above the same name deep in a cache.

### Search paths

The general path:

```
1. prefilter     skip every name that's missing a letter you typed
2. score names   score what's left, on every core
3. rank          keep the best 50 files
```

Some queries take a shortcut:

| query | path |
|---|---|
| one fuzzy word | checks the best-scoring names first and stops once the top 50 can't change |
| only exclusions or filters, no word to match | walks files from best to worst rank and stops at 50 |
| one word matching many names | merges each name's file list, already sorted by rank |
| several words | caches each word's matching names and scores only files every word can match |
| `ext:` or `type:` | scores only names with a matching extension |

### Query syntax

`'exact`, `^prefix`, `suffix$`, `!exclude`, and filters `ext:` `in:` `size:`
`mtime:`.

## Tov and kernels

```
engine.tov → tov → C → + kernels → engine
```

Tov's C is correct but slow: one core, no SIMD. So the hot functions in
`engine.tov` and its `tov/src/` modules are marked as hooks:

```ts
// hook: ids of the names any of the nt tokens could match, into cand.
// reads: umask tMask tLoose tStart u
// writes: cand[:return]
function prefilter(nt: int): int { ... }
```

A **kernel** is a small C file that replaces one hooked function. The build
gives it a typed header, so if the TypeScript changes, the kernel fails to
compile instead of reading the wrong memory.

| kernel set | what it does |
|---|---|
| `simd` | checks many bytes at once (NEON on ARM, SSE2, AVX2 or AVX-512 on x86) |
| `par` | splits the work across every core; about 11× faster |

A **shadow build** runs both the TypeScript and the kernel on every call and
stops at the first difference:

```
shadow: findFolded differs from its reference on query #1 "m" (call 49): returned 2908, the reference 2907
```

## Benchmark

| | |
|---|---|
| disk | a fake disk of 10 million files, shaped like a real developer's Mac; the same everywhere |
| queries | real searches, timed key by key: 409 keystrokes in `dev`, 371 in `holdout`; only `dev` is used for tuning |
| answers | every keystroke must match the saved Rust answer exactly |
| score | the total time for all keystrokes; lower is better |

A change must also be faster per query on queries nobody tuned against, so a
few slow queries can't carry the total. Full rules:
[docs/benchmarking.md](docs/benchmarking.md).

`tov/check.sh` checks every build and every kernel, then times it. CI runs it
on Linux with AVX-512, AVX2 and NEON.

### Results

Historical M4 Max measurements (10 cores), time per keystroke:

| engine | dev ms | holdout ms |
|---|---|---|
| Tov, 1 core | 30.8 | 24.8 |
| Tov + SIMD, 1 core | 29.0 | 23.0 |
| Rust, same algorithm, 10 cores | 2.75 | 2.41 |
| **Tov + SIMD + all cores** | **2.61** | **2.29** |
| Rust, fully tuned | 0.84 | 0.82 |

Latest Linux measurements:

```
CPU    x86-64, 8 logical CPUs, generic Intel Xeon; AVX2 yes, AVX-512 no
Tov    main and final: Clang 14.0.0, simd par, native -march=native, -O3, LTO
Rust   1.97.1, -C target-cpu=native
```

| engine | dev total ms (409 keys) | holdout total ms (371 keys) |
|---|---:|---:|
| Tov main (`79b67b0`) | 3529.189 | 3178.164 |
| Tov final (`0ebc30a`) | 2277.560 | 1727.169 |
| Rust reference from `0ebc30a` | 816.583 | 740.275 |

- Tov rows are nine-pair `bench ab` medians: final is **35.47% faster on dev
  and 45.66% faster on holdout, winning 9/9 pairs on both**. Both oracles
  passed.
- Rust rows are separate `bench run` totals (five timed rounds after warmup),
  not paired comparisons with Tov. Both oracles passed.
- Resources, the check set and full evidence:
  [final tuning results](docs/testing.md#final-tuning-results).
- The M4 table compares Tov and Rust on the same algorithm. On Linux, the
  fully tuned Rust reference is still faster on both traces. Its design:
  [docs/rust-architecture.md](docs/rust-architecture.md).

On 2-core CI machines (a different CPU each), time per keystroke:

```
AVX-512   28 ms
AVX2      33 ms
NEON      47 ms
```
