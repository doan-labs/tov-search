# Engine protocol

The benchmark drives the engine as a black box, so it can be written in any
language. `./build.sh` (repo root) must leave an executable at `bin/engine`.

## Startup

    bin/engine <disk-file>

Read the disk file, build whatever index you like in memory, then print
`ready\n` on stdout and flush. Time until `ready` is reported as `startup_s`.
Anything on stderr goes straight to the terminal.

## Queries

One query per stdin line (UTF-8, no trailing `\n` in the query itself). For
each, write the results, one per line, then an empty line, and flush:

    <score>\t<path>\n
    ...
    \n

If the query does not parse, write `error\n\n`. Exit on EOF.

The answer for every keystroke must be byte-identical to
`bench/oracle/<trace>.tsv`, which the reference engine (tov-search, `src/`)
produced: same hits, same scores, same order, at most 50 lines. Query syntax
and ranking are whatever `src/query.rs` does; read it, it is the spec.
Timing is from writing the query to reading the empty line.

## Disk file

Little-endian, written by `bench gen` (`bench/src/disk.rs`):

    "FSDISK01" | n: u64 | now: u32 | n records
    record: parent u32 | kind u8 | size u64 | mtime u32 | name_len u16 | name

- Record 0 is the root (empty name); a path is the names from the root
  joined by `/`, e.g. `/Users/bench/Developer/x/src/main.rs`.
- Parents come before their children.
- Names are never empty (except the root) and never contain `/ \t \n \r`.
- kind: 0 file, 1 dir, 2 symlink, 3 other; `| 4` = hidden flag.
- `now` is the clock that file ages are ranked against. Never read the real
  clock. The home folder (`~` in queries) is `/Users/bench`.
