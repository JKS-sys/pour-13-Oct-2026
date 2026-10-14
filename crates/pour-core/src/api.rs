//! Conversion from Homebrew's JSON API (`formula.json`, `cask.json`) into
//! Pour's compact records. Pour-native registries (such as `pour-apps`)
//! use the same JSON shape, optionally extended with a `pour` object that
//! carries installers for systems Homebrew does not cover.

use crate::model::*;
use serde_json::Value;

fn s(v: &Value) -> String {
    match v {
        Value::String(x) => x.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn opt_s(v: &Value) -> Option<String> {
    match v {
        Value::String(x) if !x.is_empty() => Some(x.clone()),
        Value::Null => None,
        Value::String(_) => None,
        other => Some(other.to_string()),
    }
}

fn strs(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn json_text(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::Array(a) if a.is_empty() => None,
        Value::Object(o) if o.is_empty() => None,
        other => Some(other.to_string()),
    }
}

/// Convert one formula object from `formula.json`.
pub fn formula_from_api(v: &Value) -> Option<Formula> {
    let name = v.get("name")?.as_str()?.to_string();
    let versions = v.get("versions").cloned().unwrap_or(Value::Null);
    let version = s(&versions["stable"]);
    if version.is_empty() {
        return None;
    }
    let mut bottles = Vec::new();
    let mut root_url = String::new();
    let mut rebuild = 0u32;
    if let Some(stable) = v.pointer("/bottle/stable") {
        root_url = s(&stable["root_url"]);
        rebuild = stable["rebuild"].as_u64().unwrap_or(0) as u32;
        if let Some(files) = stable["files"].as_object() {
            for (tag, f) in files {
                let sha = s(&f["sha256"]).to_ascii_lowercase();
                if sha.len() != 64 {
                    continue;
                }
                bottles.push(Bottle {
                    tag: tag.clone(),
                    cellar: Cellar::parse(&s(&f["cellar"])),
                    sha256: sha,
                });
            }
        }
    }
    bottles.sort_by(|a, b| a.tag.cmp(&b.tag));

    let uses_from_macos = v["uses_from_macos"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|u| match u {
                    Value::String(n) => Some(UsesFromMacos {
                        name: n.clone(),
                        kind: String::new(),
                    }),
                    Value::Object(o) => o.iter().next().map(|(n, k)| UsesFromMacos {
                        name: n.clone(),
                        kind: match k {
                            Value::String(x) => x.clone(),
                            Value::Array(a) => a
                                .iter()
                                .filter_map(|x| x.as_str())
                                .collect::<Vec<_>>()
                                .join(","),
                            _ => String::new(),
                        },
                    }),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();

    // Linux dependency variations (x86_64_linux and arm64_linux agree in
    // practice; prefer x86_64_linux then arm64_linux).
    let linux_deps = ["x86_64_linux", "arm64_linux"]
        .iter()
        .find_map(|t| v.pointer(&format!("/variations/{t}/dependencies")))
        .map(strs);

    let requirements = v["requirements"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|r| Requirement {
                    name: s(&r["name"]),
                    version: opt_s(&r["version"]),
                    cask: opt_s(&r["cask"]),
                    download: opt_s(&r["download"]),
                    contexts: strs(&r["contexts"]),
                })
                .collect()
        })
        .unwrap_or_default();

    Some(Formula {
        full_name: v["full_name"].as_str().unwrap_or(&name).to_string(),
        tap: v["tap"].as_str().unwrap_or("homebrew/core").to_string(),
        desc: s(&v["desc"]),
        homepage: s(&v["homepage"]),
        license: s(&v["license"]),
        version,
        revision: v["revision"].as_u64().unwrap_or(0) as u32,
        version_scheme: v["version_scheme"].as_u64().unwrap_or(0) as u32,
        aliases: strs(&v["aliases"]),
        oldnames: strs(&v["oldnames"]),
        deps: strs(&v["dependencies"]),
        build_deps: strs(&v["build_dependencies"]),
        uses_from_macos,
        linux_deps,
        bottles,
        root_url,
        rebuild,
        keg_only: v["keg_only"].as_bool().unwrap_or(false),
        keg_only_reason: v
            .pointer("/keg_only_reason/explanation")
            .and_then(|x| x.as_str())
            .map(str::to_string),
        caveats: opt_s(&v["caveats"]),
        service: json_text(&v["service"]),
        post_install: json_text(&v["post_install_steps"]),
        conflicts_with: strs(&v["conflicts_with"]),
        deprecated: v["deprecated"].as_bool().unwrap_or(false),
        deprecation_reason: opt_s(&v["deprecation_reason"]),
        disabled: v["disabled"].as_bool().unwrap_or(false),
        disable_reason: opt_s(&v["disable_reason"]),
        requirements,
        pour_bottle_only_if: opt_s(&v["pour_bottle_only_if"]),
        executables: strs(&v["executables"]),
        link_overwrite: strs(&v["link_overwrite"]),
        name,
    })
}

/// Convert one cask object from `cask.json` (or a Pour-native registry).
pub fn cask_from_api(v: &Value) -> Option<Cask> {
    let token = v.get("token")?.as_str()?.to_string();
    let sha = s(&v["sha256"]).to_ascii_lowercase();
    let sha256 = if sha.len() == 64 { Some(sha) } else { None };
    let variations = v["variations"]
        .as_object()
        .map(|o| {
            o.iter()
                .map(|(tag, x)| CaskVariation {
                    tag: tag.clone(),
                    url: opt_s(&x["url"]),
                    sha256: opt_s(&x["sha256"])
                        .filter(|h| h.len() == 64)
                        .map(|h| h.to_ascii_lowercase()),
                    version: opt_s(&x["version"]),
                    artifacts: json_text(&x["artifacts"]),
                })
                .filter(|x| x.url.is_some() || x.version.is_some() || x.artifacts.is_some())
                .collect()
        })
        .unwrap_or_default();
    let native = v
        .pointer("/pour/installers")
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .map(|i| NativeInstaller {
                    platform: s(&i["platform"]),
                    url: s(&i["url"]),
                    sha256: opt_s(&i["sha256"]).map(|h| h.to_ascii_lowercase()),
                    kind: s(&i["kind"]),
                })
                .filter(|i| !i.url.is_empty() && !i.platform.is_empty())
                .collect()
        })
        .unwrap_or_default();
    Some(Cask {
        full_token: v["full_token"].as_str().unwrap_or(&token).to_string(),
        tap: v["tap"].as_str().unwrap_or("homebrew/cask").to_string(),
        names: strs(&v["name"]),
        old_tokens: strs(&v["old_tokens"]),
        desc: s(&v["desc"]),
        homepage: s(&v["homepage"]),
        version: s(&v["version"]),
        sha256,
        url: s(&v["url"]),
        container: v
            .pointer("/container/type")
            .and_then(|x| x.as_str())
            .map(str::to_string),
        depends_on: json_text(&v["depends_on"]),
        artifacts: v["artifacts"].to_string(),
        auto_updates: v["auto_updates"].as_bool().unwrap_or(false),
        caveats: opt_s(&v["caveats"]),
        conflicts_with: json_text(&v["conflicts_with"]),
        deprecated: v["deprecated"].as_bool().unwrap_or(false),
        deprecation_reason: opt_s(&v["deprecation_reason"]),
        disabled: v["disabled"].as_bool().unwrap_or(false),
        disable_reason: opt_s(&v["disable_reason"]),
        variations,
        languages: strs(&v["languages"]),
        native,
        livecheck_url: v
            .pointer("/pour/livecheck")
            .and_then(|x| x.as_str())
            .map(str::to_string),
        clear_quarantine: v
            .pointer("/pour/clear_quarantine")
            .and_then(|x| x.as_bool())
            .unwrap_or(false),
        token,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn formula_conversion_keeps_bottles_and_deps() {
        let v = json!({
            "name": "jq", "full_name": "jq", "tap": "homebrew/core", "desc": "JSON processor",
            "homepage": "https://jqlang.github.io/jq/", "license": "MIT",
            "versions": {"stable": "1.8.2", "bottle": true}, "revision": 0,
            "bottle": {"stable": {"rebuild": 1, "root_url": "https://ghcr.io/v2/homebrew/core", "files": {
                "arm64_tahoe": {"cellar": ":any", "url": "x", "sha256": "CA67C64D0AAF1E5472790EC2CC081FF7972316F27095D8A8AAB81B3321247036"},
                "x86_64_linux": {"cellar": "/home/linuxbrew/.linuxbrew/Cellar", "url": "y", "sha256": "631787d9b19df5e5557cc83f972080e541f543f8df2d1f212ca4290ff23e9764"}}}},
            "dependencies": ["oniguruma"], "build_dependencies": [], "uses_from_macos": ["zlib", {"bison": "build"}],
            "variations": {"x86_64_linux": {"dependencies": ["oniguruma", "gcc"]}},
            "keg_only": false, "aliases": [], "oldnames": ["jq-old"], "executables": ["jq"]
        });
        let f = formula_from_api(&v).unwrap();
        assert_eq!(f.pkg_version(), "1.8.2");
        assert_eq!(f.bottles.len(), 2);
        assert_eq!(
            f.bottles[0].sha256,
            "ca67c64d0aaf1e5472790ec2cc081ff7972316f27095d8a8aab81b3321247036"
        );
        assert_eq!(f.bottles[0].cellar, Cellar::Any);
        assert_eq!(
            f.bottles[1].cellar,
            Cellar::Fixed("/home/linuxbrew/.linuxbrew/Cellar".into())
        );
        assert_eq!(f.runtime_deps(false), vec!["oniguruma"]);
        assert_eq!(f.runtime_deps(true), vec!["oniguruma", "gcc", "zlib"]);
        assert_eq!(f.rebuild, 1);
    }

    #[test]
    fn cask_conversion_reads_variations_and_native_installers() {
        let v = json!({
            "token": "peek", "full_token": "jks-sys/pour-apps/peek", "tap": "jks-sys/pour-apps", "name": ["Peek"],
            "desc": "Image viewer", "homepage": "https://ipconfig.co.network/peek", "version": "1.2.3",
            "sha256": "no_check", "url": "https://example.com/Peek-1.2.3-arm64.dmg",
            "artifacts": [{"app": ["Peek.app"]}],
            "variations": {"sonoma": {"url": "https://example.com/Peek-1.2.3-intel.dmg"}},
            "pour": {"installers": [{"platform": "windows-x86_64", "url": "https://example.com/Peek-Setup.exe", "kind": "exe"}],
                     "livecheck": "https://ipconfig.co.network/updates/peek/latest.json"}
        });
        let c = cask_from_api(&v).unwrap();
        assert_eq!(c.sha256, None);
        assert_eq!(c.variations.len(), 1);
        assert_eq!(c.native[0].kind, "exe");
        let (url, _, ver, _) = c.download_for(&["sonoma".to_string()]);
        assert!(url.ends_with("intel.dmg"));
        assert_eq!(ver, "1.2.3");
        assert_eq!(
            c.livecheck_url.as_deref(),
            Some("https://ipconfig.co.network/updates/peek/latest.json")
        );
    }
}
