# tov/

The tov-search engine in [Tov](https://tov.sh) (TypeScript compiled to C),
with hand-written SIMD and multicore C kernels swapped in for the hot
functions. How it all works: [root README](../README.md).

## Build and check

```sh
curl -fsSL https://tov.sh/install.sh | sh                                 # once: Tov
(cd ../rust/bench && cargo build --release && target/release/bench gen)   # once: the benchmark disk

./build.sh                     # build/engine                 plain Tov
./build.sh simd                # build/engine-simd            + SIMD kernels
./build.sh simd par            # build/engine-simd-par        + multicore kernels (the fast one)
./build.sh simd par shadow     # every kernel checked against its Tov body on each call
./build.sh simd par portable   # runs on any CPU of this family, not just this one

../rust/bench/target/release/bench run dev --engine build/engine-simd-par
./check.sh                     # run before any change lands
```

`check.sh` steps:

```
1. injector unit tests and the trailing-zero kernel regression test
2. every build against the oracle
3. every kernel against its Tov body, in shadow mode
4. shadow mode catches a planted bug
5. times the fast build              TIMING=0 skips this
```

| setting | |
|---|---|
| Tov version | the build only accepts the Tov commit it was tested with (pinned in `build.sh`); `TOV_ANY=1` overrides |
| compiler | `CC=gcc` works too, on macOS and Linux, arm64 and x86-64 |

## Writing a kernel

1. Mark the function in `engine.tov` or its module in `src/` with `// hook:`,
   plus `// reads:` and `// writes:` listing the globals it touches (writes
   must be globals of the hook's own module).
2. Build once. `build/hooks.h` now has `k_<name>_args` and the prototype.
3. Write `kernels/<set>/<name>.c` defining `k_<name>`. Every warning is an
   error.
4. Run the shadow build, then `./check.sh`.

A hook's Tov body may change only its `writes:` globals, and must leave their
lengths as they were.

## Files

| path | what it is |
|---|---|
| `engine.tov`, `src/*.tov` | the engine: a small entry module plus concern modules, about 3,400 lines of TypeScript |
| `kernels/simd/` | SIMD prefilter, case-folding substring search, and zero-safe bit scan |
| `kernels/par/` | multicore prefilter, name scoring, negation marking, folder scores, entry pass |
| `kernels/io/` | `readLine`: Tov has no stdin |
| `kernels/simd.h`, `par.h`, `par.c` | the only CPU- and OS-specific code |
| `inject.py` | reads the hooks, writes `hooks.h`, wires in kernels and shadow mode |
| `build.sh`, `check.sh` | the build and the full check |
| `tools/diff_oracle.py` | compares a raw engine transcript with the oracle |
| `bugs/` | a 13-line repro of a Tov bug: `switch` on a string frees it too early |

## Not ported from Rust

- the `re:` and `path:` regex filters (Tov has no regex)
- live disk updates

Neither is in the benchmark.
