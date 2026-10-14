//! Where Pour keeps things. Everything lives under one prefix, `/opt/pour`
//! by default, which is deliberately shorter than `/opt/homebrew`: every
//! path rewritten inside a bottle shrinks, so load commands are patched in
//! place without ever needing to grow a Mach-O header.
//!
//! ```text
//! /opt/pour/
//!   bin/ lib/ include/ share/ etc/ opt/   symlinks into kegs (like Homebrew)
//!   Cellar/<name>/<version>/              kegs (APFS clones of the store)
//!   Caskroom/<token>/<version>/           cask metadata
//!   store/<sha256>/                       content-addressed, relocated kegs
//!   cache/downloads/                      bottles and cask downloads
//!   var/pour/index.pour                   the index
//!   var/pour/taps/<name>.pour             extra registries
//!   var/pour/db/                          receipts, locks, pins
//! ```

use crate::platform::{Os, Platform};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Paths {
    pub prefix: PathBuf,
}

impl Paths {
    /// `$POUR_PREFIX`, else the platform default.
    pub fn detect(platform: &Platform) -> Paths {
        if let Ok(p) = std::env::var("POUR_PREFIX") {
            if !p.trim().is_empty() {
                return Paths {
                    prefix: PathBuf::from(p),
                };
            }
        }
        Paths {
            prefix: default_prefix(platform),
        }
    }

    pub fn new(prefix: impl Into<PathBuf>) -> Paths {
        Paths {
            prefix: prefix.into(),
        }
    }

    pub fn bin(&self) -> PathBuf {
        self.prefix.join("bin")
    }
    pub fn cellar(&self) -> PathBuf {
        self.prefix.join("Cellar")
    }
    pub fn caskroom(&self) -> PathBuf {
        self.prefix.join("Caskroom")
    }
    pub fn opt(&self) -> PathBuf {
        self.prefix.join("opt")
    }
    pub fn store(&self) -> PathBuf {
        self.prefix.join("store")
    }
    pub fn cache(&self) -> PathBuf {
        if let Ok(c) = std::env::var("POUR_CACHE") {
            if !c.trim().is_empty() {
                return PathBuf::from(c);
            }
        }
        self.prefix.join("cache")
    }
    pub fn downloads(&self) -> PathBuf {
        self.cache().join("downloads")
    }
    pub fn var(&self) -> PathBuf {
        self.prefix.join("var")
    }
    pub fn etc(&self) -> PathBuf {
        self.prefix.join("etc")
    }
    pub fn state(&self) -> PathBuf {
        self.var().join("pour")
    }
    pub fn index_file(&self) -> PathBuf {
        self.state().join("index.pour")
    }
    pub fn taps_dir(&self) -> PathBuf {
        self.state().join("taps")
    }
    pub fn db(&self) -> PathBuf {
        self.state().join("db")
    }
    pub fn pins(&self) -> PathBuf {
        self.db().join("pinned")
    }
    pub fn lock_file(&self) -> PathBuf {
        self.db().join("lock")
    }
    pub fn keg(&self, name: &str, version: &str) -> PathBuf {
        self.cellar().join(name).join(version)
    }
    pub fn opt_link(&self, name: &str) -> PathBuf {
        self.opt().join(name)
    }
    pub fn applications(&self) -> PathBuf {
        if let Ok(a) = std::env::var("POUR_APPDIR") {
            if !a.trim().is_empty() {
                return PathBuf::from(a);
            }
        }
        PathBuf::from("/Applications")
    }

    /// Is the prefix in place and writable by this user?
    pub fn writable(&self) -> bool {
        is_writable(&self.prefix)
    }

    /// Create every directory Pour needs. Fails (with a hint) when the
    /// prefix is missing and cannot be created without privileges.
    pub fn ensure(&self) -> std::io::Result<()> {
        for d in [
            self.prefix.clone(),
            self.bin(),
            self.prefix.join("sbin"),
            self.prefix.join("lib"),
            self.prefix.join("include"),
            self.prefix.join("share"),
            self.etc(),
            self.opt(),
            self.cellar(),
            self.caskroom(),
            self.store(),
            self.downloads(),
            self.state(),
            self.taps_dir(),
            self.db(),
            self.var().join("log"),
        ] {
            std::fs::create_dir_all(&d)?;
        }
        Ok(())
    }

    /// The exact text Homebrew bottles must have rewritten so they run from
    /// this prefix.
    pub fn prefix_str(&self) -> String {
        self.prefix.to_string_lossy().into_owned()
    }
}

pub fn default_prefix(platform: &Platform) -> PathBuf {
    match platform.os {
        Os::Windows => {
            let base = std::env::var("LOCALAPPDATA")
                .or_else(|_| std::env::var("USERPROFILE"))
                .unwrap_or_else(|_| "C:\\".into());
            Path::new(&base).join("Pour")
        }
        Os::Haiku => home().join("config").join("non-packaged").join("pour"),
        Os::Ios => home().join(".pour"),
        Os::Linux if platform.ish => home().join(".pour"),
        _ => PathBuf::from("/opt/pour"),
    }
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

pub fn is_writable(p: &Path) -> bool {
    if !p.exists() {
        return false;
    }
    let probe = p.join(format!(".pour-write-test-{}", std::process::id()));
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_under_prefix() {
        let p = Paths::new("/opt/pour");
        assert_eq!(p.cellar(), PathBuf::from("/opt/pour/Cellar"));
        assert_eq!(
            p.keg("jq", "1.8.2"),
            PathBuf::from("/opt/pour/Cellar/jq/1.8.2")
        );
        assert_eq!(
            p.index_file(),
            PathBuf::from("/opt/pour/var/pour/index.pour")
        );
        assert!(p.prefix_str().len() <= "/opt/homebrew".len());
    }

    #[test]
    fn ensure_creates_tree() {
        let dir = tempfile::tempdir().unwrap();
        let p = Paths::new(dir.path().join("pour"));
        p.ensure().unwrap();
        assert!(p.store().is_dir());
        assert!(p.writable());
    }
}
