#!/usr/bin/env bash
# Write Formula/pour.rb into a Homebrew tap so `brew install jks-sys/tap/pour` works.
#   bash scripts/tap-formula.sh <version> <path-to-homebrew-tap>
set -euo pipefail
VERSION=$1; TAP=$2
cd "$(dirname "$0")/.."
sha() { python3 -c 'import hashlib,sys; print(hashlib.sha256(open(sys.argv[1],"rb").read()).hexdigest())' "$1"; }
ARM=$(sha dist/pour-macos-arm64); INTEL=$(sha dist/pour-macos-x86_64); LX=$(sha dist/pour-linux-x86_64); LA=$(sha dist/pour-linux-arm64)
mkdir -p "$TAP/Formula"
cat > "$TAP/Formula/pour.rb" <<RUBY
# Pour — the Homebrew-compatible package manager that installs in milliseconds.
# Copyright (c) 2026 Jagadeesh Kumar S. All rights reserved.
class Pour < Formula
  desc "Homebrew-compatible package manager that installs in milliseconds"
  homepage "https://ipconfig.co.network/pour"
  version "$VERSION"
  license :cannot_represent

  on_macos do
    on_arm do
      url "https://ipconfig.co.network/pour/dl/v$VERSION/pour-macos-arm64"
      sha256 "$ARM"
    end
    on_intel do
      url "https://ipconfig.co.network/pour/dl/v$VERSION/pour-macos-x86_64"
      sha256 "$INTEL"
    end
  end
  on_linux do
    on_arm do
      url "https://ipconfig.co.network/pour/dl/v$VERSION/pour-linux-arm64"
      sha256 "$LA"
    end
    on_intel do
      url "https://ipconfig.co.network/pour/dl/v$VERSION/pour-linux-x86_64"
      sha256 "$LX"
    end
  end

  livecheck do
    url "https://ipconfig.co.network/pour/releases/latest.json"
    strategy :json do |json|
      json["version"]
    end
  end

  def install
    downloaded = Dir["pour-*"].first
    chmod 0755, downloaded
    bin.install downloaded => "pour"
    generate_completions_from_executable(bin/"pour", "completions")
  end

  def caveats
    <<~EOS
      Pour keeps its own prefix at /opt/pour. Create it once:
        pour init
      then add to your shell:
        eval "\$(pour shellenv)"
    EOS
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/pour --version")
  end
end
RUBY
echo "  wrote $TAP/Formula/pour.rb ($VERSION)"
