# Testing

How to prove a change is correct. How to prove it is faster is in
[benchmarking.md](benchmarking.md).

## Build and run

Needs the pinned Tov compiler, Python 3, a C compiler and Cargo. Commands
start at the repo root. Generate the disk once per machine.

```sh
(cd rust/bench && cargo build --release && target/release/bench gen)
tov/build.sh simd par
rust/bench/target/release/bench run dev --engine tov/build/engine-simd-par
rust/bench/target/release/bench run holdout --engine tov/build/engine-simd-par
tov/check.sh
```

- Always pass `--engine` to `bench run`. Without it, the harness runs
  `rust/bin/engine` from the checkout it was built in (it embeds
  `CARGO_MANIFEST_DIR` at compile time), not the Tov binary.
- `--engine` does not move the compiled-in trace and oracle paths. Rebuild
  the harness after moving a checkout.
- `bench gen` checks the deterministic disk against `rust/bench/DATASET`.

## tov/check.sh

Runs these steps in order and stops at the first failure:

```
injector unit tests
oracle            plain, SIMD, SIMD+parallel, portable    on dev and holdout
trailing-zero     kernel regression test
shadow            SIMD and combined transcripts           on dev and holdout
planted bug       a findFolded bug that shadow must catch
timing            fast build on dev                       TIMING=0 tov/check.sh skips it
```

Shadow is for correctness, not speed. To check one shadow transcript by hand:

```sh
tov/build.sh simd par shadow
sed -n 's/^@@ //p' rust/bench/oracle/dev.tsv |
  tov/build/engine-simd-par-shadow rust/bench/data/disk.bin > tov/build/shadow-dev.out
python3 tov/tools/diff_oracle.py rust/bench/oracle/dev.tsv tov/build/shadow-dev.out
```

## Changes to what is visited

A change to which names or entries are visited also needs a check shown to
fail when broken: a planted bug, a unit test, or a differential check on
arbitrary disks.

`rust/tools/diff-check.py` includes `re:` and `path:` queries that Tov does
not support, so passing it alone does not prove a Tov change. Pick checks
that cover the semantics you ported. Never change the frozen benchmark to fit
a candidate.

## Speed claims

See [benchmarking.md](benchmarking.md). Never run two benchmarks, a build and
a benchmark, or other heavy jobs on one machine at once. Besides timing
noise, Tov variants share generated files.

## CI

CI runs `tov/check.sh` on the self-hosted `custom-ci` runner, one job per SIMD width:

| arch | compiler | SIMD |
|---|---|---|
| x86-64 | clang | AVX-512 (fails if the runner lacks it) |
| x86-64 | gcc | AVX2 |

arm64 (NEON) and macOS are not in CI: run `tov/check.sh` locally.

`CFLAGS` also applies to portable builds, so the x86 SSE2 path is not tested
in CI.

Who gets CI runs:

```
draft PRs                                      never
public org members, logins in CI_TRUSTED       every push    (CI_TRUSTED: repo variable, a JSON list)
anyone else                                    once each time a maintainer adds the ci-approved label;
                                               after new pushes, remove and re-add it
PRs from forks                                 never         (an org policy)
```

The head-to-head with fsearch and fff needs a Mac and no longer runs in CI. See
[benchmarking.md](benchmarking.md#fsearch-and-fff).

## Final tuning results

```
engine revision   0ebc30a   https://github.com/doan-labs/tov-search/commit/0ebc30a50e54dfd6b561dabd0794e7914dca2e9a
baseline          79b67b0   https://github.com/doan-labs/tov-search/commit/79b67b072dd438a400aae22d686c0fd16ea341b5
PR                          https://github.com/doan-labs/tov-search/pull/5 (unmerged)
```

### Final versus main

Setup:

- Linux x86-64, 8 logical CPUs, Intel Xeon (model exposed only as
  `Intel(R) Xeon(R) Processor`), AVX2, no AVX-512.
- Both Tov engines: Clang 14.0.0, `simd par`, native `-march=native`, `-O3`
  and LTO, no extra `CFLAGS`. Rust 1.97.1 with `-C target-cpu=native`.
- Builds and timed runs did not overlap.
- Frozen disk: 10,000,541 entries, fingerprint `b4f408863e4a658d`. No frozen
  harness, generator, trace or oracle was changed.

These numbers replace the earlier pre-ranked final table. Don't compare their
absolute times with earlier AVX-512 or Apple Silicon runs.

| engine | dev total ms (409 keys) | holdout total ms (371 keys) |
|---|---:|---:|
| Tov main, 79b67b0 | 3529.189 | 3178.164 |
| Tov final, 0ebc30a | 2277.560 | 1727.169 |
| Rust reference from 0ebc30a | 816.583 | 740.275 |

- Tov rows: medians from `bench ab ... --pairs 9`. Final is **35.47% faster
  on dev and 45.66% faster on holdout, winning 9/9 pairs on both**.
- Rust row: separate `bench run` totals (five timed rounds after warmup),
  not paired with Tov. Rust is still faster on both traces.
- All oracles passed.
- No fresh fsearch/fff comparison was run at this revision.

### Never-tuned check set

Each disk ran once, after the final dev gate passed, with its unchanged
prewritten trace and `compare/compare.py run`: five timed rounds after warmup
per pass, median of three rotated passes. This is not a nine-pair A/B.

| disk | entries | Tov main ms | Tov final ms | change | Rust ms |
|---|---:|---:|---:|---:|---:|
| alternate seed | 10,000,861 | 3187.2 | 2247.1 | -29.50% | 738.3 |
| Linux a5ebb76 | 102,402 | 176.1 | 125.1 | -28.96% | 90.3 |
| LLVM 19.1.0 | 160,632 | 312.1 | 217.6 | -30.28% | 180.1 |

Every disk improved. All timed runs had zero unstable keys. These are the
original Linux check disks, not the separately generated Mac disks. Sources
and disk construction: [`tov/checkset/README.md`](../tov/checkset/README.md).

### Resources and variability

A separate resource pass read `/proc/<pid>/status` after all 780 frozen
dev/holdout query keys:

| engine | startup s | peak RSS KiB |
|---|---:|---:|
| Tov main | 4.717 | 1,216,316 |
| Tov final | 5.026 | 1,216,324 |
| Rust | 3.319 | 1,730,580 |

Final ÷ main in this pass: startup **1.066×**, peak RSS **1.000007×**, both
below the 1.5× limit.

Startup across the 18 recorded A/B pairs:

```
          median    range
main      4.885 s   4.59–8.48 s
final     4.985 s   4.85–9.02 s
```

Some paired startup readings were over 1.5× their paired baseline. The
resource pass does not prove every run stayed under that limit. No
unfavorable pair or startup outlier was removed or rerun.

### Correctness

`TIMING=0 CC=clang tov/check.sh` passed at exact final revision 0ebc30a:

- eight injector tests
- all plain/SIMD/parallel/portable dev and holdout oracles
- 10,194 trailing-zero cases
- both shadow variants matched all 409 dev and 371 holdout keys
- planted `findFolded` mutant caught

The kept final and Rust binaries also matched byte for byte on all nine check
disk × trace combinations: 2,703 keystrokes, zero mismatches.

### What was kept and rejected

Kept in the final branch:

- static-score walk
- one-token by-name merge
- ranked bitmap path
- concern-module split
- injector hardening
- native zero-safe bit scan

The requested ranked → covers → SIMD sequence is complete: ranked passed,
both cover variants failed the dev win-count gate, and cached SIMD scoring
failed both dev thresholds. Rejected experiments stay separate and are not
in the final engine.

| rejected experiment | dev improvement | winning pairs | disposition |
|---|---:|---:|---|
| c12, serial covers | 8.06% | 7/9 | rejected |
| c12b, parallel covers | 4.46% | 5/9 | rejected |
| c13, cached SIMD scorer | 0.82% | 3/9 | rejected |

None of these three got a holdout timing or check-set run after failing dev.

C13 correctness-only check on Mac, at f13f5a3 (Apple M4 Pro, Apple Clang 21):
native NEON and generic scalar each passed 2,323,812 cases and caught the
bit-63 mutant; plain/SIMD/parallel/portable oracles, shadow comparisons and
the planted `findFolded` mutant all passed. This is correctness evidence, not
a reason to keep it.

### Binary identities

| binary | SHA-256 |
|---|---|
| main 79b67b0 | `2baae56f514c0a458f1ce887b6b4aa4e7172f2493abef568a4233389e96eba91` |
| final 0ebc30a | `cddccdd27845a1b843959fbf2d173e784a4d54312ae900ac1bba067f2bacddfc` |
| Rust from 0ebc30a | `948b92fd9218f10494d9890666e65d4c7a7d3dff274f6f84989c7cd795eef083` |
