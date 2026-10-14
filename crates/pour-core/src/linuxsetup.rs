//! Linux specifics Homebrew bottles rely on: the `lib/ld.so` dynamic-loader
//! symlink every patched executable points at, GCC runtime libraries in
//! `lib/`, and a glibc-age check.

use crate::paths::Paths;
use crate::platform::Arch;
use std::path::{Path, PathBuf};

pub const SYSTEM_LOADERS_X86_64: &[&str] = &[
    "/lib64/ld-linux-x86-64.so.2",
    "/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2",
    "/system/bin/linker64",
];
pub const SYSTEM_LOADERS_ARM64: &[&str] = &[
    "/lib/ld-linux-aarch64.so.1",
    "/lib/aarch64-linux-gnu/ld-linux-aarch64.so.1",
    "/system/bin/linker64",
];

pub const GCC_RUNTIME_LIBS: &[&str] = &[
    "libatomic.so.1",
    "libgcc_s.so.1",
    "libgomp.so.1",
    "libstdc++.so.6",
];

pub fn system_loader(arch: Arch) -> Option<PathBuf> {
    let list = match arch {
        Arch::Arm64 => SYSTEM_LOADERS_ARM64,
        _ => SYSTEM_LOADERS_X86_64,
    };
    list.iter().map(PathBuf::from).find(|p| p.exists())
}

/// Point `lib/ld.so` at Pour's glibc if installed, else the system loader.
pub fn ensure_ld_so(paths: &Paths, arch: Arch) -> std::io::Result<Option<PathBuf>> {
    let lib = paths.prefix.join("lib");
    std::fs::create_dir_all(&lib)?;
    let link = lib.join("ld.so");
    let brewed = paths.opt_link("glibc").join("bin/ld.so");
    let target = if brewed.exists() {
        Some(brewed)
    } else {
        system_loader(arch)
    };
    let Some(target) = target else {
        return Ok(None);
    };
    if let Ok(cur) = std::fs::read_link(&link) {
        if cur == target {
            return Ok(Some(target));
        }
        std::fs::remove_file(&link)?;
    } else if link.exists() {
        std::fs::remove_file(&link)?;
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(&target, &link)?;
    Ok(Some(target))
}

/// Symlink GCC's runtime libraries into `lib/` when glibc is not Pour's
/// (Homebrew does the same), so C++ bottles find `libstdc++.so.6`.
pub fn ensure_gcc_runtime(paths: &Paths) -> std::io::Result<usize> {
    let gcc = paths.opt_link("gcc");
    if !gcc.exists() {
        return Ok(0);
    }
    let brewed_glibc = paths.opt_link("glibc").join("bin/ld.so").exists();
    let mut n = 0;
    for lib in GCC_RUNTIME_LIBS {
        let link = paths.prefix.join("lib").join(lib);
        if brewed_glibc {
            if link.is_symlink() {
                let _ = std::fs::remove_file(&link);
            }
            continue;
        }
        let target = gcc.join("lib/gcc/current").join(lib);
        if !target.exists() {
            continue;
        }
        if std::fs::read_link(&link)
            .map(|t| t == target)
            .unwrap_or(false)
        {
            continue;
        }
        if link.exists() || link.is_symlink() {
            let _ = std::fs::remove_file(&link);
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link)?;
        n += 1;
    }
    Ok(n)
}

/// Host glibc version from `ldd --version` or the loader, e.g. (2, 39).
pub fn host_glibc() -> Option<(u32, u32)> {
    let out = std::process::Command::new("ldd")
        .arg("--version")
        .output()
        .ok()?;
    let text =
        String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    parse_glibc(&text)
}

pub fn parse_glibc(text: &str) -> Option<(u32, u32)> {
    let first = text.lines().next()?;
    let v = first.split_whitespace().last()?;
    let mut it = v.split('.');
    Some((
        it.next()?.parse().ok()?,
        it.next()?
            .split(|c: char| !c.is_ascii_digit())
            .next()?
            .parse()
            .ok()?,
    ))
}

/// Homebrew's current bottles are built against this glibc.
pub const MIN_GLIBC: (u32, u32) = (2, 39);

pub fn glibc_too_old() -> bool {
    matches!(host_glibc(), Some(v) if v < MIN_GLIBC)
}

pub fn is_linux_prefix_ready(paths: &Paths) -> bool {
    paths.prefix.join("lib/ld.so").exists() || Path::new("/usr/lib64").exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ldd_output() {
        assert_eq!(
            parse_glibc("ldd (Ubuntu GLIBC 2.39-0ubuntu8.4) 2.39\nCopyright"),
            Some((2, 39))
        );
        assert_eq!(parse_glibc("ldd (GNU libc) 2.35"), Some((2, 35)));
        assert!(parse_glibc("").is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ld_so_symlink_points_at_a_loader() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path().join("pour"));
        paths.ensure().unwrap();
        let t = ensure_ld_so(&paths, Arch::X86_64).unwrap();
        if let Some(t) = t {
            assert_eq!(
                std::fs::read_link(paths.prefix.join("lib/ld.so")).unwrap(),
                t
            );
            // idempotent
            ensure_ld_so(&paths, Arch::X86_64).unwrap();
        }
    }
}
