# tov/

The tov-search engine in [Tov](https://tov.sh) (TypeScript compiled to C),
with hand-written SIMD and multicore C kernels swapped in for the hot
functions. How it all works: [root README](../README.md).

## Build and check

```sh
curl -fsSL https://tov.sh/install.sh | sh                                 # once: Tov
(cd ../bench && cargo build --release && target/release/bench gen)        # once: the benchmark disk

./build.sh                     # build/engine                 plain Tov
./build.sh simd                # build/engine-simd            + SIMD kernels
./build.sh simd par            # build/engine-simd-par        + multicore kernels (the fast one)
./build.sh simd par shadow     # every kernel checked against its Tov body on each call
./build.sh simd par portable   # runs on any CPU of this family, not just this one

../bench/target/release/bench run dev --engine build/engine-simd-par
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
| `tools/diff-check.py`, `stress-check.py` | two engine builds on synthetic disks and query chains, compared byte for byte |
| `tools/ab2.py` | interleaved A/B timing of two builds, keystroke by keystroke |
| `tools/brew.sh` | points the Homebrew formula at a release |
| `bugs/` | small repros of Tov compiler bugs, each with its workaround |

## Content search

`grep:`, `regex:` and `sym:` search file contents through a trigram index
(format `BFCSEG02`, see [architecture](../docs/architecture.md#content-index)).
The background program builds and merges the index in a child process
(`tov-search content-build`, `content-merge`), so searches are answered
meanwhile. Regexes run on the system's POSIX regex.

## Release

Two steps, both needed. Skip the second and `brew` keeps installing the
old version.

1. Merge, then tag `main` and push the tag:
   `git tag v0.2.0 && git push origin v0.2.0`. There is no release workflow
   (no macOS runner). Build each binary on its own platform from the tag, with
   `T` its name: `macos-arm64` on an Apple Silicon Mac
   (`./build.sh simd par portable`), and `linux-x64` on x86_64 Linux, linked
   statically so it runs on any distro
   (`CC=gcc CFLAGS=-static ./build.sh simd par portable`). Then
   `mkdir -p dist && cp build/engine-simd-par-portable dist/tov-search`,
   `tar -czf tov-search-$T.tar.gz -C dist tov-search`,
   `shasum -a 256 tov-search-$T.tar.gz > tov-search-$T.tar.gz.sha256`.
   Then `gh release create v0.2.0 --generate-notes tov-search-*.tar.gz*`
   (later ones: `gh release upload v0.2.0 tov-search-$T.tar.gz*`).
2. Once both are attached: `tools/brew.sh v0.2.0`. It points the formula
   in [doan-labs/homebrew-tap](https://github.com/doan-labs/homebrew-tap) at the
   new build.

Check: `brew update && brew upgrade tov-search` gets the new version.
