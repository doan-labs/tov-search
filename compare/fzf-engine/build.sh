#!/bin/sh
# Build fzf-engine: fzf at a pinned tag, with bench.go dropped into its package.
set -eu
FZF_TAG=v0.65.2
here=$(cd "$(dirname "$0")" && pwd)
src="$here/target/fzf-$FZF_TAG"
[ -d "$src" ] || git clone -q --depth 1 --branch "$FZF_TAG" https://github.com/junegunn/fzf "$src" 2>/dev/null
cp "$here/bench.go" "$src/src/bench.go"
mkdir -p "$src/cmd/fzf-engine" && cp "$here/main/main.go" "$src/cmd/fzf-engine/main.go"
(cd "$src" && go build -trimpath -o "$here/target/fzf-engine" ./cmd/fzf-engine)
echo "$here/target/fzf-engine"
