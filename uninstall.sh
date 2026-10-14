#!/bin/sh
# Pour uninstaller — https://ipconfig.co.network/pour
#
#   curl -fsSL https://ipconfig.co.network/pour/uninstall | sh            asks before removing
#   curl -fsSL https://ipconfig.co.network/pour/uninstall | sh -s -- -y   no questions
#
# What it removes, in order (it shows each step):
#   1. everything Pour installed under its prefix (/opt/pour): the packages, the store,
#      the index, the `pour` program itself
#   2. Pour's download cache, if POUR_CACHE put it outside the prefix
#   3. the PATH lines the installer added to your shell start-up files
#   4. the `pour` link the installer put in a folder on your PATH
# Apps Pour placed in /Applications stay where they are (they are ordinary apps now);
# run `pour uninstall --cask <app>` first for any you want gone.
#
# Copyright (c) 2026 Jagadeesh Kumar S. All rights reserved.
set -u

say() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
die() { printf '\033[1;31mError:\033[0m %s\n' "$*" >&2; exit 1; }

YES=0
for a in "$@"; do case "$a" in -y|--yes) YES=1 ;; esac; done
[ -n "${POUR_YES:-}" ] && YES=1

os=$(uname -s 2>/dev/null || echo unknown)
PREFIX="${POUR_PREFIX:-}"
if [ -z "$PREFIX" ]; then
  if [ -x /opt/pour/bin/pour ] || [ -d /opt/pour ]; then PREFIX=/opt/pour
  elif [ -d "$HOME/.pour" ]; then PREFIX="$HOME/.pour"
  elif [ -d "$HOME/config/non-packaged/pour" ]; then PREFIX="$HOME/config/non-packaged/pour"
  else PREFIX=/opt/pour; fi
fi
case "$PREFIX" in ""|/|/usr|/usr/local|/opt|/opt/homebrew|"$HOME") die "refusing to remove $PREFIX" ;; esac

# the download cache lives in the prefix unless POUR_CACHE moved it
CACHE="${POUR_CACHE:-}"

size() { du -sh "$1" 2>/dev/null | cut -f1; }
say "Pour uninstaller"
if [ -d "$PREFIX" ]; then
  n=$(ls "$PREFIX/Cellar" 2>/dev/null | wc -l | tr -d ' ')
  say "prefix $PREFIX ($(size "$PREFIX"), $n packages)"
else
  say "no prefix at $PREFIX"
fi
[ -d "$CACHE" ] && say "download cache $CACHE ($(size "$CACHE"))"
apps=$(ls "$PREFIX/Caskroom" 2>/dev/null | tr '\n' ' ')
[ -n "$apps" ] && say "apps placed by Pour stay installed: $apps(remove them first with: pour uninstall --cask <app>)"

if [ "$YES" != 1 ]; then
  if [ -r /dev/tty ]; then
    printf 'Remove Pour and everything above? [y/N] '
    read -r answer < /dev/tty || answer=""
    case "$answer" in y|Y|yes|YES) ;; *) say "nothing removed (add -s -- -y after sh to skip the question)"; exit 0 ;; esac
  else
    die "no terminal to ask — run with: sh -s -- -y"
  fi
fi

# 1. prefix
if [ -d "$PREFIX" ]; then
  if rm -rf "$PREFIX" 2>/dev/null && [ ! -e "$PREFIX" ]; then :
  else
    command -v sudo >/dev/null 2>&1 || die "$PREFIX needs administrator rights to remove and sudo is missing"
    say "removing $PREFIX (your password, once)"
    sudo -p "Password: " rm -rf "$PREFIX" || die "could not remove $PREFIX"
  fi
  say "removed $PREFIX"
fi

# 2. cache
[ -d "$CACHE" ] && rm -rf "$CACHE" && say "removed $CACHE"

# 3. shell start-up lines (both the current "# Pour PATH" block and the older "# Pour — " one)
for rc in "$HOME/.zshenv" "$HOME/.zshrc" "$HOME/.zprofile" "$HOME/.bashrc" "$HOME/.bash_profile" "$HOME/.profile" "$HOME/.config/fish/config.fish"; do
  [ -f "$rc" ] || continue
  grep -qs '# Pour PATH\|# Pour — https://ipconfig.co.network/pour\|pour shellenv' "$rc" || continue
  tmp="$rc.pour-uninstall.$$"
  awk '
    /^# Pour PATH/ || /^# Pour — https:\/\/ipconfig\.co\.network\/pour/ { skip = 1; next }
    skip == 1 { skip = 0; next }
    /pour shellenv/ { next }
    { print }
  ' "$rc" > "$tmp" && cat "$tmp" > "$rc" && rm -f "$tmp"
  say "cleaned $rc"
done

# 4. links to the old prefix in folders on PATH
oldifs=$IFS; IFS=:
for d in $PATH "$HOME/.local/bin" "$HOME/bin" /usr/local/bin; do
  l="$d/pour"
  if [ -L "$l" ]; then
    case "$(readlink "$l")" in "$PREFIX"/*) rm -f "$l" 2>/dev/null && say "removed link $l" ;; esac
  fi
done
IFS=$oldifs

printf '\n\033[1mPour is gone.\033[0m Open a new terminal (or run: hash -r). To come back:\n  curl -fsSL https://ipconfig.co.network/pour/install | sh\n\n'
