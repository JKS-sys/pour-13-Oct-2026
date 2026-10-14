#!/usr/bin/env bash
# Pour · update.sh — the one script that does everything.
#
#   bash update.sh                 test, build every platform, release, publish, sync registry + apps, deploy the site
#   bash update.sh --no-publish    build and package only (dist/)
#   bash update.sh --version 0.2.0 release as this version (default: bump the patch)
#   bash update.sh --skip-tests    (not recommended)
#   bash update.sh --only build|release|registry|apps|site|details
#
# Steps, in order — each prints what it is doing:
#   doctor  → tools present (cargo, cargo-zigbuild + zig, gh, node, wrangler)
#   version → bump Cargo.toml (patch) unless --version
#   tests   → cargo fmt/clippy/test
#   build   → pour for macOS (arm64, x86_64), Linux (x86_64, arm64, i686 musl),
#             Windows (x86_64, arm64), FreeBSD (x86_64); strip; checksums; dist/latest.json
#   release → git tag, GitHub Release with every binary + install.sh + latest.json
#   mirror  → Cloudflare R2 (served by the Worker at ipconfig.co.network/pour/…)
#   registry→ ../pour-registry/update.sh (index of every Homebrew formula + cask)
#   apps    → ../pour-apps (versions of Jagadeesh's apps)
#   site    → ../pour-site (wrangler deploy)
#   details → description, website and topics on every GitHub repo
#
# Credentials come from a gitignored .env (GH_TOKEN, CLOUDFLARE_API_TOKEN,
# CLOUDFLARE_ACCOUNT_ID) read with scripts/env.sh — never from this file.
# Copyright (c) 2026 Jagadeesh Kumar S. All rights reserved.
set -euo pipefail
cd "$(dirname "$0")"
. scripts/env.sh; load_env .env 2>/dev/null || true
export GH_TOKEN="${GH_TOKEN:-${GITHUB_TOKEN:-}}"

PUBLISH=1; TESTS=1; VERSION=""; ONLY=""
while [ $# -gt 0 ]; do
  case "$1" in
    --no-publish) PUBLISH=0 ;;
    --skip-tests) TESTS=0 ;;
    --version) VERSION="$2"; shift ;;
    --only) ONLY="$2"; shift ;;
    -h|--help) sed -n '2,25p' "$0"; exit 0 ;;
    *) echo "unknown option $1" >&2; exit 2 ;;
  esac
  shift
done
only() { [ -z "$ONLY" ] || [ "$ONLY" = "$1" ]; }
say() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33mWarning:\033[0m %s\n' "$*" >&2; }
die() { printf '\033[1;31mError:\033[0m %s\n' "$*" >&2; exit 1; }
retry() { # retry <tries> <cmd...> — for Cloudflare's occasional 5xx
  local n=$1 i=1 delay=5; shift
  until "$@"; do
    [ $i -ge "$n" ] && return 1
    warn "attempt $i failed; retrying in ${delay}s"; sleep $delay; i=$((i+1)); delay=$((delay*2))
  done
}

OWNER=JKS-sys
REPO=$OWNER/pour-13-Oct-2026   # repositories carry their date suffix
TARGETS=(
  "aarch64-apple-darwin pour-macos-arm64"
  "x86_64-apple-darwin pour-macos-x86_64"
  "x86_64-unknown-linux-musl pour-linux-x86_64"
  "aarch64-unknown-linux-musl pour-linux-arm64"
  "i686-unknown-linux-musl pour-linux-i686"
  "x86_64-pc-windows-gnu pour-windows-x86_64.exe"
  "aarch64-pc-windows-gnullvm pour-windows-arm64.exe"
  "x86_64-unknown-freebsd pour-freebsd-x86_64"
)

# ---------------------------------------------------------------- doctor
say "doctor"
for t in cargo git; do command -v $t >/dev/null || die "$t is missing"; done
if ! command -v cargo-zigbuild >/dev/null; then
  say "installing cargo-zigbuild (one time)"; cargo install cargo-zigbuild
fi
if ! command -v zig >/dev/null && ! python3 -c 'import ziglang' 2>/dev/null; then
  say "installing zig (one time, via pip)"; pip3 install --quiet --break-system-packages ziglang || pip3 install --quiet ziglang
fi
for t in $(printf '%s\n' "${TARGETS[@]}" | cut -d' ' -f1); do rustup target add "$t" >/dev/null 2>&1 || warn "could not add target $t"; done
command -v gh >/dev/null || warn "gh is missing — releases will be skipped"
command -v node >/dev/null || warn "node is missing — site and apps steps will be skipped"

# ---------------------------------------------------------------- version
CUR=$(sed -n 's/^version = "\([0-9.]*\)"/\1/p' Cargo.toml | head -1)
if only build || only release; then
  if [ -n "$VERSION" ]; then NEW="$VERSION"
  elif [ "$PUBLISH" = 1 ] && [ -z "$ONLY" ]; then
    IFS=. read -r a b c <<< "$CUR"; NEW="$a.$b.$((c+1))"
  else NEW="$CUR"; fi
  if [ "$NEW" != "$CUR" ]; then
    say "version $CUR → $NEW"
    sed -i.bak "s/^version = \"$CUR\"/version = \"$NEW\"/" Cargo.toml && rm -f Cargo.toml.bak
    cargo update -q -p pour -p pour-core 2>/dev/null || true
  fi
  VERSION="$NEW"
else
  VERSION="${VERSION:-$CUR}"
fi
echo "$VERSION" > VERSION

# ---------------------------------------------------------------- tests
if [ "$TESTS" = 1 ] && (only build || only release); then
  say "tests"
  cargo fmt --all -- --check 2>/dev/null || { warn "formatting differs; running cargo fmt"; cargo fmt --all; }
  cargo clippy --all-targets -q -- -D warnings 2>/dev/null || warn "clippy had findings (not fatal)"
  cargo test -q 2>&1 | tail -3
fi

# ---------------------------------------------------------------- build
if only build || only release; then
  say "building $VERSION for ${#TARGETS[@]} targets"
  rm -rf dist; mkdir -p dist
  host=$(rustc -vV | sed -n 's/^host: //p')
  for entry in "${TARGETS[@]}"; do
    set -- $entry; target=$1; name=$2
    printf '  %-28s ' "$target"
    if [ "$target" = "$host" ]; then
      cargo build -q --release -p pour --target "$target" 2>/tmp/pour-build.log || { echo "failed"; cat /tmp/pour-build.log | tail -5; continue; }
    else
      cargo zigbuild -q --release -p pour --target "$target" 2>/tmp/pour-build.log || { echo "failed"; tail -5 /tmp/pour-build.log; continue; }
    fi
    bin="target/$target/release/pour"; case "$name" in *.exe) bin="$bin.exe";; esac
    cp "$bin" "dist/$name"
    echo "$(du -h "dist/$name" | cut -f1)"
  done
  cp install.sh dist/install.sh
  (cd dist && for f in pour-*; do
     if command -v sha256sum >/dev/null; then sha256sum "$f"; else shasum -a 256 "$f"; fi
   done > SHA256SUMS)
  notes=$(git log --no-merges --pretty='- %s' "$(git describe --tags --abbrev=0 2>/dev/null || git rev-list --max-parents=0 HEAD)..HEAD" 2>/dev/null | grep -v '^- sync ' | head -30)
  [ -n "$notes" ] || notes="- Maintenance release."
  python3 - "$VERSION" "$notes" <<'PY' > dist/latest.json
import json, sys, os, hashlib, datetime
version, notes = sys.argv[1], sys.argv[2]
assets = {}
for f in sorted(os.listdir("dist")):
    if f.startswith("pour-"):
        assets[f] = {"url": f"https://github.com/JKS-sys/pour-13-Oct-2026/releases/download/v{version}/{f}",
                     "mirror": f"https://ipconfig.co.network/pour/dl/{f}",
                     "sha256": hashlib.sha256(open(os.path.join("dist", f), "rb").read()).hexdigest(),
                     "size": os.path.getsize(os.path.join("dist", f))}
json.dump({"name": "pour", "version": version, "pub_date": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
           "notes": notes, "assets": assets, "install": "curl -fsSL https://ipconfig.co.network/pour/install | sh",
           "owner": "Jagadeesh Kumar S", "homepage": "https://ipconfig.co.network/pour"}, sys.stdout, indent=1)
PY
  say "dist/: $(ls dist | tr '\n' ' ')"
fi

[ "$PUBLISH" = 1 ] || { say "not publishing (--no-publish)"; exit 0; }

# ---------------------------------------------------------------- release
if only release && command -v gh >/dev/null && [ -n "$GH_TOKEN" ]; then
  say "release v$VERSION on GitHub"
  # A fresh zip has no .git: push-github.sh fetches GitHub's newest commit, commits this folder on
  # top of it and pushes a fast-forward (never a rewrite). Then the tag.
  COMMIT_MSG="release $VERSION" retry 3 bash scripts/push-github.sh "${REPO#*/}" || warn "source push failed"
  git tag -f "v$VERSION" >/dev/null
  AUTH="Authorization: Basic $(printf 'x-access-token:%s' "$GH_TOKEN" | base64 | tr -d '\n')"
  retry 3 git -c "http.extraHeader=$AUTH" push -q -f origin "v$VERSION"
  if gh release view "v$VERSION" -R "$REPO" >/dev/null 2>&1; then
    gh release upload "v$VERSION" dist/* -R "$REPO" --clobber
  else
    gh release create "v$VERSION" dist/* -R "$REPO" --title "Pour $VERSION" --notes-file <(python3 -c 'import json;print(json.load(open("dist/latest.json"))["notes"])') --latest
  fi
fi

# ---------------------------------------------------------------- mirror (R2)
if only release && [ -n "${CLOUDFLARE_API_TOKEN:-}" ] && [ -n "${CLOUDFLARE_ACCOUNT_ID:-}" ] && command -v npx >/dev/null; then
  say "mirror to Cloudflare R2"
  for f in dist/*; do
    ct=application/octet-stream
    case "$f" in *.json) ct=application/json;; *.sh) ct=text/plain;; esac
    # pinned copy first (pinned installs point at it), then the "newest" copy
    case "$f" in *.json|*.sh) ;; *) retry 4 npx --yes wrangler@4 r2 object put "pour-registry/releases/v$VERSION/$(basename "$f")" --file "$f" --content-type "$ct" --remote >/dev/null ;; esac
    retry 4 npx --yes wrangler@4 r2 object put "pour-registry/releases/$(basename "$f")" --file "$f" --content-type "$ct" --remote >/dev/null
  done
fi

# ---------------------------------------------------------------- registry, apps, site
if only registry && [ -x ../pour-registry/update.sh ]; then
  say "registry sync"; POUR_BIN="$PWD/target/release/pour" bash ../pour-registry/update.sh || warn "registry sync failed"
fi
if only apps && [ -f ../pour-apps/build.mjs ] && command -v node >/dev/null; then
  say "apps"; (cd ../pour-apps && node build.mjs && git add index.json && (git diff --cached --quiet || git commit -qm "refresh apps") && retry 3 git push -q) || warn "apps step failed"
  if [ -n "${CLOUDFLARE_API_TOKEN:-}" ]; then retry 4 npx --yes wrangler@4 r2 object put pour-registry/apps/index.json --file ../pour-apps/index.json --content-type application/json --remote >/dev/null || true; fi
fi
SITE_DIR=""; for d in ../pour-site-13-Oct-2026 ../pour-site; do [ -f "$d/package.json" ] && { SITE_DIR="$d"; break; }; done
if only site && [ -n "$SITE_DIR" ] && command -v node >/dev/null; then
  # its own deploy.sh deploys the Worker and pushes that folder to its repository
  say "site"; (cd "$SITE_DIR" && if [ -x deploy.sh ]; then bash deploy.sh; else npm ci --silent 2>/dev/null || npm install --silent; retry 4 npm run deploy; fi) || warn "site deploy failed"
fi
if only details; then
  say "README numbers"; python3 scripts/readme-stats.py || warn "could not refresh the README stats"
  if ! git diff --quiet README.md; then git add README.md && git commit -qm "README: numbers from the newest index and benchmark" && retry 3 git push -q 2>/dev/null || warn "README commit not pushed"; fi
fi
if only details && command -v gh >/dev/null && [ -n "$GH_TOKEN" ]; then
  say "GitHub repository details"; bash scripts/repo-details.sh || warn "could not set every repo's details"
fi
say "done: Pour $VERSION"
