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

Compared with [fsearch](https://github.com/noahdunnagan/fsearch) and
[fff](https://github.com/dmtrKovalenko/fff): see [compare/](compare/) and
[docs/benchmarking.md](docs/benchmarking.md).

## Repository layout

| Folder | Contents |
|---|---|
| [`tov/`](tov/) | The engine, CLI and background program, in [Tov](https://tov.sh) with C kernels |
| [`bench/`](bench/) | The benchmark harness, test disk generator, traces and saved answers |
| [`compare/`](compare/) | Benchmarks against fsearch and fff |
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
