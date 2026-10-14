//! Bottles on GitHub Packages: blob URLs, the OCI manifest and the
//! `sh.brew.tab` annotation that tells us which files need relocation.

use crate::model::{Bottle, Formula};
use crate::net::{self, GHCR_AUTH};
use serde::{Deserialize, Serialize};

/// `python@3.12` → `python/3.12`, `libsigc++` → `libsigcxx` (OCI names).
pub fn image_name(formula: &str) -> String {
    formula.replace('@', "/").replace('+', "x")
}

/// Download URL of a bottle blob.
pub fn blob_url(f: &Formula, b: &Bottle) -> String {
    let root = if f.root_url.is_empty() {
        "https://ghcr.io/v2/homebrew/core"
    } else {
        f.root_url.as_str()
    };
    format!(
        "{}/{}/blobs/sha256:{}",
        root.trim_end_matches('/'),
        image_name(&f.name),
        b.sha256
    )
}

/// URL of the OCI image index carrying every tag's annotations.
pub fn manifest_url(f: &Formula) -> String {
    let root = if f.root_url.is_empty() {
        "https://ghcr.io/v2/homebrew/core"
    } else {
        f.root_url.as_str()
    };
    let mut version = f.pkg_version();
    if f.rebuild > 0 {
        version.push_str(&format!("-{}", f.rebuild));
    }
    format!(
        "{}/{}/manifests/{}",
        root.trim_end_matches('/'),
        image_name(&f.name),
        version
    )
}

/// Headers for ghcr.io requests.
pub fn headers(url: &str) -> Vec<(String, String)> {
    let mut h = Vec::new();
    if url.starts_with("https://ghcr.io/") {
        h.push(("Authorization".to_string(), GHCR_AUTH.to_string()));
    }
    h
}

/// The subset of Homebrew's install receipt a bottle manifest carries.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Tab {
    #[serde(default)]
    pub changed_files: Option<Vec<String>>,
    #[serde(default)]
    pub linkage_files: Option<Vec<String>>,
    #[serde(default)]
    pub built_prefix: Option<String>,
    #[serde(default)]
    pub runtime_dependencies: Option<Vec<RuntimeDep>>,
    #[serde(default)]
    pub bottle_size: Option<u64>,
    #[serde(default)]
    pub installed_size: Option<u64>,
    #[serde(default)]
    pub glibc_version: Option<String>,
    /// Homebrew that built the bottle (`"5.1.15"`), from the tab.
    #[serde(default)]
    pub homebrew_version: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RuntimeDep {
    pub full_name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub declared_directly: bool,
}

impl Tab {
    /// All files the manifest says carry prefix references.
    pub fn files(&self) -> Option<Vec<String>> {
        match (&self.changed_files, &self.linkage_files) {
            (None, None) => None,
            (a, b) => {
                let mut v: Vec<String> = a.clone().unwrap_or_default();
                for f in b.clone().unwrap_or_default() {
                    if !v.contains(&f) {
                        v.push(f);
                    }
                }
                Some(v)
            }
        }
    }
}

/// Fetch the manifest and extract the tab for the bottle with `sha256`.
/// Any failure yields `None`: Pour then scans the whole keg instead.
pub fn fetch_tab(agent: &ureq::Agent, f: &Formula, b: &Bottle) -> Option<Tab> {
    let url = manifest_url(f);
    let mut hdrs: Vec<(String, String)> = headers(&url);
    hdrs.push((
        "Accept".into(),
        "application/vnd.oci.image.index.v1+json".into(),
    ));
    let h: Vec<(&str, &str)> = hdrs.iter().map(|(a, c)| (a.as_str(), c.as_str())).collect();
    let json = net::get_json(agent, &url, &h).ok()?;
    parse_tab(&json, &b.sha256)
}

pub fn parse_tab(index: &serde_json::Value, sha256: &str) -> Option<Tab> {
    let manifests = index.get("manifests")?.as_array()?;
    for m in manifests {
        let ann = m.get("annotations")?;
        if ann.get("sh.brew.bottle.digest").and_then(|d| d.as_str()) != Some(sha256) {
            continue;
        }
        let tab_text = ann.get("sh.brew.tab").and_then(|t| t.as_str())?;
        let mut tab: Tab = serde_json::from_str(tab_text).ok()?;
        tab.bottle_size = ann
            .get("sh.brew.bottle.size")
            .and_then(|v| v.as_str())
            .and_then(|v| v.parse().ok());
        tab.installed_size = ann
            .get("sh.brew.bottle.installed_size")
            .and_then(|v| v.as_str())
            .and_then(|v| v.parse().ok());
        tab.glibc_version = ann
            .get("sh.brew.bottle.glibc.version")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        return Some(tab);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Cellar;

    #[test]
    fn urls_follow_homebrew_conventions() {
        let f = Formula {
            name: "python@3.14".into(),
            version: "3.14.0".into(),
            revision: 1,
            rebuild: 2,
            root_url: "https://ghcr.io/v2/homebrew/core".into(),
            ..Default::default()
        };
        let b = Bottle {
            tag: "arm64_tahoe".into(),
            cellar: Cellar::Any,
            sha256: "ab".repeat(32),
        };
        assert_eq!(
            blob_url(&f, &b),
            format!(
                "https://ghcr.io/v2/homebrew/core/python/3.14/blobs/sha256:{}",
                "ab".repeat(32)
            )
        );
        assert_eq!(
            manifest_url(&f),
            "https://ghcr.io/v2/homebrew/core/python/3.14/manifests/3.14.0_1-2"
        );
        assert_eq!(image_name("libsigc++@2"), "libsigcxx/2");
        assert_eq!(headers("https://ghcr.io/v2/x")[0].1, GHCR_AUTH);
        assert!(headers("https://example.com/x").is_empty());
    }

    #[test]
    fn tab_parsing_picks_matching_digest() {
        let idx = serde_json::json!({"manifests": [
            {"annotations": {"sh.brew.bottle.digest": "aaa", "sh.brew.tab": "{\"changed_files\":[\"bin/x\"],\"linkage_files\":[\"lib/y.dylib\"]}", "sh.brew.bottle.size": "123"}},
            {"annotations": {"sh.brew.bottle.digest": "bbb", "sh.brew.tab": "{\"changed_files\":[\"other\"]}"}}
        ]});
        let tab = parse_tab(&idx, "aaa").unwrap();
        assert_eq!(tab.files().unwrap(), vec!["bin/x", "lib/y.dylib"]);
        assert_eq!(tab.bottle_size, Some(123));
        assert!(parse_tab(&idx, "zzz").is_none());
    }
}
