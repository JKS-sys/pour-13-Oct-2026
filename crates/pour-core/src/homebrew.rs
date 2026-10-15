//! What Homebrew has installed on this machine, read straight from its Cellar
//! and Caskroom (no `brew` process, no Ruby): `pour list` shows it beside
//! Pour's own packages, `pour info` mentions it, `pour migrate` installs it.
//!
//! Copyright (c) 2026 Jagadeesh Kumar S. All rights reserved. See LICENSE.

use std::path::{Path, PathBuf};

/// One formula or cask Homebrew installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrewItem {
    pub name: String,
    /// Newest installed version (directory names sort by version parts).
    pub version: String,
    /// `INSTALL_RECEIPT.json` → `installed_on_request` (casks: always true).
    pub on_request: bool,
    pub path: PathBuf,
}

/// Homebrew prefixes that exist here: `$HOMEBREW_PREFIX`, `/opt/homebrew`,
/// `/usr/local` (only with a Cellar or Caskroom), `/home/linuxbrew/.linuxbrew`,
/// `~/.linuxbrew`. Pour's own prefix is never one of them.
pub fn prefixes(pour_prefix: &Path) -> Vec<PathBuf> {
    let mut c: Vec<PathBuf> = Vec::new();
    if let Some(p) = std::env::var_os("HOMEBREW_PREFIX") {
        c.push(PathBuf::from(p));
    }
    for p in ["/opt/homebrew", "/usr/local", "/home/linuxbrew/.linuxbrew"] {
        c.push(PathBuf::from(p));
    }
    if let Some(h) = std::env::var_os("HOME") {
        c.push(PathBuf::from(h).join(".linuxbrew"));
    }
    let mut out: Vec<PathBuf> = Vec::new();
    for p in c {
        if p == pour_prefix || out.contains(&p) {
            continue;
        }
        if p.join("Cellar").is_dir() || p.join("Caskroom").is_dir() {
            out.push(p);
        }
    }
    out
}

fn version_key(v: &str) -> Vec<u64> {
    v.split(['.', '_', '-', ','])
        .map(|x| {
            x.chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse()
                .unwrap_or(0)
        })
        .collect()
}

fn newest_version(dir: &Path) -> Option<(String, PathBuf)> {
    let mut best: Option<(String, PathBuf)> = None;
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        if n.starts_with('.') || !e.path().is_dir() {
            continue;
        }
        let better = match &best {
            None => true,
            Some((b, _)) => version_key(&n) > version_key(b),
        };
        if better {
            best = Some((n, e.path()));
        }
    }
    best
}

fn scan(root: &Path, receipts: bool) -> Vec<BrewItem> {
    let Ok(rd) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out: Vec<BrewItem> = rd
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                return None;
            }
            let (version, path) = newest_version(&e.path())?;
            let on_request = !receipts
                || std::fs::read_to_string(path.join("INSTALL_RECEIPT.json"))
                    .ok()
                    .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
                    .and_then(|v| v.get("installed_on_request").and_then(|b| b.as_bool()))
                    .unwrap_or(true);
            Some(BrewItem {
                name,
                version,
                on_request,
                path,
            })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Formulae and casks Homebrew installed, across every Homebrew prefix here.
pub fn installed(pour_prefix: &Path) -> (Vec<BrewItem>, Vec<BrewItem>) {
    let mut f: Vec<BrewItem> = Vec::new();
    let mut c: Vec<BrewItem> = Vec::new();
    for p in prefixes(pour_prefix) {
        for i in scan(&p.join("Cellar"), true) {
            if !f.iter().any(|x| x.name == i.name) {
                f.push(i);
            }
        }
        for i in scan(&p.join("Caskroom"), false) {
            if !c.iter().any(|x| x.name == i.name) {
                c.push(i);
            }
        }
    }
    f.sort_by(|a, b| a.name.cmp(&b.name));
    c.sort_by(|a, b| a.name.cmp(&b.name));
    (f, c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_cellar_and_caskroom() {
        let d = std::env::temp_dir().join(format!("pour-brew-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        for (n, v) in [
            ("cmake", "4.1.2"),
            ("cmake", "4.0.0"),
            ("git", "2.51.0"),
            ("mysql@8.4", "8.4.6_2"),
        ] {
            std::fs::create_dir_all(d.join("Cellar").join(n).join(v)).unwrap();
        }
        std::fs::write(
            d.join("Cellar/git/2.51.0/INSTALL_RECEIPT.json"),
            r#"{"installed_on_request": false}"#,
        )
        .unwrap();
        std::fs::create_dir_all(d.join("Caskroom/tg-pro/2.104")).unwrap();
        std::env::set_var("HOMEBREW_PREFIX", &d);
        let (f, c) = installed(Path::new("/nonexistent-pour"));
        let cm = f.iter().find(|i| i.name == "cmake").unwrap();
        assert_eq!(cm.version, "4.1.2");
        assert!(!f.iter().find(|i| i.name == "git").unwrap().on_request);
        assert!(f.iter().any(|i| i.name == "mysql@8.4"));
        assert_eq!(c[0].name, "tg-pro");
        std::env::remove_var("HOMEBREW_PREFIX");
        let _ = std::fs::remove_dir_all(&d);
    }
}
