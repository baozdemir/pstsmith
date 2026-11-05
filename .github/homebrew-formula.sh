#!/bin/sh
# The Homebrew formula of a release: macOS (arm64, x86-64) and Linux (arm64, x86-64) archives with their SHA-256.
#   sh .github/homebrew-formula.sh vX.Y.Z DIST_DIR > pstsmith.rb
set -eu
TAG=$1
DIST=$2
VERSION=${TAG#v}
BASE="https://github.com/baozdemir/pstsmith/releases/download/$TAG"
sum() { sha256sum "$DIST/pstsmith-$1.tar.gz" | cut -d' ' -f1; }
cat <<RUBY
class Pstsmith < Formula
  desc "Convert EML, MBOX and Maildir mail to Outlook PST files, without Outlook"
  homepage "https://github.com/baozdemir/pstsmith"
  version "$VERSION"
  license "MIT"

  on_macos do
    on_arm do
      url "$BASE/pstsmith-aarch64-apple-darwin.tar.gz"
      sha256 "$(sum aarch64-apple-darwin)"
    end
    on_intel do
      url "$BASE/pstsmith-x86_64-apple-darwin.tar.gz"
      sha256 "$(sum x86_64-apple-darwin)"
    end
  end

  on_linux do
    on_arm do
      url "$BASE/pstsmith-aarch64-unknown-linux-musl.tar.gz"
      sha256 "$(sum aarch64-unknown-linux-musl)"
    end
    on_intel do
      url "$BASE/pstsmith-x86_64-unknown-linux-musl.tar.gz"
      sha256 "$(sum x86_64-unknown-linux-musl)"
    end
  end

  def install
    bin.install "pstsmith"
  end

  test do
    assert_match "pstsmith #{version}", shell_output("#{bin}/pstsmith --version")
  end
end
RUBY
