#!/bin/sh
# Points the Homebrew formula in doan-labs/homebrew-tap at release <tag>, so
# `brew install doan-labs/tap/tov-search` gets it. Run it once the release
# workflow has uploaded the build:  rust/tools/brew.sh v0.1.0
set -eu
tag=$1
tarball=tov-search-aarch64-apple-darwin.tar.gz
path=repos/doan-labs/homebrew-tap/contents/Formula/tov-search.rb
sha=$(gh release download "$tag" -R doan-labs/tov-search -p "$tarball.sha256" -O - | cut -d' ' -f1)
formula=$(cat <<EOF
class TovSearch < Formula
  desc "Whole-disk fuzzy file search for macOS"
  homepage "https://github.com/doan-labs/tov-search"
  url "https://github.com/doan-labs/tov-search/releases/download/$tag/$tarball"
  sha256 "$sha"
  version "${tag#v}"
  license "MIT"
  depends_on arch: :arm64
  depends_on :macos

  def install
    bin.install "tov-search"
  end
end
EOF
)
old=$(gh api "$path" -q .sha 2>/dev/null || true)
gh api -X PUT "$path" -f message="tov-search $tag" \
  -f content="$(printf '%s\n' "$formula" | base64 | tr -d '\n')" ${old:+-f sha="$old"} >/dev/null
echo "homebrew-tap: tov-search $tag"
