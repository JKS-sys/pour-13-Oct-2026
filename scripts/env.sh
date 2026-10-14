# shellcheck shell=bash
# Read .env as data, never as shell code.
#
#   . scripts/env.sh && load_env
#
# `. ./.env` runs every line as a command, so a value with a space in it
# (MAC_SIGN_IDENTITY=JKS-sys App Self-Signed) ran "App" and stopped
# update.sh. This reads KEY=VALUE lines, strips one pair of surrounding
# quotes, and exports them — whatever the value contains. Works in macOS's
# bash 3.2.
load_env() {
  local file="${1:-.env}" line key val
  [ -f "$file" ] || return 0
  while IFS= read -r line || [ -n "$line" ]; do
    line="${line%$'\r'}"
    [[ "$line" =~ ^[[:space:]]*# ]] && continue
    [[ "$line" =~ ^[[:space:]]*(export[[:space:]]+)?([A-Za-z_][A-Za-z0-9_]*)=(.*)$ ]] || continue
    key="${BASH_REMATCH[2]}"
    val="${BASH_REMATCH[3]}"
    if [[ "$val" =~ ^\"(.*)\"$ ]] || [[ "$val" =~ ^\'(.*)\'$ ]]; then val="${BASH_REMATCH[1]}"; fi
    export "$key=$val"
  done < "$file"
  # Plan ids may be named by price (RZP_20_MONTHLY, RZP_220_YEARLY) or by
  # period (RZP_PLAN_MONTHLY, RZP_PLAN_YEARLY). Either name fills the other.
  : "${RZP_PLAN_MONTHLY:=${RZP_20_MONTHLY:-}}" "${RZP_PLAN_YEARLY:=${RZP_220_YEARLY:-}}"
  : "${RZP_20_MONTHLY:=${RZP_PLAN_MONTHLY:-}}" "${RZP_220_YEARLY:=${RZP_PLAN_YEARLY:-}}"
  export RZP_PLAN_MONTHLY RZP_PLAN_YEARLY RZP_20_MONTHLY RZP_220_YEARLY
}

# Write KEY="VALUE" once (quoted, so even a plain `source` would be safe).
keep_env() {
  local file="${3:-.env}"
  grep -q "^$1=" "$file" 2>/dev/null && return 0
  # A file whose last line has no newline would swallow the new key.
  if [ -s "$file" ] && [ -n "$(tail -c1 "$file")" ]; then printf '\n' >> "$file"; fi
  printf '%s="%s"\n' "$1" "$2" >> "$file"
}

# Write KEY="VALUE", replacing an existing (possibly empty) line.
set_env() {
  local file="${3:-.env}" tmp
  tmp=$(mktemp)
  if [ -f "$file" ]; then grep -v "^$1=" "$file" > "$tmp" || true; fi
  if [ -s "$tmp" ] && [ -n "$(tail -c1 "$tmp")" ]; then printf '\n' >> "$tmp"; fi
  printf '%s="%s"\n' "$1" "$2" >> "$tmp"
  cat "$tmp" > "$file"; rm -f "$tmp"
}
