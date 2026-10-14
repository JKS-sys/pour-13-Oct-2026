#!/bin/sh
# Pour installer — https://ipconfig.co.network/pour
#
#   curl -fsSL https://ipconfig.co.network/pour/install | sh
#
# What it does, in order (it tells you before each step):
#   1. works out your system and picks the matching `pour` build
#   2. downloads it from ipconfig.co.network (fallback: GitHub Releases) and
#      checks its SHA-256 against the release feed
#   3. puts it in /opt/pour/bin (asks for your password once if needed)
#   4. fetches the index of every Homebrew formula and cask
#   5. prints the one line to add to your shell
#
# Copyright (c) 2026 Jagadeesh Kumar S. All rights reserved.
set -eu

FEED="${POUR_RELEASE_FEED:-https://ipconfig.co.network/pour/releases/latest.json}"
GH="https://github.com/JKS-sys/pour-13-Oct-2026/releases/latest/download"
PREFIX="${POUR_PREFIX:-}"

say() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
die() { printf '\033[1;31mError:\033[0m %s\n' "$*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

fetch() { # fetch URL DEST [quiet]
  if have curl; then curl -f${3:-sS}L --retry 3 -o "$2" "$1"
  elif have wget; then wget -q -O "$2" "$1"
  else die "need curl or wget"; fi
}

# 1. System
os=$(uname -s 2>/dev/null || echo unknown); arch=$(uname -m 2>/dev/null || echo unknown)
case "$os" in
  Darwin) os=macos ;;
  Linux)  os=linux ;;
  FreeBSD) os=freebsd ;;
  Haiku)  os=haiku ;;
  MINGW*|MSYS*|CYGWIN*) os=windows ;;
  *) die "Pour has no build for $os yet. Build from source: cargo install --git https://github.com/JKS-sys/pour-13-Oct-2026 pour" ;;
esac
case "$arch" in
  arm64|aarch64) arch=arm64 ;;
  x86_64|amd64)  arch=x86_64 ;;
  i386|i686)     arch=i686 ;;
  *) die "Pour has no build for $arch yet." ;;
esac
if [ "$os" = linux ] && [ -f /proc/ish/version ]; then
  say "iSH on iOS detected"
fi
asset="pour-$os-$arch"; [ "$os" = windows ] && asset="$asset.exe"
if [ -z "$PREFIX" ]; then
  case "$os" in
    haiku) PREFIX="$HOME/config/non-packaged/pour" ;;
    *) if [ -f /proc/ish/version ]; then PREFIX="$HOME/.pour"; else PREFIX=/opt/pour; fi ;;
  esac
fi
say "Pour for $os ($arch) → $PREFIX"

# 2. Download + verify
tmp=$(mktemp -d 2>/dev/null || mktemp -d -t pour)
trap 'rm -rf "$tmp"' EXIT
url="https://ipconfig.co.network/pour/dl/$asset"; alt="$GH/$asset"; sha=""
if fetch "$FEED" "$tmp/latest.json" 2>/dev/null; then
  # the feed is pretty-printed JSON: flatten it so one sed sees the whole asset object
  feed=$(tr -d '\n\r' < "$tmp/latest.json")
  m=$(printf '%s' "$feed" | sed -n "s/.*\"$asset\": *{[^}]*\"mirror\": *\"\([^\"]*\)\".*/\1/p")
  g=$(printf '%s' "$feed" | sed -n "s/.*\"$asset\": *{[^}]*\"url\": *\"\([^\"]*\)\".*/\1/p")
  sha=$(printf '%s' "$feed" | sed -n "s/.*\"$asset\": *{[^}]*\"sha256\": *\"\([0-9a-f]*\)\".*/\1/p")
  [ -z "$m" ] || url="$m"; [ -z "$g" ] || alt="$g"
fi
say "downloading $url"
fetch "$url" "$tmp/pour" s || { say "mirror did not answer, trying $alt"; fetch "$alt" "$tmp/pour"; }
digest() {
  if have sha256sum; then sha256sum "$1" | cut -d' ' -f1
  elif have shasum; then shasum -a 256 "$1" | cut -d' ' -f1
  elif have openssl; then openssl dgst -sha256 "$1" | sed 's/.*= *//'; fi
}
if [ -n "$sha" ]; then
  got=$(digest "$tmp/pour")
  if [ -n "$got" ] && [ "$got" != "$sha" ]; then
    # A CDN can hold the previous binary for a few minutes after a release
    # while the feed is already new: fetch past the cache once, then check again.
    say "download did not match the feed, fetching a fresh copy"
    fetch "$url?v=$(printf %.12s "$sha")" "$tmp/pour" s || fetch "$alt" "$tmp/pour"
    got=$(digest "$tmp/pour")
    [ -z "$got" ] || [ "$got" = "$sha" ] || die "checksum mismatch (expected $sha, got $got) — not installing"
  fi
  [ -n "$got" ] && say "checksum verified"
fi
chmod +x "$tmp/pour"

# 3. Prefix
need_sudo=0
if [ ! -d "$PREFIX" ]; then
  mkdir -p "$PREFIX" 2>/dev/null || need_sudo=1
elif [ ! -w "$PREFIX" ]; then need_sudo=1; fi
if [ "$need_sudo" = 1 ]; then
  have sudo || die "$PREFIX is not writable and sudo is missing. Run as root, or set POUR_PREFIX to a folder you own (keep it short: 13 characters or fewer on macOS)."
  say "creating $PREFIX (your password, once)"
  sudo -p "Password: " sh -c "mkdir -p '$PREFIX' && chown -R '$(id -un)' '$PREFIX'"
fi
mkdir -p "$PREFIX/bin"
mv "$tmp/pour" "$PREFIX/bin/pour"
if [ "$os" = macos ]; then
  xattr -dr com.apple.quarantine "$PREFIX/bin/pour" 2>/dev/null || true
  codesign -s - -f "$PREFIX/bin/pour" >/dev/null 2>&1 || true
fi
say "installed $("$PREFIX/bin/pour" --version)"

# 4. Index
POUR_PREFIX="$PREFIX" "$PREFIX/bin/pour" update || say "could not fetch the index now — run \`pour update\` later"

# 5. Shell
shell_name=$(basename "${SHELL:-sh}")
case "$shell_name" in
  zsh)  rc="$HOME/.zprofile"; line="eval \"\$($PREFIX/bin/pour shellenv)\"" ;;
  bash) if [ "$os" = macos ]; then rc="$HOME/.bash_profile"; else rc="$HOME/.bashrc"; fi; line="eval \"\$($PREFIX/bin/pour shellenv)\"" ;;
  fish) rc="$HOME/.config/fish/config.fish"; line="$PREFIX/bin/pour shellenv fish | source" ;;
  *)    rc="$HOME/.profile"; line="eval \"\$($PREFIX/bin/pour shellenv)\"" ;;
esac
if [ -n "${POUR_NO_MODIFY_PATH:-}" ]; then
  say "add this to $rc:  $line"
elif grep -qs 'pour shellenv' "$rc" 2>/dev/null; then
  say "$rc already loads Pour"
else
  mkdir -p "$(dirname "$rc")"
  printf '\n# Pour — https://ipconfig.co.network/pour\n%s\n' "$line" >> "$rc"
  say "added Pour to $rc"
fi
printf '\n\033[1mDone.\033[0m Open a new terminal (or run: %s) and try:\n\n  pour install jq\n  pour search music\n  pour install --cask peek\n\n' "$line"
