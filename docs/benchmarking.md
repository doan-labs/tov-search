# Benchmarking

## Score

We time a search for every key typed, then add the times up. Lower is
better.

```
typed    s     sr    src
time     2ms   1ms   1ms    score = sum over every key of every query
```

Every answer must match the saved answer exactly, or the run fails. All
engines search the same 10M-file test disk.

## Query lists

Tune only on `dev`. The other lists check that a gain is real.

```
dev          tune on this
holdout      checks the gain isn't only on dev queries
random       checks the gain isn't only on these query types   (compare.py gen, new list each run)
other disks  checks the gain isn't only on the test disk        (tov/checkset/)
```

## Per-query check

The total can go down while most queries get slower:

```
             before    after
query A      500ms     250ms    faster
20 others     10ms      12ms    slower, each
total        700ms     490ms    total is 30% lower, but 20 of 21 queries are slower
```

So we also compare each query on its own. `geo mean` = new time ÷ old time
per query, averaged so one big query can't dominate. Below 1 means faster.

## Steps to keep a speed change

```
1. tov/check.sh     passes
2. dev              new wins ≥8 of 9 runs and is ≥1% faster
3. holdout          report the result
4. per query        on holdout and a random list: geo mean ≤1.00, slower count ≤ faster count
5. memory, startup  ≤1.5× main
```

```sh
bench ab dev     /abs/old /abs/new --pairs 9      # bench = bench/target/release/bench
bench ab holdout /abs/old /abs/new --pairs 9
python3 compare/compare.py gen bench/data/disk.bin /tmp/random.txt
python3 compare/compare.py run /tmp/random.txt --disk bench/data/disk.bin --engine old=/abs/old --engine new=/abs/new
```

Run the last line on `bench/traces/holdout.txt` too. Use one idle
machine, one job at a time. Put every number in the PR, including bad ones.

## fsearch and fff

`compare/` times Tov, fsearch and fff on holdout, a random list and the
Linux source (fsearch only builds on macOS, so run it on a Mac; CI no longer
does). It is one run and varies
±20%, so it isn't proof. fff skips hidden and ignored files, so it searches
fewer files.
