#!/usr/bin/env bash
# Push this folder to its GitHub repository as one new commit on top of what is already there:
#   bash scripts/push-github.sh [repo-name]          (default: $GH_REPO, else the name below)
# A fresh zip has no .git — this fetches the repo's newest commit first (depth 1), puts HEAD on it
# with `reset --soft` (the files here stay exactly as they are), commits, and pushes a fast-forward.
# Needs GH_TOKEN (from .env / .env.local, one folder up, or ~/.ipconfig.env). SKIP_PUSH=1 skips.
# The commit is by the owner's git identity only.
set -euo pipefail
cd "$(dirname "$0")/.."
[ "${SKIP_PUSH:-0}" = "1" ] && { echo "· push skipped (SKIP_PUSH=1)"; exit 0; }
load_env() {
  local file="$1" line key val
  [ -f "$file" ] || return 0
  while IFS= read -r line || [ -n "$line" ]; do
    line="${line%$'\r'}"
    [[ "$line" =~ ^[[:space:]]*# ]] && continue
    [[ "$line" =~ ^[[:space:]]*(export[[:space:]]+)?([A-Za-z_][A-Za-z0-9_]*)=(.*)$ ]] || continue
    key="${BASH_REMATCH[2]}"; val="${BASH_REMATCH[3]}"
    if [[ "$val" =~ ^\"(.*)\"$ ]] || [[ "$val" =~ ^\'(.*)\'$ ]]; then val="${BASH_REMATCH[1]}"; fi
    [ -z "${!key:-}" ] && export "$key=$val"
  done < "$file"
}
for f in ./.env ./.env.local ../.env.local ../.env "$HOME/.ipconfig.env"; do load_env "$f"; done
GH_TOKEN="${GH_TOKEN:-${GITHUB_TOKEN:-}}"
[ -n "$GH_TOKEN" ] || { echo "✗ GH_TOKEN is not set (.env / .env.local) — nothing pushed"; exit 1; }
OWNER="${GH_OWNER:-JKS-sys}"
NAME="${1:-${GH_REPO:-pour-13-Oct-2026}}"
AUTH="Authorization: Basic $(printf 'x-access-token:%s' "$GH_TOKEN" | base64 | tr -d '\n')"
git init -q -b main 2>/dev/null || git init -q
git symbolic-ref HEAD refs/heads/main 2>/dev/null || true
git config user.name  "${GIT_AUTHOR_NAME:-Jagadeesh Kumar S}"
git config user.email "${GIT_AUTHOR_EMAIL:-JKS.sys@icloud.com}"
git remote remove origin 2>/dev/null || true
git remote add origin "${GIT_REMOTE_URL:-https://github.com/$OWNER/$NAME.git}"
for x in .env .env.local .dev.vars node_modules .wrangler target dist; do grep -qx "$x/\?" .gitignore 2>/dev/null || printf '%s\n' "$x" >> .gitignore; done
if git -c "http.extraHeader=$AUTH" fetch -q --depth=1 origin main 2>/dev/null; then
  git reset -q --soft FETCH_HEAD && echo "· on top of GitHub's $(git rev-parse --short FETCH_HEAD)"
else
  echo "· GitHub has no main branch yet — this is the first commit"
fi
git add -A
MSG="${COMMIT_MSG:-$(basename "$NAME") — $(date '+%d %b %Y %H:%M')}"
if git diff --cached --quiet; then echo "· nothing new to commit"; else git commit -q -m "$MSG" && echo "· committed $(git rev-parse --short HEAD)"; fi
push() { git -c "http.extraHeader=$AUTH" push -q -u origin main 2>&1 | tail -2; return "${PIPESTATUS[0]}"; }
if ! push; then
  git -c "http.extraHeader=$AUTH" fetch -q --depth=1 origin main && git reset -q --soft FETCH_HEAD && git commit -q -m "$MSG" && push || { echo "✗ push failed"; exit 1; }
fi
echo "✓ https://github.com/$OWNER/$NAME"
