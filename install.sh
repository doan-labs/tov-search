#!/bin/sh
# Install tov-search: curl -fsSL https://raw.githubusercontent.com/doan-labs/tov-search/main/install.sh | sh
# Downloads the latest release for this machine, checks its checksum, then
# runs `tov-search install`, which puts it in ~/.local/bin.
# Pass `--login` (… | sh -s -- --login) to also start it at login.
set -eu

repo=doan-labs/tov-search

if command -v brew >/dev/null 2>&1 && brew list tov-search >/dev/null 2>&1; then
    echo "tov-search is installed with Homebrew; update it with: brew upgrade tov-search"
    exit 0
fi

case "$(uname -s)-$(uname -m)" in
    Darwin-arm64) target=aarch64-apple-darwin ;;
    Linux-x86_64)
        # The x86_64 build needs AVX2 (Intel 2013+, AMD 2015+).
        grep -qw avx2 /proc/cpuinfo || { echo "this CPU lacks AVX2; build from source (see rust/README.md)" >&2; exit 1; }
        target=x86_64-unknown-linux-gnu ;;
    Linux-aarch64 | Linux-arm64) target=aarch64-unknown-linux-gnu ;;
    *) echo "no prebuilt tov-search for $(uname -s) $(uname -m); build from source (see rust/README.md)" >&2; exit 1 ;;
esac

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
url="https://github.com/$repo/releases/latest/download/tov-search-$target.tar.gz"
echo "downloading $url"
curl -fsSL "$url" -o "$tmp/t.tar.gz"
want=$(curl -fsSL "$url.sha256" | cut -d' ' -f1)
if command -v sha256sum >/dev/null 2>&1; then got=$(sha256sum "$tmp/t.tar.gz" | cut -d' ' -f1); else got=$(shasum -a 256 "$tmp/t.tar.gz" | cut -d' ' -f1); fi
[ "$got" = "$want" ] || { echo "checksum mismatch" >&2; exit 1; }
tar -xzf "$tmp/t.tar.gz" -C "$tmp"
"$tmp/tov-search" install "$@"

case ":$PATH:" in
    *":$HOME/.local/bin:"*) ;;
    *) echo "add ~/.local/bin to your PATH (in ~/.zshrc or ~/.bashrc): export PATH=\"\$HOME/.local/bin:\$PATH\"" ;;
esac
