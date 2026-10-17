#!/bin/sh
# Points the Homebrew formula in doan-labs/homebrew-tap at release <tag>, so
# `brew install doan-labs/tap/tov-search` gets it, on macOS and on Linux. Run
# it once the release workflow has uploaded all three builds:
#   rust/tools/brew.sh v0.1.0
set -eu
tag=$1
base=https://github.com/doan-labs/tov-search/releases/download/$tag
path=repos/doan-labs/homebrew-tap/contents/Formula/tov-search.rb
sha() {
  gh release download "$tag" -R doan-labs/tov-search -p "tov-search-$1.tar.gz.sha256" -O - | cut -d' ' -f1
}
mac=$(sha aarch64-apple-darwin)
linux_x86=$(sha x86_64-unknown-linux-gnu)
linux_arm=$(sha aarch64-unknown-linux-gnu)
formula=$(cat <<EOF
class TovSearch < Formula
  desc "Whole-disk fuzzy file search"
  homepage "https://github.com/doan-labs/tov-search"
  version "${tag#v}"
  license "MIT"

  on_macos do
    depends_on arch: :arm64
    url "$base/tov-search-aarch64-apple-darwin.tar.gz"
    sha256 "$mac"
  end

  on_linux do
    on_intel do
      url "$base/tov-search-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "$linux_x86"
    end
    on_arm do
      url "$base/tov-search-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "$linux_arm"
    end
  end

  def install
    bin.install "tov-search"
  end
end
EOF
)
if [ "${BREW_DRY_RUN:-}" ]; then
  printf '%s\n' "$formula"
  exit
fi
old=$(gh api "$path" -q .sha 2>/dev/null || true)
gh api -X PUT "$path" -f message="tov-search $tag" \
  -f content="$(printf '%s\n' "$formula" | base64 | tr -d '\n')" ${old:+-f sha="$old"} >/dev/null
echo "homebrew-tap: tov-search $tag"
