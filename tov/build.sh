#!/bin/sh
# Build the Tov engine: engine.tov -> C -> link kernels -> one binary.
#
#   ./build.sh                     build/engine                    pure Tov (only the raw stdin hook)
#   ./build.sh simd                build/engine-simd               plus every kernel in kernels/simd/
#   ./build.sh simd par            build/engine-simd-par           plus kernels/par/ (later sets win a clash)
#   ./build.sh simd par shadow     build/engine-simd-par-shadow    each kernel checked against its Tov body
#   ./build.sh simd par portable   build/engine-simd-par-portable  runs on any CPU of this family
#
# A native build uses everything this CPU has (-march/-mcpu=native), so it may
# not start on an older CPU. A portable build targets the baseline (x86-64 with
# SSE2, or arm64 with NEON); on x86 every kernel is also built for AVX2 and that
# copy runs when the CPU has it.
#
# macOS or Linux (glibc or musl), clang or gcc: CC picks the compiler (clang if
# found, else cc), CFLAGS adds flags. Needs `tov` (https://tov.sh) at the commit
# below: the hook contract reads Tov's generated C, so a different Tov is
# refused (TOV_ANY=1 to try one anyway).
set -e
cd "$(dirname "$0")"
TOV_PIN=ba81eb0d9cffe745411cafdd112c59d86cd7deb8
TOV="${TOV:-$(command -v tov || echo "$HOME/.cargo/bin/tov")}"
TOV_SRC="${TOV_SRC:-$HOME/.tov/src}"
have=$(git -C "$TOV_SRC" rev-parse HEAD 2>/dev/null || echo "unknown")
if [ "$have" != "$TOV_PIN" ] && [ -z "$TOV_ANY" ]; then
  echo "build: tov source at $TOV_SRC is $have, tested with $TOV_PIN (TOV_ANY=1 to try anyway)" >&2
  exit 1
fi
CC="${CC:-$(command -v clang || echo cc)}"

variant=plain
out=build/engine
kernels="kernels/io/*.c"
shadow=
portable=
if [ $# -gt 0 ]; then
  variant=$(echo "$@" | tr ' /.' '---' | tr -s '-')
  out="build/engine-$variant"
  for v in "$@"; do
    case "$v" in
      shadow) shadow=--shadow ;;
      portable) portable=1 ;;
      *) kernels="$kernels kernels/$v/*.c" ;;
    esac
  done
fi

case "$(uname -m)" in
  x86_64 | amd64) cpu=-march=native base=-march=x86-64 wide=-mavx2 ;;
  aarch64 | arm64) cpu=-mcpu=native base='' wide='' ;; # NEON is in the baseline
  *) cpu=-march=native base='' wide='' ;;              # simd.h falls back to plain C
esac
widen=
if [ -n "$portable" ]; then
  cpu=$base
  [ -n "$wide" ] && widen=--wide
fi
case "$(uname -s)" in
  # A 512 MiB main stack. Elsewhere Tov's main() moves itself to a 1 GiB thread.
  Darwin) link="-DTVG_MAIN_STACK=1 -Wl,-stack_size,0x20000000" ;;
  *) link=-pthread ;;
esac

mkdir -p "build/obj/$variant"
"$TOV" build engine.tov -o build/engine-tov --emit-c build/engine.c >/dev/null

# Align functions and loops to 64 bytes so hot loops don't speed up or slow
# down when unrelated code moves them across cache lines.
FLAGS="-O3 -std=gnu11 -flto -ffp-contract=off -fno-stack-protector -U_FORTIFY_SOURCE -D_FORTIFY_SOURCE=0 -falign-functions=64 -falign-loops=64 $cpu ${CFLAGS:-}"
# Kernels are plain C held to their prototype in hooks.h: every warning is an error.
# -Wconversion: a contract field can't be narrowed or sign-flipped silently.
WARN="-Wall -Wextra -Wconversion -Werror -Wno-unused-parameter -Wmissing-prototypes"
# shellcheck disable=SC2086
contract=$(python3 inject.py engine.tov build/engine.c "build/engine.$variant.c" build/hooks.h $shadow $widen $kernels)
obj="build/obj/$variant"
# shellcheck disable=SC2086
$CC $FLAGS -pthread $WARN -c kernels/par.c -o "$obj/par.o"
objs="$obj/par.o"
for k in $contract; do
  n=$(basename "$k" .c)
  # shellcheck disable=SC2086
  $CC $FLAGS $WARN -Ikernels -include build/hooks.h -c "$k" -o "$obj/$n.o"
  objs="$objs $obj/$n.o"
  if [ -n "$widen" ]; then
    # shellcheck disable=SC2086
    $CC $FLAGS $wide $WARN -Ikernels -include build/hooks.h "-Dk_$n=k_${n}__wide" -c "$k" -o "$obj/$n.wide.o"
    objs="$objs $obj/$n.wide.o"
  fi
done
# shellcheck disable=SC2086
$CC $FLAGS -w -DTOV_UNCHECKED=1 $link "build/engine.$variant.c" $objs -o "$out" -lm
echo "built $out"
