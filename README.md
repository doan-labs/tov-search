# tov-search

Fast fuzzy file search for the whole disk on macOS and Linux. It indexes every
file once, then answers each query in about a millisecond and tolerates typos.
It can also search file contents.

Started as a fork of [fsearch](https://github.com/noahdunnagan/fsearch), then
rewritten in [Tov](https://tov.sh/) as an experiment.

## Install

macOS (Apple Silicon):

```sh
brew install doan-labs/tap/tov-search
```

Linux: build from source, see [rust/README.md](rust/README.md).

## Usage

```sh
tov-search main.rs                     # find files by name
tov-search 'readme in:~/Developer'     # only inside a folder
tov-search 'ext:rs grep:apply_dir'     # search file contents
```

Query syntax:

- Words are fuzzy. Words of 5 or more letters allow one typo.
- `'exact`, `^prefix`, `suffix$`, `!exclude`
- Filters: `ext:` `type:` `kind:` `in:` `size:` `mtime:` `path:` `re:`
  `grep:` `regex:` `sym:` `limit:`

The full CLI, the JSON API and the Rust crate are documented in
[rust/README.md](rust/README.md).

## Performance

On an M1 Pro with 10.4M files and folders, time per search:

| | |
|---|---|
| Name search, whole disk | 0.8 ms |
| Name search with a typo | 0.7 ms |
| Content search | 0.8-1.5 ms |
| First index of the disk | ~60 s, once |

Compared with [fsearch](https://github.com/noahdunnagan/fsearch) and
[fff](https://github.com/dmtrKovalenko/fff): see [compare/](compare/) and
[docs/benchmarking.md](docs/benchmarking.md).

## Repository layout

| Folder | Contents |
|---|---|
| [`rust/`](rust/) | The CLI and daemon you install, plus the benchmark harness |
| [`tov/`](tov/) | A second engine written in [Tov](https://tov.sh) with C kernels; it must return the same results as the Rust one |
| [`compare/`](compare/) | Benchmarks against fsearch and fff |
| [`docs/`](docs/) | Architecture, testing and benchmarking |

## Development

```sh
curl -fsSL https://tov.sh/install.sh | sh                              # install the Tov compiler
(cd rust/bench && cargo build --release && target/release/bench gen)   # generate the test disk
tov/check.sh                                                           # build, test and time
```

Contributing rules are in [AGENTS.md](AGENTS.md).

## Credits

Based on [fsearch](https://github.com/noahdunnagan/fsearch) by Noah Dunnagan.
MIT license.
