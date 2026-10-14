//! Content-addressed store. A bottle is unpacked and relocated once into
//! `store/<sha256>/`; every install of it afterwards is an APFS clone
//! (macOS), a hard-link tree (Linux, same file system) or a copy — which
//! is why a warm `pour install` takes milliseconds.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreMeta {
    pub name: String,
    pub version: String,
    pub tag: String,
    pub sha256: String,
    pub created_at: u64,
    pub relocated: bool,
    pub pour_version: String,
}

pub const META_FILE: &str = ".pour-store.json";

/// `store/<xx>/<sha256>` — two levels below the prefix, like `Cellar/<name>/<version>`,
/// so a bottle's relative symlinks (`../../../../../../../../../etc/…`) land in the
/// same place whether they are followed from the keg or from the store.
pub fn entry_dir(store: &Path, sha256: &str) -> PathBuf {
    let shard = sha256.get(..2).unwrap_or("00");
    store.join(shard).join(sha256)
}

/// Temporary sibling for an entry being built (same shard, atomic rename).
pub fn tmp_dir(store: &Path, sha256: &str) -> PathBuf {
    let shard = sha256.get(..2).unwrap_or("00");
    store
        .join(shard)
        .join(format!("{sha256}.tmp-{}", std::process::id()))
}

/// Is the entry complete (its metadata file is written last)?
pub fn is_complete(store: &Path, sha256: &str) -> bool {
    entry_dir(store, sha256).join(META_FILE).is_file()
}

pub fn read_meta(store: &Path, sha256: &str) -> Option<StoreMeta> {
    let text = std::fs::read_to_string(entry_dir(store, sha256).join(META_FILE)).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn write_meta(dir: &Path, meta: &StoreMeta) -> std::io::Result<()> {
    let tmp = dir.join(format!("{META_FILE}.tmp"));
    std::fs::write(&tmp, serde_json::to_vec_pretty(meta).unwrap_or_default())?;
    std::fs::rename(tmp, dir.join(META_FILE))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CloneMethod {
    /// APFS clone of the whole tree (macOS): one syscall, copy-on-write.
    Clonefile,
    /// The keg is a symlink to the store entry (Linux and friends): zero copies.
    Symlink,
    Hardlink,
    Copy,
}

/// Materialize a store entry at `dest` (which must not exist).
pub fn materialize(src: &Path, dest: &Path) -> std::io::Result<CloneMethod> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    #[cfg(target_os = "macos")]
    {
        if clonefile_dir(src, dest).is_ok() {
            let _ = std::fs::remove_file(dest.join(META_FILE));
            return Ok(CloneMethod::Clonefile);
        }
        let _ = std::fs::remove_dir_all(dest);
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let target = crate::link::relative_target(dest, src);
        if std::os::unix::fs::symlink(&target, dest).is_ok() {
            return Ok(CloneMethod::Symlink);
        }
    }
    match link_tree(src, dest) {
        Ok(()) => Ok(CloneMethod::Hardlink),
        Err(_) => {
            let _ = std::fs::remove_dir_all(dest);
            copy_tree(src, dest)?;
            Ok(CloneMethod::Copy)
        }
    }
}

#[cfg(target_os = "macos")]
fn clonefile_dir(src: &Path, dest: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    extern "C" {
        fn clonefile(src: *const libc::c_char, dst: *const libc::c_char, flags: u32)
            -> libc::c_int;
    }
    const CLONE_NOFOLLOW: u32 = 0x0001;
    let s = CString::new(src.as_os_str().as_bytes())?;
    let d = CString::new(dest.as_os_str().as_bytes())?;
    // SAFETY: both strings are valid NUL-terminated paths for the call's duration.
    let rc = unsafe { clonefile(s.as_ptr(), d.as_ptr(), CLONE_NOFOLLOW) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn walk(
    src: &Path,
    dest: &Path,
    file_op: &dyn Fn(&Path, &Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    std::fs::create_dir_all(dest)?;
    #[cfg(unix)]
    {
        if let Ok(meta) = std::fs::metadata(src) {
            let _ = std::fs::set_permissions(dest, meta.permissions());
        }
    }
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == META_FILE {
            continue;
        }
        let from = entry.path();
        let to = dest.join(&name);
        let meta = std::fs::symlink_metadata(&from)?;
        if meta.is_symlink() {
            let target = std::fs::read_link(&from)?;
            #[cfg(unix)]
            std::os::unix::fs::symlink(&target, &to)?;
            #[cfg(windows)]
            {
                let _ = target;
                std::fs::copy(&from, &to)?;
            }
        } else if meta.is_dir() {
            walk(&from, &to, file_op)?;
        } else {
            file_op(&from, &to)?;
        }
    }
    Ok(())
}

fn link_tree(src: &Path, dest: &Path) -> std::io::Result<()> {
    walk(src, dest, &|f, t| std::fs::hard_link(f, t))
}

fn copy_tree(src: &Path, dest: &Path) -> std::io::Result<()> {
    walk(src, dest, &|f, t| std::fs::copy(f, t).map(|_| ()))
}

/// Remove a tree even when some entries are read-only.
pub fn remove_tree(path: &Path) -> std::io::Result<()> {
    if !path.exists() && !path.is_symlink() {
        return Ok(());
    }
    if path.is_symlink() || !path.is_dir() {
        return std::fs::remove_file(path);
    }
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(_) => {
            make_writable(path);
            std::fs::remove_dir_all(path)
        }
    }
}

fn make_writable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut stack = vec![path.to_path_buf()];
        while let Some(p) = stack.pop() {
            if let Ok(meta) = std::fs::symlink_metadata(&p) {
                if meta.is_dir() {
                    let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755));
                    if let Ok(rd) = std::fs::read_dir(&p) {
                        for e in rd.flatten() {
                            stack.push(e.path());
                        }
                    }
                } else if !meta.is_symlink() {
                    let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644));
                }
            }
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// Total size of a tree in bytes (hard links counted once).
pub fn tree_size(path: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    #[cfg(unix)]
    let mut seen = std::collections::HashSet::new();
    while let Some(p) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&p) else {
            continue;
        };
        for e in rd.flatten() {
            let Ok(meta) = e.metadata() else { continue };
            if meta.is_dir() {
                stack.push(e.path());
            } else if meta.is_file() {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    if !seen.insert((meta.dev(), meta.ino())) {
                        continue;
                    }
                }
                total += meta.len();
            }
        }
    }
    total
}

/// Store entries — `(sha256-or-name, metadata, directory)` — including
/// leftovers from interrupted builds and entries in the old flat layout.
pub fn entries(store: &Path) -> Vec<(String, Option<StoreMeta>, PathBuf)> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(store) else {
        return out;
    };
    for e in rd.flatten() {
        let p = e.path();
        if !p.is_dir() {
            continue;
        }
        let name = e.file_name().to_string_lossy().into_owned();
        if name.len() == 2 {
            if let Ok(inner) = std::fs::read_dir(&p) {
                for f in inner.flatten() {
                    if f.path().is_dir() {
                        let sha = f.file_name().to_string_lossy().into_owned();
                        let meta = std::fs::read_to_string(f.path().join(META_FILE))
                            .ok()
                            .and_then(|t| serde_json::from_str(&t).ok());
                        out.push((sha, meta, f.path()));
                    }
                }
            }
        } else {
            // pre-shard layout: store/<sha256>
            let meta = std::fs::read_to_string(p.join(META_FILE))
                .ok()
                .and_then(|t| serde_json::from_str(&t).ok());
            out.push((name, meta, p));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn materialize_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("store");
        let entry = entry_dir(&store, "abc");
        std::fs::create_dir_all(entry.join("bin")).unwrap();
        std::fs::write(entry.join("bin/tool"), "x").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("tool", entry.join("bin/alias")).unwrap();
        write_meta(
            &entry,
            &StoreMeta {
                name: "demo".into(),
                version: "1".into(),
                tag: "all".into(),
                sha256: "abc".into(),
                created_at: 1,
                relocated: false,
                pour_version: "0".into(),
            },
        )
        .unwrap();
        assert!(is_complete(&store, "abc"));
        assert_eq!(read_meta(&store, "abc").unwrap().name, "demo");

        let keg = dir.path().join("Cellar/demo/1");
        let method = materialize(&entry, &keg).unwrap();
        assert!(matches!(
            method,
            CloneMethod::Clonefile
                | CloneMethod::Symlink
                | CloneMethod::Hardlink
                | CloneMethod::Copy
        ));
        assert_eq!(std::fs::read_to_string(keg.join("bin/tool")).unwrap(), "x");
        assert!(method == CloneMethod::Symlink || !keg.join(META_FILE).exists());
        #[cfg(unix)]
        assert!(keg.join("bin/alias").is_symlink());
        assert_eq!(entries(&store).len(), 1);
        assert!(tree_size(&keg) >= 1);
        remove_tree(&keg).unwrap();
        assert!(!keg.exists());
        assert!(
            entry.join("bin/tool").exists(),
            "store survives keg removal"
        );
    }
}
