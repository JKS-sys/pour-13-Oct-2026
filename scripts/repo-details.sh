#!/usr/bin/env bash
# Description, website and topics on every Pour repository (GitHub → About).
# REST only (gh api), so it works wherever GraphQL is blocked.
set -uo pipefail
cd "$(dirname "$0")/.."
. scripts/env.sh; load_env .env 2>/dev/null || true
export GH_TOKEN="${GH_TOKEN:-${GITHUB_TOKEN:-}}"
HOME_URL=https://ipconfig.co.network/pour
set_details() { # repo "description" "topics,comma,separated"
  local repo=$1 desc=$2 topics=$3 names
  names=$(printf '%s' "$topics" | tr 'A-Z' 'a-z' | tr ',' '\n' | sed 's/[^a-z0-9-]/-/g; /^$/d' | head -20 | awk '{printf "%s\"%s\"", (NR>1?",":""), $0}')
  if printf '{"description":%s,"homepage":"%s"}' "$(python3 -c 'import json,sys;print(json.dumps(sys.argv[1]))' "$desc")" "$HOME_URL" | gh api -X PATCH "repos/$repo" --input - >/dev/null 2>&1 &&
     printf '{"names":[%s]}' "$names" | gh api -X PUT "repos/$repo/topics" --input - >/dev/null 2>&1; then
    echo "  $repo: details set"
  else
    echo "  $repo: could not set details (token needs repo scope / admin)"; return 1
  fi
}
rc=0
set_details JKS-sys/pour-13-Oct-2026 "Pour — the Homebrew-compatible package manager that installs in milliseconds. Every Homebrew formula and cask, poured into /opt/pour by a 3 MB Rust binary. macOS, Linux, WSL, ChromeOS, Windows, FreeBSD, Haiku, iOS (iSH). By Jagadeesh Kumar S." "package-manager,homebrew,rust,macos,linux,windows,freebsd,haiku,ios,cli,bottles,casks,fast,developer-tools,devops,command-line" || rc=1
set_details JKS-sys/pour-registry-13-Oct-2026 "The Pour registry: every Homebrew formula and cask (16,000+), re-indexed every four hours, with a log of what was registered and de-registered. By Jagadeesh Kumar S." "package-manager,homebrew,registry,index,automation,github-actions,pour" || rc=1
set_details JKS-sys/pour-apps-13-Oct-2026 "Jagadeesh Kumar S's apps — Peek, Voxo, Nib, Keel, Swep, Grab, Dial, Mend, Twig, Cine, Veil and more — registered for Pour on macOS, Windows, Linux and FreeBSD." "package-manager,apps,macos,windows,linux,tauri,pour,registry" || rc=1
set_details JKS-sys/pour-site-13-Oct-2026 "The Pour website at ipconfig.co.network/pour — install page, package browser, downloads and registry mirror, served by a Cloudflare Worker. By Jagadeesh Kumar S." "website,cloudflare-workers,package-manager,pour,homebrew" || rc=1
set_details JKS-sys/pour-action-13-Oct-2026 "GitHub Action that installs Pour and your packages in CI in seconds — uses: JKS-sys/pour-action-13-Oct-2026. By Jagadeesh Kumar S." "github-action,ci,package-manager,pour,homebrew,actions" || rc=1
set_details JKS-sys/pour-bench-13-Oct-2026 "Reproducible benchmarks: Pour against Homebrew on the same machine, cold and warm, with the scripts and raw timings. By Jagadeesh Kumar S." "benchmark,performance,package-manager,homebrew,pour" || rc=1
exit $rc
