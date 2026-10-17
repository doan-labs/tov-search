# tov-search

Whole-disk file search for macOS. Finds any file by name in about a
millisecond, forgives typos, and searches inside files with an index. Use it
as a CLI (with a small daemon) or as a Rust crate.

```
brew install doan-labs/tap/tov-search   # Apple Silicon
tov-search tov-search main           # find files by name
tov-search 'ext:rs grep:apply_dir'   # search inside files
```

Or build it: `cargo build --release && ./target/release/tov-search install`
(to `~/.local/bin`).

## Speed

M4 Max, 7.7M files and folders on disk.

| | |
|---|---|
| find a file by name, whole disk | p50 1.3 ms |
| search inside files | p50 9 ms |
| a new, renamed or deleted file shows up | ~0.1 s |
| first crawl of the disk | ~20 s, once |
| daemon memory | 30-135 MB |

## Comparison with fff

Chromium (509k files), same Mac, same queries.

| | tov-search | [fff](https://github.com/dmtrKovalenko/fff) |
|---|---|---|
| find a file by name | 1.1 ms | 13.8 ms |
| search inside files | 5.6 ms | 53 ms |
| typo still finds the file first | 98% | 88% |
| ready after launch | 50 ms | 2.5 s |
| memory | 50 MB (whole disk) | 358 MB (that folder) |

On the smaller Linux kernel (96k files), name search is a tie and tov-search
wins the rest. fff searches the contents of about 9% more files, because
tov-search skips some file types and `build/` and `vendor/` folders.

## Query syntax

```
tov-search 'readme in:~/Developer'          # inside a folder
tov-search 'type:image size:>5mb mtime:<7d'
tov-search 'ext:rs regex:fn\s+\w+_dir'      # regex inside files
tov-search 'sym:apply_dir'                  # where it's defined
```

- Words are fuzzy. Words of 5+ letters forgive one typo: `mian.rs` finds
  `main.rs`.
- Also `'exact`, `^prefix`, `suffix$` and `!exclude`.
- Filters: `ext:` `type:` `kind:` `in:` `size:` `mtime:` `re:` `path:`
  `grep:` `regex:` `sym:` `limit:`.
- Content search is smart-case.

## Full Disk Access

| started | access |
|---|---|
| from a terminal with Full Disk Access | indexes everything |
| as a login item (`tov-search install --login`) | give `~/.local/bin/tov-search` its own grant in System Settings > Privacy & Security, again after each rebuild |
| without access | skips the protected folders instead of popping a prompt |

## API

JSON lines over `~/Library/Application Support/TovSearch/tov-search.sock`, or
`tov-search stdio`:

```json
{"q": "tov-search main", "limit": 20}
{"op": "grep", "pattern": "apply_dir", "in": "~/Developer"}
```

Or link the crate:

```rust
let engine = tov_search::Engine::start(tov_search::Options { dir: tov_search::default_dir(&home), home: home.clone(), skip: None })?;
let hits = engine.search(&tov_search::Query::parse("tov-search main", &home)?)?;
```

An app and the CLI share one index: the first process owns it, the others
follow along.

## Release

Two steps, both needed. Skip the second and `brew` keeps installing the
old version.

1. Bump `version` in `Cargo.toml`, merge, then tag `main` and push the tag:
   `git tag v0.2.0 && git push origin v0.2.0`. There is no release workflow
   (no macOS runner): on an Apple Silicon Mac, check out the tag, run
   `RUSTFLAGS='-C target-cpu=apple-m1' cargo build --release --locked --manifest-path rust/Cargo.toml`,
   then `tar -czf tov-search-aarch64-apple-darwin.tar.gz -C rust/target/release tov-search`,
   `shasum -a 256 tov-search-aarch64-apple-darwin.tar.gz > tov-search-aarch64-apple-darwin.tar.gz.sha256`
   and `gh release create v0.2.0 --generate-notes tov-search-aarch64-apple-darwin.tar.gz*`.
2. Then `rust/tools/brew.sh v0.2.0`. It points the formula
   in [doan-labs/homebrew-tap](https://github.com/doan-labs/homebrew-tap) at the
   new build.

Check: `brew update && brew upgrade tov-search` gets the new version.

## Folders

```
src/              the engine and CLI
bench/            frozen benchmark harness (see bench/PROTOCOL.md)
tools/            diff-check and stress-check (candidate vs reference), ab2 (A/B timing), realdisk
build.sh          builds bin/engine (required by the benchmark protocol)
```

Design, decisions and history: [docs/rust-architecture.md](../docs/rust-architecture.md).

## Index

```
crawl     getattrlistbulk, once
updates   FSEvents; a restart replays only what changed
names     one mmap'd file, folder by folder, so in: is a range; each distinct name is scored once
content   trigram index of your text files; matches are read fresh from disk, so never stale
```
