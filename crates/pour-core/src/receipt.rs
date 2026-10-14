//! Install receipts: what is installed, from where, and which links it owns.
//! A receipt lives inside its keg (`.pour-receipt.json`) and a minimal
//! Homebrew-style `INSTALL_RECEIPT.json` sits beside it so tools that read
//! Homebrew cellars still understand the keg.

use crate::model::Kind;
use crate::paths::Paths;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const RECEIPT: &str = ".pour-receipt.json";
/// One file with every receipt, so `list`, `info`, `search` and `outdated`
/// read one small JSON instead of walking the Cellar (hundreds of syscalls
/// with 60 kegs). Rewritten whenever a receipt changes; also checked against
/// the Cellar/opt/Caskroom directory mtimes so a manual `rm -rf` never lies.
pub const STATE_CACHE: &str = "installed.json";

#[derive(Serialize, Deserialize)]
struct StateCache {
    stamp: (u64, u64, u64),
    formulae: Vec<(Receipt, PathBuf, bool)>,
    casks: Vec<(Receipt, PathBuf, bool)>,
}

fn dir_stamp(p: &Path) -> u64 {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

fn stamp(paths: &Paths) -> (u64, u64, u64) {
    (
        dir_stamp(&paths.cellar()),
        dir_stamp(&paths.opt()),
        dir_stamp(&paths.caskroom()),
    )
}

fn cache_file(paths: &Paths) -> PathBuf {
    paths.state().join(STATE_CACHE)
}

/// Forget the cached state (the next read rescans the Cellar).
pub fn invalidate(paths: &Paths) {
    let _ = std::fs::remove_file(cache_file(paths));
}

fn load_state(paths: &Paths) -> Option<StateCache> {
    let text = std::fs::read(cache_file(paths)).ok()?;
    let c: StateCache = serde_json::from_slice(&text).ok()?;
    (c.stamp == stamp(paths)).then_some(c)
}

/// A receipt without its (possibly thousands of) link and artifact entries.
fn slim(mut r: Receipt) -> Receipt {
    r.links_len = r.links.len().max(r.artifacts.len());
    r.links = Vec::new();
    r.artifacts = Vec::new();
    r
}

fn scan_and_cache(paths: &Paths) -> StateCache {
    let st = stamp(paths);
    let c = StateCache {
        stamp: st,
        formulae: scan_formulae(paths)
            .into_iter()
            .map(|i| (slim(i.receipt), i.path, i.active))
            .collect(),
        casks: scan_casks(paths)
            .into_iter()
            .map(|i| (slim(i.receipt), i.path, i.active))
            .collect(),
    };
    if let Ok(bytes) = serde_json::to_vec(&c) {
        let f = cache_file(paths);
        let tmp = f.with_extension("json.tmp");
        if std::fs::create_dir_all(paths.state()).is_ok() && std::fs::write(&tmp, bytes).is_ok() {
            let _ = std::fs::rename(&tmp, &f);
        }
    }
    c
}

/// Every installed formula keg, newest versions last within a name.
pub fn installed_formulae(paths: &Paths) -> Vec<Installed> {
    let c = load_state(paths).unwrap_or_else(|| scan_and_cache(paths));
    c.formulae
        .into_iter()
        .map(|(receipt, path, active)| Installed {
            receipt,
            path,
            active,
        })
        .collect()
}

/// Every installed cask (Caskroom/<token>/<version>).
pub fn installed_casks(paths: &Paths) -> Vec<Installed> {
    let c = load_state(paths).unwrap_or_else(|| scan_and_cache(paths));
    c.casks
        .into_iter()
        .map(|(receipt, path, active)| Installed {
            receipt,
            path,
            active,
        })
        .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Receipt {
    pub kind: Kind,
    pub name: String,
    pub version: String,
    pub tap: String,
    pub desc: String,
    pub homepage: String,
    /// Bottle (or cask download) digest.
    pub sha256: Option<String>,
    pub tag: String,
    pub cellar: Option<String>,
    pub installed_at: u64,
    pub installed_on_request: bool,
    pub installed_as_dependency: bool,
    pub runtime_deps: Vec<String>,
    pub keg_only: bool,
    /// Prefix-relative links this keg owns.
    pub links: Vec<String>,
    /// For casks: artifacts placed outside the prefix (apps, binaries, fonts).
    pub artifacts: Vec<String>,
    pub relocated_files: usize,
    pub deep_relocation: bool,
    pub pour_version: String,
    pub platform: String,
    pub pinned: bool,
    /// Pour-native registry install: which installer kind ran.
    pub native_kind: Option<String>,
    /// Number of links/artifacts, kept when the lists themselves are left
    /// out of the installed-state cache. Use `Installed::full()` for the lists.
    #[serde(default)]
    pub links_len: usize,
}

impl Default for Kind {
    fn default() -> Self {
        Kind::Formula
    }
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Receipt {
    pub fn write(&self, keg: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(keg)?;
        // Cellar/<name>/<version> or Caskroom/<token>/<version>: the prefix is three up
        if let Some(prefix) = keg.ancestors().nth(3) {
            let _ = std::fs::remove_file(prefix.join("var/pour").join(STATE_CACHE));
        }
        let tmp = keg.join(format!("{RECEIPT}.tmp"));
        std::fs::write(&tmp, serde_json::to_vec_pretty(self).unwrap_or_default())?;
        std::fs::rename(&tmp, keg.join(RECEIPT))?;
        if self.kind == Kind::Formula {
            let hb = serde_json::json!({
                "homebrew_version": format!("pour {}", self.pour_version),
                "used_options": [],
                "unused_options": [],
                "built_as_bottle": true,
                "poured_from_bottle": true,
                "loaded_from_api": true,
                "installed_as_dependency": self.installed_as_dependency,
                "installed_on_request": self.installed_on_request,
                "changed_files": [],
                "time": self.installed_at,
                "source_modified_time": self.installed_at,
                "compiler": "clang",
                "aliases": [],
                "runtime_dependencies": self.runtime_deps.iter().map(|d| serde_json::json!({"full_name": d, "version": "", "declared_directly": true})).collect::<Vec<_>>(),
                "source": {"path": null, "tap": self.tap, "tap_git_head": null, "spec": "stable", "versions": {"stable": self.version, "head": null, "version_scheme": 0}},
                "arch": std::env::consts::ARCH,
                "built_on": {"os": std::env::consts::OS},
            });
            let _ = std::fs::write(
                keg.join("INSTALL_RECEIPT.json"),
                serde_json::to_vec_pretty(&hb).unwrap_or_default(),
            );
        }
        Ok(())
    }

    pub fn read(keg: &Path) -> Option<Receipt> {
        let text = std::fs::read_to_string(keg.join(RECEIPT)).ok()?;
        serde_json::from_str(&text).ok()
    }
}

/// One installed keg or cask.
#[derive(Debug, Clone)]
pub struct Installed {
    pub receipt: Receipt,
    pub path: PathBuf,
    /// Is this the version `opt/<name>` points at?
    pub active: bool,
}

impl Installed {
    /// The complete receipt from the keg (with `links` and `artifacts`),
    /// which the cached one leaves out.
    pub fn full(&self) -> Receipt {
        Receipt::read(&self.path).unwrap_or_else(|| self.receipt.clone())
    }
}

fn scan_formulae(paths: &Paths) -> Vec<Installed> {
    let mut out = Vec::new();
    let Ok(names) = std::fs::read_dir(paths.cellar()) else {
        return out;
    };
    let mut names: Vec<_> = names.flatten().filter(|e| e.path().is_dir()).collect();
    names.sort_by_key(|e| e.file_name());
    for n in names {
        let name = n.file_name().to_string_lossy().into_owned();
        let active_target = std::fs::read_link(paths.opt_link(&name))
            .ok()
            .map(|t| crate::link::normalize(&paths.opt().join(t)));
        let Ok(versions) = std::fs::read_dir(n.path()) else {
            continue;
        };
        let mut versions: Vec<_> = versions.flatten().filter(|e| e.path().is_dir()).collect();
        versions.sort_by_key(|e| e.file_name());
        for v in versions {
            let path = v.path();
            let receipt = Receipt::read(&path).unwrap_or_else(|| Receipt {
                kind: Kind::Formula,
                name: name.clone(),
                version: v.file_name().to_string_lossy().into_owned(),
                ..Default::default()
            });
            let active = active_target.as_deref() == Some(crate::link::normalize(&path).as_path());
            out.push(Installed {
                receipt,
                path,
                active,
            });
        }
    }
    out
}

fn scan_casks(paths: &Paths) -> Vec<Installed> {
    let mut out = Vec::new();
    let Ok(tokens) = std::fs::read_dir(paths.caskroom()) else {
        return out;
    };
    let mut tokens: Vec<_> = tokens.flatten().filter(|e| e.path().is_dir()).collect();
    tokens.sort_by_key(|e| e.file_name());
    for t in tokens {
        let Ok(versions) = std::fs::read_dir(t.path()) else {
            continue;
        };
        let mut versions: Vec<_> = versions.flatten().filter(|e| e.path().is_dir()).collect();
        versions.sort_by_key(|e| e.file_name());
        let last = versions.len().saturating_sub(1);
        for (i, v) in versions.into_iter().enumerate() {
            let path = v.path();
            let Some(receipt) = Receipt::read(&path) else {
                continue;
            };
            out.push(Installed {
                receipt,
                path,
                active: i == last,
            });
        }
    }
    out
}

/// The active installed keg of `name`, if any.
pub fn find_formula(paths: &Paths, name: &str) -> Option<Installed> {
    let mut all: Vec<Installed> = installed_formulae(paths)
        .into_iter()
        .filter(|i| i.receipt.name == name)
        .collect();
    if all.is_empty() {
        return None;
    }
    if let Some(pos) = all.iter().position(|i| i.active) {
        return Some(all.swap_remove(pos));
    }
    all.pop()
}

pub fn find_cask(paths: &Paths, token: &str) -> Option<Installed> {
    installed_casks(paths)
        .into_iter()
        .rfind(|i| i.receipt.name == token)
}

/// Names of installed formulae that depend (at run time) on `name`.
pub fn dependents(paths: &Paths, name: &str) -> Vec<String> {
    installed_formulae(paths)
        .into_iter()
        .filter(|i| i.active && i.receipt.runtime_deps.iter().any(|d| d == name))
        .map(|i| i.receipt.name)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receipts_round_trip_and_listing() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path().join("pour"));
        paths.ensure().unwrap();
        let keg = paths.keg("jq", "1.8.2");
        let r = Receipt {
            kind: Kind::Formula,
            name: "jq".into(),
            version: "1.8.2".into(),
            runtime_deps: vec!["oniguruma".into()],
            installed_on_request: true,
            ..Default::default()
        };
        r.write(&keg).unwrap();
        assert!(keg.join("INSTALL_RECEIPT.json").exists());
        let keg2 = paths.keg("oniguruma", "6.9.10");
        Receipt {
            kind: Kind::Formula,
            name: "oniguruma".into(),
            version: "6.9.10".into(),
            installed_as_dependency: true,
            ..Default::default()
        }
        .write(&keg2)
        .unwrap();
        crate::link::link_keg(&paths, "jq", &keg, false, false).unwrap();
        let all = installed_formulae(&paths);
        assert_eq!(all.len(), 2);
        let jq = find_formula(&paths, "jq").unwrap();
        assert!(jq.active);
        assert_eq!(jq.receipt.runtime_deps, vec!["oniguruma"]);
        assert_eq!(dependents(&paths, "oniguruma"), vec!["jq"]);
        assert!(find_formula(&paths, "nothing").is_none());
    }
}
