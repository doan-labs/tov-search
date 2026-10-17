# Build, kernels and where to change code

How `engine.tov` and the `tov/src/` modules become a binary, how kernels are
injected and shadowed, and which functions to touch for each kind of change.

## Hook locations

Hooks live in the module that owns their globals.

| Hook | File |
|---|---|
| `readLine` | `engine.tov` |
| `rankedIndexBuild` | `src/rankedindex.tov` |
| `findFolded`, `rfindFolded` | `src/score.tov` |
| `prefilter`, `scoreNamesLoop` | `src/names.tov` |
| `allMemos` | `src/memos.tov` |
| `markNegs` | `src/negs.tov` |
| `fullPass` | `src/entries.tov` |
| `trailingZeros64` | `src/masks.tov` |

## Build and injection

Run `tov/build.sh` from anywhere; it changes to `tov/`.

```text
engine.tov -> tov build --emit-c -> build/engine.c
           -> inject.py + selected kernels -> build/engine.<variant>.c + hooks.h
           -> compile typed kernel objects -> cc with LTO -> build/engine[-variant]
```

### Generated output

- Tov also emits `build/engine-tov`. Use the final injected binary.
- `build/hooks.h`, patched C and `build/obj/<variant>/` are generated. Edit
  source contracts and kernels, never these files.
- Builds share `engine.c` and `hooks.h`, so run one build at a time.

### inject.py checks

`inject.py` follows `import ... from "./..."` from `engine.tov` and parses
hooks and globals from every module. It checks these against Tov's emitted C:

- `f<m>_<name>` and `g<m>_<global>` names
- positional `p0...` signatures
- global types
- a hook's `writes:` are globals of its own module

It rejects unknown globals/types, read/write overlap, invalid write bounds
and kernels without a matching hook. A compiler naming change is an
injection failure. Don't patch generated C by hand.

### Typed interface

Each typed hook gets `k_<name>_args` and `k_<name>()`:

| Contract item | C type |
|---|---|
| read array | const pointer + length |
| write array | mutable pointer + length |
| read scalar | value |
| write scalar | pointer |

Shims make written arrays unique before exposing pointers to C.

### Compiler flags

- Kernels compile separately with `-Wall -Wextra -Wconversion -Werror` and
  forced `hooks.h`.
- Final link: `-O3 -flto`, `-ffp-contract=off`, `TOV_UNCHECKED=1`.
- Every compile aligns functions and loops to 64 bytes, so timing doesn't
  swing with code layout.
- `CC` selects clang/gcc. `CFLAGS` adds flags.
- Native builds use `-march=native`/`-mcpu=native`.
- macOS requests a 512 MiB main stack. Linux links pthreads.

### Compiler pin

- `TOV_PIN` in `build.sh` must match CI's `TOV_PIN`.
- The script checks `TOV_SRC` (default `~/.tov/src`) is at that commit.
  `TOV` selects the compiler.
- `TOV_ANY=1` is an explicit experiment override, not a verified toolchain.

### Build variants

| Command | Output / meaning |
|---|---|
| `tov/build.sh` | `tov/build/engine`: Tov bodies plus raw I/O. |
| `tov/build.sh simd` | `engine-simd`: SIMD hooks, one query thread. |
| `tov/build.sh par` | `engine-par`: parallel hooks; folded-search Tov hooks remain, while C name scoring uses SIMD helpers. |
| `tov/build.sh simd par` | `engine-simd-par`: combined fast build. Later sets win duplicate filenames (`par/prefilter.c` replaces SIMD prefilter). |
| `tov/build.sh simd par shadow` | `engine-simd-par-shadow`: compare selected typed kernels with Tov bodies on every call. |
| `tov/build.sh simd par portable` | `engine-simd-par-portable`: baseline SSE2 on x86-64 / NEON on arm64; x86 also compiles AVX2 copies, selected by runtime CPU support. |

### SIMD bit scan

`trailingZeros64(bits)` returns the least-significant set-bit index for
nonzero input, 64 for zero. The SIMD C kernel guards zero, then calls
`__builtin_ctzll`. LTO lowers the nonzero scan to a native CPU bit-scan
sequence. The ranked walk still clears and visits each candidate word in the
same order.

## Shadow builds

For each hook call:

```
1. keep the original written arrays (Tov copy-on-write)
2. run the Tov reference body, copy out its results
3. restore the original arrays/scalars
4. run the kernel from that exact pre-call state
5. compare
```

Copying bytes back into a possibly aliased array was unsafe (see
`03dd6d0`'s body).

- Checks: unchanged lengths, return values, declared writes.
- On mismatch: abort with hook/query/call/slot diagnostics.
- `writes: a[:return]` or `a[:writtenScalar]` compares only the populated
  prefix. Unbounded arrays compare fully.
- Raw I/O is not shadow-compared.

## Where to change code

| Change | Tov | Also update |
|---|---|---|
| Syntax or filters | `parse()`, `filter()`, `pushToken()`, range/unit helpers | Compare with Rust `Query::parse()`/`filter()`. Name-side filter changes also affect `nameKey()`/`narrows()` and C name scoring. |
| Scoring | `fuzzyScore()`, `tokenScore()`, typo helpers, `rankTweaks()`/`scoreEntry()` | Matching helpers in `par/scoreNamesLoop.c` or `par/fullPass.c`. These C ports bypass Tov helper calls, so changing only the Tov helper leaves fast builds stale. |
| Layout, scope or priors | `readDisk()`, `build()`, `encDec()`, `priorAdjust()`, `lookup()` | Compare Rust `Index::build()` and accessors. Revisit kernel topology plans, padding and ID assumptions. |
| Ranked index build | `rankedIndexBuild()` | See below. |
| Candidate pruning, cache reuse | `prefilter()`, `scoreNamesLoop()`, `nameKey()`/`narrows()`, `search()` | Their kernels. Keep partial and negative names for directory memos, not just final usable names. |

`rankedIndexBuild()` builds the eager name priors and packed name info, the
key/head/short by-key lists, class counts and start-fit bitmaps. The parallel
kernel uses per-chunk histograms and chunk-ordered scatters so IDs stay
ascending within each key bucket. Shadow compares all written arrays and
scalars with the scalar Tov body.

### New kernel

1. Mark a standalone Tov function with contiguous `// hook:`, `// reads:`,
   `// writes:` comments.
2. Build to get its typed header.
3. Add `tov/kernels/<set>/<function>.c` defining `k_<function>()`.
4. Select the set via `build.sh`.
5. Test with shadow, then run the full gate.

Unreplaced hooks keep running their Tov bodies.

### Kernel rules

- A hook may leave changes only in its declared `writes:` globals. Array
  lengths must not change.
- `fullPass()` uses `topScore/topIdx` as scratch but empties them on entry
  and exit. Keep that boundary state.
- Keep kernels typed, including local pointers to contract fields. Don't hide
  type changes behind casts or untyped `memcpy`. Pointer element changes,
  narrowing and sign changes should fail compilation; safe scalar widening
  can compile. Shadow is still needed for semantic mistakes.

## Rejected SIMD scoring experiment

C13 on [`exp/tov-c13-mask64`](https://github.com/doan-labs/tov-search/tree/exp/tov-c13-mask64),
at `f13f5a3`, is not active.

- It cached equality masks for names 1–64 bytes and queries 2–64 bytes.
- Correctness tests and mutants passed on Linux AVX2/SSE2/scalar and Mac
  NEON/scalar.
- It failed the dev gate at −0.82% and 3/9 wins.
- None of its scorer, helpers or tests are part of this branch.

See [final tuning results](testing.md#final-tuning-results).
