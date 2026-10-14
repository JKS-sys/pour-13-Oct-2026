#!/usr/bin/env python3
"""Refresh the numbers at the top of README.md between <!-- stats:start --> and
<!-- stats:end --> from the registry index and the newest pour-bench run.

    python3 scripts/readme-stats.py [../pour-registry/dist/index.json] [../pour-bench/results]

Nothing is typed in by hand: counts come from the index, timings from the
benchmark's raw TSV on the machine that ran it."""
import glob, json, os, re, sys

index = sys.argv[1] if len(sys.argv) > 1 else "../pour-registry/dist/index.json"
results = sys.argv[2] if len(sys.argv) > 2 else "../pour-bench/results"
apps = "../pour-apps/index.json"

rows = []
try:
    meta = json.load(open(index))
    meta = meta.get("meta", meta)
    rows.append(("Formulae registered", f"**{int(meta['formulae']):,}**"))
    rows.append(("Casks registered", f"**{int(meta['casks']):,}**"))
except Exception as e:
    print("no index:", e)
try:
    n = len(json.load(open(apps))["casks"])
    rows.append(("The creator's apps", f"**{n}**"))
except Exception:
    pass

runs = sorted(glob.glob(os.path.join(results, "*", "timings.tsv")))
if runs:
    t = {}
    for line in open(runs[-1]):
        parts = line.rstrip("\n").split("\t")
        if len(parts) >= 2:
            t[parts[0]] = float(parts[1])
    def fmt(ms):
        return f"{ms/1000:.1f} s" if ms >= 1000 else f"{ms:.0f} ms"
    def pair(label, pour_key, brew_key):
        if pour_key in t and brew_key in t and t[pour_key] > 0:
            rows.append((label, f"**{fmt(t[pour_key])}** (Homebrew: {fmt(t[brew_key])}, {t[brew_key]/t[pour_key]:.0f}× slower)"))
    pair("`pour info jq`", "pour-info jq (mean of 20)", "brew-info jq (mean of 3)")
    pair("`pour search json`", "pour-search json (mean of 20)", "brew-search json (mean of 3)")
    pair("Warm `pour install jq`", "pour-warm-jq", "brew-warm-jq")
    pair("Cold `pour install ffmpeg`", "pour-cold-ffmpeg", "brew-cold-ffmpeg")
    date = os.path.basename(os.path.dirname(runs[-1]))
    rows.append(("Measured", f"[pour-bench](https://github.com/JKS-sys/pour-bench-13-Oct-2026), {date}, same Linux machine, same packages, both tools from a clean state"))

block = "<!-- stats:start -->\n| | |\n|---|---|\n" + "".join(f"| {k} | {v} |\n" for k, v in rows) + "<!-- stats:end -->"
p = "README.md"
s = open(p).read()
new = re.sub(r"<!-- stats:start -->.*?<!-- stats:end -->", block, s, flags=re.S)
open(p, "w").write(new)
print("README stats:", len(rows), "rows", "from", runs[-1] if runs else "no bench")
