#!/bin/sh
# Everything that must hold before a Tov change lands, in one command:
#   1. plain, simd, simd+par and portable builds answer dev and holdout exactly like the oracle
#   2. shadow builds: every kernel matches its Tov reference body on every call
#   3. shadow mode still works: a planted kernel bug must be caught
#   4. timing of the fast build (bench run dev; TIMING=0 skips it)
# Run from anywhere, on macOS or Linux. Stops at the first failure.
set -e
cd "$(dirname "$0")"
BENCH=../rust/bench
B=$BENCH/target/release/bench
DISK=$BENCH/data/disk.bin

step() { printf '\n== %s\n' "$*"; }

step "0. injector unit tests"
python3 -m unittest discover -s . -p 'test_inject.py'

step "1. oracle"
for v in "" "simd" "simd par" "simd par portable"; do
  # shellcheck disable=SC2086
  ./build.sh $v >/dev/null 2>&1 || ./build.sh $v
  e=build/engine${v:+-$(echo "$v" | tr ' ' '-')}
  for t in dev holdout; do
    printf '%-28s %-8s ' "$e" "$t"
    # Not piped: a pipeline's status is grep's, which would hide bench's exit 1.
    if ! r=$($B run $t --engine "$e" --rounds 1); then echo "$r" | grep '^oracle:'; exit 1; fi
    echo "$r" | grep '^oracle:'
  done
done

step "1b. trailing-zero kernel unit test"
"${CC:-cc}" -std=c11 -Wall -Wextra -Wconversion -Werror -Ibuild \
  -include build/hooks.h \
  tests/trailingZeros64.c kernels/simd/trailingZeros64.c -o build/trailingZeros64-test
build/trailingZeros64-test

step "2. shadow"
for v in "simd" "simd par"; do
  # shellcheck disable=SC2086
  ./build.sh $v shadow >/dev/null 2>&1 || ./build.sh $v shadow
  e=build/engine-$(echo "$v" | tr ' ' '-')-shadow
  for t in dev holdout; do
    printf '%-28s %-8s ' "$e" "$t"
    if ! sed -n 's/^@@ //p' $BENCH/oracle/$t.tsv | "$e" $DISK >build/shadow-$t.out 2>build/shadow-$t.err; then
      echo "FAIL"; grep -v '^ready$' build/shadow-$t.err >&2; exit 1
    fi
    python3 tools/diff_oracle.py $BENCH/oracle/$t.tsv build/shadow-$t.out
  done
done

step "3. shadow self-test (planted bug in findFolded)"
rm -rf build/mutant && mkdir -p build/mutant
sed 's/return find_folded(\(.*\));/return find_folded(\1) + (a->to - a->from > 30);/' \
  kernels/simd/findFolded.c >build/mutant/findFolded.c
grep -q 'a->from > 30' build/mutant/findFolded.c || { echo "FAIL: could not plant the bug"; exit 1; }
./build.sh simd ../build/mutant shadow >/dev/null 2>&1 || ./build.sh simd ../build/mutant shadow
if sed -n 's/^@@ //p' $BENCH/oracle/dev.tsv | build/engine-simd-build-mutant-shadow $DISK 2>&1 >/dev/null | grep 'shadow: findFolded'; then
  echo "caught"
else
  echo "FAIL: shadow mode missed a planted bug"; exit 1
fi

if [ "${TIMING:-1}" != 0 ]; then
  step "4. timing"
  if ! r=$($B run dev --engine build/engine-simd-par); then echo "$r" | grep '^oracle:'; exit 1; fi
  echo "$r" | grep -E '^(metric|oracle):'
fi
printf '\ncheck: all passed\n'
