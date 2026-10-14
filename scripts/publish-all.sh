#!/usr/bin/env bash
# publish-all.sh — put every Pour repository on GitHub in one go.
#
#   bash pour/scripts/publish-all.sh            from the folder that holds
#                                               pour/ pour-registry/ pour-apps/
#                                               pour-site/ pour-action/ pour-bench/
#
# What it does (each step is skipped when already done):
#   1. creates the six public repositories under JKS-sys, named <dir>-13-Oct-2026,
#      and pushes `main` of each as ONE commit authored by you (git config --global)
#      — no other contributor ever appears
#   2. publishes release v<VERSION> of Pour with every binary, install.sh and
#      latest.json — from pour/dist/ when it exists, otherwise fetched from
#      the mirror at ipconfig.co.network and checked against its SHA-256s
#   3. publishes the rolling `latest` release of pour-registry (the index)
#   4. sets the Cloudflare secrets the sync and release workflows need
#   5. sets description, website and topics on every repository
#   6. adds Formula/pour.rb to homebrew-tap when ../homebrew-tap is a clone
#   7. starts the first registry and apps sync runs
#
# Needs: git, gh, curl, python3. Credentials come from pour/.env (gitignored):
# GH_TOKEN (repo + workflow scopes), CLOUDFLARE_API_TOKEN, CLOUDFLARE_ACCOUNT_ID.
#
# Copyright (c) 2026 Jagadeesh Kumar S. All rights reserved.
set -uo pipefail
say() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
ok() { printf '  \033[32mOK\033[0m %s\n' "$*"; }
warn() { printf '  \033[33m!\033[0m %s\n' "$*"; }
die() { printf '\033[1;31mError:\033[0m %s\n' "$*" >&2; exit 1; }

# Where are the six folders? Next to this script, in the current folder, or
# one level up (the script may live in pour/scripts/). ROOT=... overrides.
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=""
for c in "${ROOT_DIR:-}" "$HERE" "$PWD" "$HERE/.." "$HERE/../.." "$HOME/Downloads"; do
  [ -n "$c" ] && [ -d "$c/pour" ] && [ -d "$c/pour-registry" ] && { ROOT=$(cd "$c" && pwd); break; }
done
[ -n "$ROOT" ] || die "cannot find the unzipped folders (pour/, pour-registry/, ...). Put this script next to them, or run: ROOT_DIR=/path/to/folder bash publish-all.sh"
cd "$ROOT"
OWNER=JKS-sys
SUFFIX=-13-Oct-2026                      # every repository is <dir>$SUFFIX
REPOS=(pour pour-registry pour-apps pour-site pour-action pour-bench)
MIRROR=https://ipconfig.co.network/pour
for t in git curl python3; do command -v $t >/dev/null || die "need $t"; done
command -v gh >/dev/null || die "need the GitHub CLI: brew install gh"
GIT_NAME=$(git config --global user.name || true); GIT_MAIL=$(git config --global user.email || true)
[ -n "$GIT_NAME" ] && [ -n "$GIT_MAIL" ] || die "set your git identity first: git config --global user.name 'Your Name'; git config --global user.email you@example.com"
case "$GIT_NAME$GIT_MAIL" in *[Cc]laude*|*[Aa]nthropic*) die "git identity must be yours, not an assistant's";; esac

# .env is read as data (KEY=VALUE lines), never sourced. Looked for next to
# this script, in pour/, and in the current folder.
load_env() {
  local file="$1" line key val
  [ -f "$file" ] || return 0
  while IFS= read -r line || [ -n "$line" ]; do
    line="${line%$'\r'}"
    [[ "$line" =~ ^[[:space:]]*# ]] && continue
    [[ "$line" =~ ^[[:space:]]*(export[[:space:]]+)?([A-Za-z_][A-Za-z0-9_]*)=(.*)$ ]] || continue
    key="${BASH_REMATCH[2]}"; val="${BASH_REMATCH[3]}"
    if [[ "$val" =~ ^\"(.*)\"$ ]] || [[ "$val" =~ ^\'(.*)\'$ ]]; then val="${BASH_REMATCH[1]}"; fi
    export "$key=$val"
  done < "$file"
}
for f in "$HERE/.env" "$HERE/ship.env" "$ROOT/.env" "$ROOT/ship.env" "$ROOT/pour/.env" "$PWD/.env"; do load_env "$f"; done
export GH_TOKEN="${GH_TOKEN:-${GITHUB_TOKEN:-}}"
[ -n "$GH_TOKEN" ] || die "GH_TOKEN missing: copy your ship.env next to this script as .env (or into pour/.env)"
gh auth status >/dev/null 2>&1 || warn "gh auth status failed; continuing with GH_TOKEN"
VERSION=$(tr -d '[:space:]' < pour/VERSION)
for r in "${REPOS[@]}"; do [ -d "$r" ] || die "$r/ is missing next to pour/ (unzip $r.zip into a folder named $r)"; done

# 1. Repositories — each one gets a fresh history with a single commit by you
say "repositories"
for r in "${REPOS[@]}"; do
  name="$r$SUFFIX"
  if gh api "repos/$OWNER/$name" >/dev/null 2>&1; then
    ok "$OWNER/$name exists - left as it is (set REPUSH=1 to replace its history with this folder)"
    [ "${REPUSH:-0}" = 1 ] || continue
  else
    gh repo create "$OWNER/$name" --public >/dev/null 2>&1 && ok "created $OWNER/$name" || die "could not create $OWNER/$name (token needs the repo scope)"
  fi
  (
    cd "$r"
    rm -rf .git
    git init -q -b main
    git add -A
    git -c user.name="$GIT_NAME" -c user.email="$GIT_MAIL" commit -q -m "$r: initial release" --author="$GIT_NAME <$GIT_MAIL>"
    git remote add origin "https://github.com/$OWNER/$name.git"
    if git push -q -f -u origin main 2>/dev/null; then ok "$r: pushed main (1 commit, by $GIT_NAME)"
    else warn "$r: push failed - check GH_TOKEN scopes"; fi
  )
done

# `uses: JKS-sys/pour-action-13-Oct-2026@v1` needs the v1 tag to follow main
(cd pour-action && git tag -f v1 >/dev/null 2>&1 && git push -q -f origin v1 2>/dev/null && ok "pour-action: tag v1 -> main" || warn "pour-action: could not push the v1 tag")
(cd pour && git tag -f "v$VERSION" >/dev/null 2>&1 && git push -q -f origin "v$VERSION" 2>/dev/null || true)

# 2. Pour release
say "release v$VERSION of pour"
if gh release view "v$VERSION" -R "$OWNER/pour$SUFFIX" >/dev/null 2>&1 && [ "$(gh release view "v$VERSION" -R "$OWNER/pour$SUFFIX" --json assets --jq '.assets|length')" -ge 8 ]; then ok "release v$VERSION already has its files"
else
  gh release delete "v$VERSION" -R "$OWNER/pour$SUFFIX" -y >/dev/null 2>&1 || true
  tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
  if [ -f pour/dist/latest.json ] && [ -f pour/dist/SHA256SUMS ]; then
    cp pour/dist/* "$tmp/"; ok "using pour/dist/"
  else
    say "fetching the release files from the mirror"
    UA="pour-publish/$VERSION"
    curl -fsSL -A "$UA" "$MIRROR/releases/latest.json" -o "$tmp/latest.json" || die "mirror has no latest.json"
    curl -fsSL -A "$UA" "$MIRROR/dl/v$VERSION/SHA256SUMS" -o "$tmp/SHA256SUMS" || curl -fsSL -A "$UA" "$MIRROR/dl/SHA256SUMS" -o "$tmp/SHA256SUMS" || die "mirror has no SHA256SUMS"
    while read -r sum name; do
      [ -n "$name" ] || continue
      curl -fsSL -A "$UA" "$MIRROR/dl/v$VERSION/$name" -o "$tmp/$name" || curl -fsSL -A "$UA" "$MIRROR/dl/$name" -o "$tmp/$name" || die "could not download $name"
      got=$(shasum -a 256 "$tmp/$name" 2>/dev/null | cut -d' ' -f1); [ -n "$got" ] || got=$(sha256sum "$tmp/$name" | cut -d' ' -f1)
      [ "$got" = "$sum" ] || die "$name did not match its SHA-256"
      ok "$name $(du -k "$tmp/$name" | cut -f1) KB, checksum OK"
    done < "$tmp/SHA256SUMS"
    cp pour/install.sh "$tmp/install.sh"
  fi
  notes=$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1])).get("notes","Pour "+sys.argv[2]))' "$tmp/latest.json" "$VERSION")
  gh release create "v$VERSION" "$tmp"/* -R "$OWNER/pour$SUFFIX" --title "Pour $VERSION" --notes "$notes" --latest >/dev/null && ok "release v$VERSION published" || die "release failed"
fi

# 3. Registry release
say "registry release"
if ! gh release view latest -R "$OWNER/pour-registry$SUFFIX" >/dev/null 2>&1; then
  tmp2=$(mktemp -d)
  for f in index.pour.gz index.json search.json changes.json; do
    if [ -f "pour-registry/dist/$f" ]; then cp "pour-registry/dist/$f" "$tmp2/"; else curl -fsSL "$MIRROR/registry/$f" -o "$tmp2/$f" || true; fi
  done
  gh release create latest "$tmp2"/* -R "$OWNER/pour-registry$SUFFIX" --title "Pour registry — rolling" --notes "Rebuilt every four hours from Homebrew's API. Always the newest index." --latest >/dev/null && ok "registry release published" || warn "registry release failed (the sync workflow will create it)"
  rm -rf "$tmp2"
else ok "registry release exists"; fi

# 4. Secrets
say "workflow secrets"
if [ -n "${CLOUDFLARE_API_TOKEN:-}" ] && [ -n "${CLOUDFLARE_ACCOUNT_ID:-}" ]; then
  for r in pour pour-registry pour-apps; do
    printf '%s' "$CLOUDFLARE_API_TOKEN" | gh secret set CLOUDFLARE_API_TOKEN -R "$OWNER/$r$SUFFIX" >/dev/null 2>&1 &&
    printf '%s' "$CLOUDFLARE_ACCOUNT_ID" | gh secret set CLOUDFLARE_ACCOUNT_ID -R "$OWNER/$r$SUFFIX" >/dev/null 2>&1 && ok "$r: Cloudflare secrets set" || warn "$r: could not set secrets"
  done
else warn "CLOUDFLARE_API_TOKEN / CLOUDFLARE_ACCOUNT_ID not in .env - the sync workflows will publish to GitHub only"; fi

# 5. Details
say "descriptions, websites, topics"
bash pour/scripts/repo-details.sh || true

# 6. Tap
if [ -d homebrew-tap/.git ] || [ -d ../homebrew-tap/.git ]; then
  tap=homebrew-tap; [ -d "$tap/.git" ] || tap=../homebrew-tap
  say "homebrew-tap formula"
  bash pour/scripts/tap-formula.sh "$VERSION" "$tap" && (cd "$tap" && git add Formula/pour.rb && git -c user.name="Jagadeesh Kumar S" -c user.email="${GIT_EMAIL:-jagadeesh_2k17@proton.me}" commit -q -m "pour $VERSION" 2>/dev/null; git push -q 2>/dev/null && ok "homebrew-tap: Formula/pour.rb pushed" || warn "homebrew-tap: commit or push failed")
fi

# 7. First syncs
say "first sync runs"
gh workflow run sync.yml -R "$OWNER/pour-registry$SUFFIX" >/dev/null 2>&1 && ok "pour-registry sync started" || warn "could not start the registry sync (Actions may still be initialising; it runs every 4 h anyway)"
gh workflow run sync.yml -R "$OWNER/pour-apps$SUFFIX" >/dev/null 2>&1 && ok "pour-apps sync started" || true

printf '\n\033[1mDone.\033[0m https://github.com/%s/pour%s and https://ipconfig.co.network/pour\n' "$OWNER" "$SUFFIX"
