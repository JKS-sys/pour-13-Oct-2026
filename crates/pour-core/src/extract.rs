//! Unpacking bottles (`.tar.gz`) into a directory, dropping the leading
//! `name/version/` the archive carries, with permissions and symlinks kept.

use flate2::read::MultiGzDecoder;
use std::fs::File;
use std::io::BufReader;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum ExtractError {
    #[error("archive entry escapes the destination: {0}")]
    Escape(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Unpack `archive` into `dest`, removing the first `strip` path components
/// of every entry. Returns the number of entries written.
pub fn untar_gz(archive: &Path, dest: &Path, strip: usize) -> Result<usize, ExtractError> {
    let file = File::open(archive)?;
    let gz = MultiGzDecoder::new(BufReader::with_capacity(1 << 20, file));
    let mut tar = tar::Archive::new(gz);
    tar.set_preserve_permissions(true);
    tar.set_preserve_mtime(true);
    tar.set_unpack_xattrs(false);
    std::fs::create_dir_all(dest)?;
    let mut count = 0usize;
    let mut delayed_dir_perms: Vec<(PathBuf, u32)> = Vec::new();
    for entry in tar.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let stripped: PathBuf = path.components().skip(strip).collect();
        if stripped.as_os_str().is_empty() {
            continue;
        }
        if stripped.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        }) {
            return Err(ExtractError::Escape(path.display().to_string()));
        }
        let target = dest.join(&stripped);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let kind = entry.header().entry_type();
        if kind.is_dir() {
            std::fs::create_dir_all(&target)?;
            if let Ok(mode) = entry.header().mode() {
                delayed_dir_perms.push((target.clone(), mode));
            }
            count += 1;
            continue;
        }
        // A file replacing an earlier symlink or read-only file must not be
        // blocked by it.
        if let Ok(meta) = std::fs::symlink_metadata(&target) {
            if meta.is_dir() && !kind.is_dir() {
                std::fs::remove_dir_all(&target)?;
            } else if !meta.is_dir() {
                std::fs::remove_file(&target)?;
            }
        }
        entry.set_preserve_permissions(true);
        if !entry.unpack(&target).map(|_| true).unwrap_or(false) {
            // tar's unpack failed softly (e.g. hard link whose source is
            // missing); fall back to copying the content.
            if kind.is_file() {
                let mut f = File::create(&target)?;
                std::io::copy(&mut entry, &mut f)?;
            }
        }
        count += 1;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for (dir, mode) in delayed_dir_perms {
            // Keep directories writable for the owner so later relocation and
            // uninstall can work; Homebrew does the same.
            let _ = std::fs::set_permissions(
                &dir,
                std::fs::Permissions::from_mode((mode | 0o200) & 0o7777),
            );
        }
    }
    Ok(count)
}

/// Is `path` a gzip file (by magic)?
pub fn is_gzip(path: &Path) -> bool {
    let mut buf = [0u8; 2];
    File::open(path)
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut buf))
        .map(|_| buf == [0x1f, 0x8b])
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::GzEncoder;
    use flate2::Compression;

    fn make_bottle(dir: &Path) -> PathBuf {
        let archive = dir.join("demo-1.0.bottle.tar.gz");
        let gz = GzEncoder::new(File::create(&archive).unwrap(), Compression::fast());
        let mut tar = tar::Builder::new(gz);
        let mut add = |path: &str, data: &[u8], mode: u32| {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(mode);
            h.set_cksum();
            tar.append_data(&mut h, path, data).unwrap();
        };
        add("demo/1.0/bin/demo", b"#!/bin/sh\necho hi\n", 0o755);
        add("demo/1.0/README", b"readme", 0o644);
        add(
            "demo/1.0/lib/libdemo.1.dylib",
            b"\xcf\xfa\xed\xfe fake",
            0o644,
        );
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Symlink);
        h.set_size(0);
        h.set_mode(0o777);
        h.set_cksum();
        tar.append_link(&mut h, "demo/1.0/lib/libdemo.dylib", "libdemo.1.dylib")
            .unwrap();
        tar.into_inner().unwrap().finish().unwrap();
        archive
    }

    #[test]
    fn strips_two_components_and_keeps_links() {
        let dir = tempfile::tempdir().unwrap();
        let archive = make_bottle(dir.path());
        assert!(is_gzip(&archive));
        let dest = dir.path().join("keg");
        let n = untar_gz(&archive, &dest, 2).unwrap();
        assert_eq!(n, 4);
        assert_eq!(
            std::fs::read_to_string(dest.join("README")).unwrap(),
            "readme"
        );
        assert!(dest.join("lib/libdemo.dylib").is_symlink());
        assert_eq!(
            std::fs::read_link(dest.join("lib/libdemo.dylib")).unwrap(),
            PathBuf::from("libdemo.1.dylib")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(dest.join("bin/demo"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o755
            );
        }
    }

    #[test]
    fn rejects_escapes() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("evil.tar.gz");
        let gz = GzEncoder::new(File::create(&archive).unwrap(), Compression::fast());
        let mut tar = tar::Builder::new(gz);
        let mut h = tar::Header::new_gnu();
        h.set_size(1);
        h.set_mode(0o644);
        // The tar crate refuses `..` through set_path, so write the raw name
        // field the way a hostile archive would.
        let evil = b"a/b/../../../etc/passwd";
        h.as_gnu_mut().unwrap().name[..evil.len()].copy_from_slice(evil);
        h.set_cksum();
        tar.append(&h, &b"x"[..]).unwrap();
        tar.into_inner().unwrap().finish().unwrap();
        assert!(matches!(
            untar_gz(&archive, &dir.path().join("out"), 2),
            Err(ExtractError::Escape(_))
        ));
    }
}
