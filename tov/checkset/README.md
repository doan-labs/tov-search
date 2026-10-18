# Check-set disks

Query traces for these disks were written before any timing. Never tune on
them. Run a candidate on them once, after it passes dev AB9.

| disk | source | entries |
|---|---|---:|
| `seed2.bin` | benchmark generator, seed `0x5eed2` | 10,000,861 |
| `linux.bin` | Linux `a5ebb76233b79db01e82e061173cb73c6d2b5c6b` | 102,402 |
| `llvm.bin` | LLVM `llvmorg-19.1.0` = `a4bf6cd7cfb1a1421ba92bca9d017b49936c55e4` | 160,632 |

The disks were generated outside the repo from the source at
`688079831120928c23eb0b6e55e9f94311458a3b`. `compare.py mkdisk` keeps the
checkout's `.git` tree in each disk.

## Alternate seed

Copy the frozen benchmark source, leaving out its `DATASET` fingerprint file
(in the copy only), then build and generate. Run the commands here from the
repo root:

```sh
rsync -a --exclude 'target/' --exclude 'data/' --exclude 'out/' --exclude 'DATASET' \
  bench/ ~/checkset/gen-seed2/
cd ~/checkset/gen-seed2 && cargo build --release && \
  target/release/bench gen --disk ~/checkset/disks/seed2.bin
```

The only source change in the copy (`src/generate.rs:227`):

```diff
-        r: Rng::new(0x5eed),
+        r: Rng::new(0x5eed2),
```

Generator fingerprint: `f40dcfc215a561f1`.

## Linux

```sh
git init -q ~/checkset/src/linux
git -C ~/checkset/src/linux fetch -q --depth 1 \
  https://github.com/torvalds/linux a5ebb76233b79db01e82e061173cb73c6d2b5c6b
git -C ~/checkset/src/linux checkout -q FETCH_HEAD
python3 compare/compare.py mkdisk \
  ~/checkset/src/linux ~/checkset/disks/linux.bin
```

## LLVM

```sh
git init -q ~/checkset/src/llvm-project
git -C ~/checkset/src/llvm-project fetch -q --depth 1 \
  https://github.com/llvm/llvm-project refs/tags/llvmorg-19.1.0
git -C ~/checkset/src/llvm-project checkout -q FETCH_HEAD
git -C ~/checkset/src/llvm-project rev-parse HEAD
python3 compare/compare.py mkdisk \
  ~/checkset/src/llvm-project ~/checkset/disks/llvm.bin
```

## macOS filesystem

Check out Linux on a case-sensitive filesystem. The pinned Linux source has
13 tracked paths that collide by case: 12 netfilter upper/lower case variants
and `Z6.0+pooncelock+poonceLock+pombonce.litmus`. Normal case-insensitive
APFS silently loses files.

Before `mkdisk`, check that every tracked path is present and the checkout is
clean.

Record the disk hash and entry count for each disk you generate. `mkdisk`
includes `.git` metadata, so counts can differ between Git clone layouts.
Compare counts only when the source revision and full tracked tree match.
