# shellcheck shell=bash
# Keys come from the ipconfig dashboard vault — never from a .env file in the repository.
#
#   . scripts/env.sh && load_vault pour
#
# Opens the vault with your GitHub sign-in (gh auth login as JKS-sys), else this Mac's approved
# read-only token in the keychain (curl -fsSL https://ipconfig.co.network/vault/login.sh | bash).
# The export is read as data (KEY=VALUE lines, never `source`d) from a 0600 temp file that is
# deleted at once. Keys already in the environment win, so CI secrets still work.
VAULT_URL="${VAULT_URL:-https://ipconfig.co.network/api/v1/owner/vault}"

_read_kv() { # file → export KEY=VALUE lines not already set
  local line key val
  while IFS= read -r line || [ -n "$line" ]; do
    line="${line%$'\r'}"
    [[ "$line" =~ ^[[:space:]]*# ]] && continue
    [[ "$line" =~ ^[[:space:]]*(export[[:space:]]+)?([A-Za-z_][A-Za-z0-9_]*)=(.*)$ ]] || continue
    key="${BASH_REMATCH[2]}"; val="${BASH_REMATCH[3]}"
    if [[ "$val" =~ ^\"(.*)\"$ ]] || [[ "$val" =~ ^\'(.*)\'$ ]]; then val="${BASH_REMATCH[1]}"; fi
    [ -n "${!key:-}" ] || export "$key=$val"
  done < "$1"
}

vault_token() {
  local t
  t="$(gh auth token 2>/dev/null || true)"
  [ -z "$t" ] && [ "$(uname)" = Darwin ] && t="$(security find-generic-password -s ipconfig-vault -w 2>/dev/null || true)"
  printf '%s' "$t"
}

load_vault() { # app name (default pour); returns 1 when the vault could not be read
  local app="${1:-pour}" tok tmp rc=1
  # keys already here (CI secrets, an exported shell): nothing to fetch
  if [ -n "${GH_TOKEN:-}" ] && [ -n "${CLOUDFLARE_API_TOKEN:-}" ] && [ -n "${CLOUDFLARE_ACCOUNT_ID:-}" ]; then return 0; fi
  tok="$(vault_token)"
  if [ -z "$tok" ]; then
    echo "Warning: no way into the vault — run: gh auth login   (or approve this Mac: curl -fsSL https://ipconfig.co.network/vault/login.sh | bash)" >&2
    return 1
  fi
  tmp="$(mktemp "${TMPDIR:-/tmp}/vault.XXXXXX")"; chmod 600 "$tmp"
  if curl -fsS -H "Authorization: Bearer $tok" "$VAULT_URL/$app/export?format=env&shared=1" -o "$tmp"; then
    _read_kv "$tmp"; rc=0
    echo "==> keys from the dashboard vault ($app + shared)"
  else
    echo "Warning: the dashboard vault did not answer for $app" >&2
  fi
  rm -f "$tmp"
  return $rc
}

# Plan ids may be named by price or by period; either name fills the other.
rzp_aliases() {
  : "${RZP_PLAN_MONTHLY:=${RZP_20_MONTHLY:-}}" "${RZP_PLAN_YEARLY:=${RZP_220_YEARLY:-}}"
  : "${RZP_20_MONTHLY:=${RZP_PLAN_MONTHLY:-}}" "${RZP_220_YEARLY:=${RZP_PLAN_YEARLY:-}}"
  export RZP_PLAN_MONTHLY RZP_PLAN_YEARLY RZP_20_MONTHLY RZP_220_YEARLY
}
