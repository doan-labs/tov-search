# tov-search

Fast fuzzy file search for the whole disk on macOS and Linux. It indexes every
file once, then answers each query in about a millisecond and tolerates typos.
It can also search file contents.

Started as a fork of [fsearch](https://github.com/noahdunnagan/fsearch), then
rewritten in [Tov](https://tov.sh/) (TypeScript compiled to C) with
hand-written C kernels for the hot paths.

## Install

macOS (Apple Silicon) and Linux (x86_64):

```sh
curl -fsSL https://raw.githubusercontent.com/doan-labs/tov-search/main/install.sh | sh
```

Or with Homebrew: `brew install doan-labs/tap/tov-search`. To build from
source, see [tov/README.md](tov/README.md).

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

`tov-search` with no arguments lists the commands (`serve`, `status`,
`index`, `install`); `--json` prints results as JSON.

## Performance

Every query typed key by key; the sum of each keystroke's median time, lower
is better. Apple M2 Pro VM, 5 cores, macOS 15.

| File list | tov-search | [fsearch](https://github.com/noahdunnagan/fsearch) | [fzf](https://github.com/junegunn/fzf) |
|---|---|---|---|
| Linux kernel, 102,368 files | **45.2 ms** | 68.7 ms | 1,052 ms |
| LLVM, 204,749 files | 202.9 ms | **188.3 ms** | 4,396 ms |
| Chromium, 557,077 files | 549.8 ms | **362.5 ms** | 26,004 ms |
| 10M-file test disk, holdout (350 keys) | **847.5 ms** | 1,220.8 ms | not run (hours) |

On the 10M-file disk tov-search also used less memory (1,171 vs 1,572 MB)
and CPU (56% vs 82% of all cores); fsearch started faster (2.9 vs 3.8 s).
fzf has no filters and indexes files only, so it runs plain word queries.
Single runs vary by about ±20%. How to run them, and
[fff](https://github.com/dmtrKovalenko/fff) too: [compare/](compare/) and
[docs/benchmarking.md](docs/benchmarking.md).

## Repository layout

| Folder | Contents |
|---|---|
| [`tov/`](tov/) | The engine, CLI and background program, in [Tov](https://tov.sh) with C kernels |
| [`bench/`](bench/) | The benchmark harness, test disk generator, traces and saved answers |
| [`compare/`](compare/) | Benchmarks against fsearch, fff and fzf |
| [`docs/`](docs/) | Architecture, testing and benchmarking |

## Development

```sh
curl -fsSL https://tov.sh/install.sh | sh                              # install the Tov compiler
(cd bench && cargo build --release && target/release/bench gen)        # generate the test disk
tov/check.sh                                                           # build, test and time
```

Contributing rules are in [AGENTS.md](AGENTS.md).

## Credits

Based on [fsearch](https://github.com/noahdunnagan/fsearch) by Noah Dunnagan.
MIT license.
