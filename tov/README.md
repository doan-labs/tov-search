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
| `engine.tov`, `src/*.tov` | the engine: a small entry module plus concern modules, about 9,800 lines of TypeScript. `src/app.tov` is the command line and background program; `src/{cscope,cgrams,segs,segbuild,plan,grep,csearch,content}.tov` are content search |
| `kernels/simd/` | SIMD prefilter, case-folding substring search, and zero-safe bit scan |
| `kernels/par/` | multicore prefilter, name scoring, negation marking, folder scores, entry pass, content gram counting and filling |
| `kernels/grams.h` | the content gram hashing shared by `gramCount` and `gramFill` |
| `kernels/io/` | what Tov can't do: stdin, sockets, file watching, mapped files, reading many files at once, regex, background child processes |
| `kernels/simd.h`, `par.h`, `par.c` | the only CPU- and OS-specific code |
| `inject.py` | reads the hooks, writes `hooks.h`, wires in kernels and shadow mode |
| `build.sh`, `check.sh` | the build and the full check |
| `tools/diff_oracle.py` | compares a raw engine transcript with the oracle |
| `tools/grepcheck.py` | content index and `grep:`/`regex:`/`sym:` answers against the Rust app on a real disk file |
| `bugs/` | small repros of Tov compiler bugs, each with its workaround |

## Content search

`grep:`, `regex:` and `sym:` work as in the Rust app, on the same index
format. The background program builds and merges the index in a child
process (`tov-search content-build`, `content-merge`), so searches are
answered meanwhile. `tools/grepcheck.py` checks that the index files and the
answers match the Rust app byte for byte.

## Differences from Rust

- Regexes run on the system's POSIX regex, so a bad pattern's error text is
  not Rust's.
- `read`, the count of files opened by a content search, is exact in Tov. Rust
  reads a few files ahead in parallel, so its count runs a few higher.
