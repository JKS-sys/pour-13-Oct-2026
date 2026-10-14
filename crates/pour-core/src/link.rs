//! Linking a keg into the prefix the way Homebrew does: `opt/<name>`, then
//! `bin`, `sbin`, `include`, `lib`, `share`, `etc` and `Frameworks`. Whole
//! directories are symlinked when nothing else needs to share them (one
//! link for 5,000 man pages), while shared trees (`lib/pkgconfig`,
//! `share/man/man1`, …) are real directories with per-file links. When a
//! second keg needs a directory another keg owns as a symlink, that symlink
//! is expanded into a real directory so both can live there. Every link is
//! recorded so uninstall removes exactly those.

use crate::paths::Paths;
use crate::receipt::Receipt;
use std::path::{Path, PathBuf};

pub const LINK_DIRS: &[&str] = &[
    "etc",
    "bin",
    "sbin",
    "include",
    "lib",
    "share",
    "Frameworks",
];

#[derive(Debug, Default)]
pub struct LinkReport {
    /// Links made, relative to the prefix.
    pub linked: Vec<String>,
    /// Existing files or foreign links that were left alone: (path, owner).
    pub conflicts: Vec<(String, String)>,
    pub skipped: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Symlink the whole directory (or the file).
    Link,
    /// Make a real directory and descend.
    Mkpath,
    SkipDir,
    SkipFile,
}

const SHARE_MKPATH: &[&str] = &[
    "aclocal",
    "doc",
    "info",
    "java",
    "locale",
    "man",
    "applications",
    "gnome",
    "gnome/help",
    "icons",
    "mime-info",
    "pixmaps",
    "sounds",
    "postgresql",
    "zsh",
    "fish",
    "pwsh",
    "lua",
    "guile",
    "pypy",
    "bash-completion",
    "bash-completion/completions",
    "elisp",
    "emacs",
    "emacs/site-lisp",
    "fonts",
    "vim",
    "vim/vimfiles",
    "nvim",
    "nvim/site",
    "xml",
    "cmake",
    "glib-2.0",
    "glib-2.0/schemas",
    "mime",
    "metainfo",
    "appdata",
    "dbus-1",
    "dbus-1/services",
    "thumbnailers",
    "gtk-doc",
    "gtk-doc/html",
    "gir-1.0",
    "vala",
    "vala/vapi",
];

/// Homebrew's per-directory rules, keyed by the top-level dir and the
/// path relative to it (`man/man1`, `pkgconfig`, …).
fn mode_for(top: &str, rel: &str) -> Mode {
    match top {
        "etc" => Mode::Mkpath,
        "bin" | "sbin" => Mode::SkipDir,
        "include" => {
            if rel.starts_with("postgresql@") {
                Mode::Mkpath
            } else {
                Mode::Link
            }
        }
        "share" => {
            if rel == "info/dir"
                || rel == "locale/locale.alias"
                || (rel.starts_with("icons/") && rel.ends_with("/icon-theme.cache"))
            {
                return Mode::SkipFile;
            }
            let first = rel.split('/').next().unwrap_or("");
            if rel.starts_with("locale/")
                || rel.starts_with("icons/")
                || rel.starts_with("man/")
                || rel.starts_with("postgresql@")
                || SHARE_MKPATH.contains(&rel)
                || matches!(
                    first,
                    "zsh" | "fish" | "pwsh" | "lua" | "guile" | "pypy" | "icons" | "locale"
                )
            {
                Mode::Mkpath
            } else {
                Mode::Link
            }
        }
        "lib" => {
            if rel == "charset.alias" {
                return Mode::SkipFile;
            }
            let first = rel.split('/').next().unwrap_or("");
            let mk = matches!(
                first,
                "cps" | "pkgconfig" | "cmake" | "dtrace" | "ghc" | "php"
            ) || first.starts_with("gdk-pixbuf")
                || first.starts_with("gio")
                || first.starts_with("lua")
                || first.starts_with("mecab")
                || first.starts_with("node")
                || first.starts_with("ocaml")
                || first.starts_with("perl5")
                || first.starts_with("postgresql@")
                || first.starts_with("pypy")
                || (first.starts_with("python")
                    && first.len() > 6
                    && first.as_bytes()[6].is_ascii_digit())
                || first == "R"
                || first.starts_with("R/")
                || first.starts_with("ruby");
            if mk {
                Mode::Mkpath
            } else {
                Mode::Link
            }
        }
        "Frameworks" => {
            if rel.ends_with(".framework") || rel.ends_with(".framework/Versions") {
                Mode::Mkpath
            } else {
                Mode::Link
            }
        }
        _ => Mode::Link,
    }
}

fn skip_name(name: &str) -> bool {
    name == ".DS_Store"
        || name.ends_with(".pyc")
        || name.ends_with(".pyo")
        || name == ".pour-receipt.json"
        || name == "INSTALL_RECEIPT.json"
        || name == ".brew"
        || name == "sbom.spdx.json"
}

/// Relative symlink target from `from` (a path inside the prefix) to `to`.
pub fn relative_target(from: &Path, to: &Path) -> PathBuf {
    let from_dir = from.parent().unwrap_or(from);
    let fc: Vec<_> = from_dir.components().collect();
    let tc: Vec<_> = to.components().collect();
    let mut common = 0;
    while common < fc.len() && common < tc.len() && fc[common] == tc[common] {
        common += 1;
    }
    let mut out = PathBuf::new();
    for _ in common..fc.len() {
        out.push("..");
    }
    for c in &tc[common..] {
        out.push(c.as_os_str());
    }
    out
}

fn make_link(target: &Path, link: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
    }
    #[cfg(windows)]
    {
        if target.is_dir() {
            std::os::windows::fs::symlink_dir(target, link)
        } else {
            std::os::windows::fs::symlink_file(target, link)
        }
    }
}

pub fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Absolute, normalized target of a symlink.
fn link_target(link: &Path) -> Option<PathBuf> {
    let target = std::fs::read_link(link).ok()?;
    let abs = if target.is_absolute() {
        target
    } else {
        link.parent().unwrap_or(link).join(target)
    };
    Some(normalize(&abs))
}

/// Does the symlink at `link` point into `dir`?
fn points_into(link: &Path, dir: &Path) -> bool {
    link_target(link)
        .map(|t| t.starts_with(normalize(dir)))
        .unwrap_or(false)
}

/// `<cellar>/<name>/<version>/...` → (name, keg path).
fn keg_of(paths: &Paths, target: &Path) -> Option<(String, PathBuf)> {
    let rel = target.strip_prefix(normalize(&paths.cellar())).ok()?;
    let mut it = rel.components();
    let name = it.next()?.as_os_str().to_string_lossy().into_owned();
    let version = it.next()?.as_os_str().to_string_lossy().into_owned();
    let keg = paths.cellar().join(&name).join(version);
    Some((name, keg))
}

struct Linker<'a> {
    paths: &'a Paths,
    name: String,
    keg: PathBuf,
    cellar_name_dir: PathBuf,
    overwrite: bool,
    report: LinkReport,
}

impl Linker<'_> {
    fn conflict_owner(&self, dst: &Path) -> String {
        match link_target(dst) {
            Some(t) => keg_of(self.paths, &t)
                .map(|(n, _)| n)
                .unwrap_or_else(|| t.display().to_string()),
            None => "a file".to_string(),
        }
    }

    /// Replace a directory symlink owned by another keg with a real directory
    /// holding per-file links to that keg, and fix that keg's receipt.
    fn expand_dir_symlink(&mut self, dst: &Path, rel: &str) -> std::io::Result<()> {
        let Some(target) = link_target(dst) else {
            return Ok(());
        };
        std::fs::remove_file(dst)?;
        std::fs::create_dir_all(dst)?;
        let mut sub = LinkReport::default();
        if target.is_dir() {
            link_children_flat(self.paths, &target, dst, &mut sub)?;
        }
        // Repair the other keg's receipt: the directory link becomes its children.
        if let Some((other_name, other_keg)) = keg_of(self.paths, &target) {
            if other_name != self.name {
                if let Some(mut r) = Receipt::read(&other_keg) {
                    r.links.retain(|l| l != rel);
                    r.links.extend(sub.linked.iter().cloned());
                    let _ = r.write(&other_keg);
                }
            }
        }
        Ok(())
    }

    fn link_file(&mut self, src: &Path, dst: &Path, rel: &str) -> std::io::Result<()> {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if let Ok(existing) = std::fs::symlink_metadata(dst) {
            let ours = existing.is_symlink()
                && (points_into(dst, &self.cellar_name_dir) || std::fs::metadata(dst).is_err());
            if ours || self.overwrite {
                if existing.is_dir() && !existing.is_symlink() {
                    std::fs::remove_dir_all(dst)?;
                } else {
                    std::fs::remove_file(dst)?;
                }
            } else {
                let owner = self.conflict_owner(dst);
                self.report.conflicts.push((rel.to_string(), owner));
                return Ok(());
            }
        }
        make_link(&relative_target(dst, src), dst)?;
        self.report.linked.push(rel.to_string());
        Ok(())
    }

    fn link_dir(&mut self, top: &str, src_dir: &Path, rel_dir: &str) -> std::io::Result<()> {
        let Ok(rd) = std::fs::read_dir(src_dir) else {
            return Ok(());
        };
        let mut entries: Vec<_> = rd.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        // Big flat directories (man3 with thousands of pages): link in parallel.
        if entries.len() > 256 {
            use rayon::prelude::*;
            let files: Vec<_> = entries
                .iter()
                .filter(|e| {
                    std::fs::symlink_metadata(e.path())
                        .map(|m| !m.is_dir() || m.is_symlink())
                        .unwrap_or(false)
                })
                .collect();
            if files.len() > 256 {
                let dst_dir = self.paths.prefix.join(top).join(rel_dir);
                std::fs::create_dir_all(&dst_dir)?;
                let overwrite = self.overwrite;
                let cellar_name_dir = self.cellar_name_dir.clone();
                let results: Vec<Result<Option<String>, (String, String)>> = files
                    .par_iter()
                    .map(|e| {
                        let name = e.file_name().to_string_lossy().into_owned();
                        if skip_name(&name) {
                            return Ok(None);
                        }
                        let rel = if rel_dir.is_empty() {
                            name.clone()
                        } else {
                            format!("{rel_dir}/{name}")
                        };
                        if mode_for(top, &rel) == Mode::SkipFile {
                            return Ok(None);
                        }
                        let full_rel = format!("{top}/{rel}");
                        let src = e.path();
                        let dst = dst_dir.join(&name);
                        if let Ok(existing) = std::fs::symlink_metadata(&dst) {
                            let ours = existing.is_symlink()
                                && (points_into(&dst, &cellar_name_dir)
                                    || std::fs::metadata(&dst).is_err());
                            if ours || overwrite {
                                let _ = if existing.is_dir() && !existing.is_symlink() {
                                    std::fs::remove_dir_all(&dst)
                                } else {
                                    std::fs::remove_file(&dst)
                                };
                            } else {
                                let owner = link_target(&dst)
                                    .map(|t| t.display().to_string())
                                    .unwrap_or_else(|| "a file".into());
                                return Err((full_rel, owner));
                            }
                        }
                        match make_link(&relative_target(&dst, &src), &dst) {
                            Ok(()) => Ok(Some(full_rel)),
                            Err(e) => Err((full_rel, e.to_string())),
                        }
                    })
                    .collect();
                for r in results {
                    match r {
                        Ok(Some(rel)) => self.report.linked.push(rel),
                        Ok(None) => self.report.skipped += 1,
                        Err(c) => self.report.conflicts.push(c),
                    }
                }
                entries.retain(|e| {
                    std::fs::symlink_metadata(e.path())
                        .map(|m| m.is_dir() && !m.is_symlink())
                        .unwrap_or(false)
                });
            }
        }
        for e in entries {
            let name = e.file_name().to_string_lossy().into_owned();
            if skip_name(&name) {
                self.report.skipped += 1;
                continue;
            }
            let src = e.path();
            let rel = if rel_dir.is_empty() {
                name.clone()
            } else {
                format!("{rel_dir}/{name}")
            };
            let mode = mode_for(top, &rel);
            let full_rel = format!("{top}/{rel}");
            let dst = self.paths.prefix.join(&full_rel);
            let meta = std::fs::symlink_metadata(&src)?;
            if meta.is_dir() && !meta.is_symlink() {
                match mode {
                    Mode::SkipDir => {
                        self.report.skipped += 1;
                        continue;
                    }
                    Mode::SkipFile => {}
                    _ => {}
                }
                match std::fs::symlink_metadata(&dst) {
                    Ok(ex) if ex.is_symlink() => {
                        if points_into(&dst, &self.cellar_name_dir)
                            || std::fs::metadata(&dst).is_err()
                        {
                            // Our own older version (or dangling): replace.
                            std::fs::remove_file(&dst)?;
                            if mode == Mode::Link {
                                make_link(&relative_target(&dst, &src), &dst)?;
                                self.report.linked.push(full_rel);
                                continue;
                            }
                            std::fs::create_dir_all(&dst)?;
                        } else if std::fs::metadata(&dst).map(|m| m.is_dir()).unwrap_or(false) {
                            // Another keg owns this directory as one symlink: share it.
                            self.expand_dir_symlink(&dst, &full_rel)?;
                        } else {
                            let owner = self.conflict_owner(&dst);
                            self.report.conflicts.push((full_rel, owner));
                            continue;
                        }
                    }
                    Ok(ex) if ex.is_dir() => {}
                    Ok(_) => {
                        if self.overwrite {
                            std::fs::remove_file(&dst)?;
                        } else {
                            let owner = self.conflict_owner(&dst);
                            self.report.conflicts.push((full_rel, owner));
                            continue;
                        }
                    }
                    Err(_) => {}
                }
                if !dst.exists() {
                    if mode == Mode::Link {
                        if let Some(parent) = dst.parent() {
                            std::fs::create_dir_all(parent)?;
                        }
                        make_link(&relative_target(&dst, &src), &dst)?;
                        self.report.linked.push(full_rel);
                        continue;
                    }
                    std::fs::create_dir_all(&dst)?;
                }
                // Real directory in the prefix: descend.
                self.link_dir(top, &src, &rel)?;
            } else {
                if mode == Mode::SkipFile {
                    self.report.skipped += 1;
                    continue;
                }
                self.link_file(&src, &dst, &full_rel)?;
            }
        }
        Ok(())
    }
}

/// Link every entry below `src` into the real directory `dst` (used when a
/// directory symlink is expanded so two kegs can share it).
fn link_children_flat(
    paths: &Paths,
    src: &Path,
    dst: &Path,
    report: &mut LinkReport,
) -> std::io::Result<()> {
    let Ok(rd) = std::fs::read_dir(src) else {
        return Ok(());
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if skip_name(&name) {
            continue;
        }
        let s = e.path();
        let d = dst.join(&name);
        if d.exists() || d.is_symlink() {
            continue;
        }
        make_link(&relative_target(&d, &s), &d)?;
        report.linked.push(
            d.strip_prefix(&paths.prefix)
                .unwrap_or(&d)
                .to_string_lossy()
                .replace('\\', "/"),
        );
    }
    Ok(())
}

/// Create `opt/<name>` and link the keg's files into the prefix.
pub fn link_keg(
    paths: &Paths,
    name: &str,
    keg: &Path,
    overwrite: bool,
    link_files: bool,
) -> std::io::Result<LinkReport> {
    let mut report = LinkReport::default();
    let opt = paths.opt_link(name);
    std::fs::create_dir_all(paths.opt())?;
    if opt.is_symlink() || opt.exists() {
        let _ = if opt.is_dir() && !opt.is_symlink() {
            std::fs::remove_dir_all(&opt)
        } else {
            std::fs::remove_file(&opt)
        };
    }
    make_link(&relative_target(&opt, keg), &opt)?;
    report.linked.push(format!("opt/{name}"));
    if !link_files {
        return Ok(report);
    }
    let mut linker = Linker {
        paths,
        name: name.to_string(),
        keg: keg.to_path_buf(),
        cellar_name_dir: paths.cellar().join(name),
        overwrite,
        report,
    };
    for top in LINK_DIRS {
        let src_dir = linker.keg.join(top);
        if src_dir.is_dir() {
            std::fs::create_dir_all(paths.prefix.join(top))?;
            linker.link_dir(top, &src_dir, "")?;
        }
    }
    Ok(linker.report)
}

/// Remove the given prefix-relative links if they still point into the keg's
/// formula directory, then prune empty directories we created.
pub fn unlink_files(paths: &Paths, name: &str, links: &[String]) -> usize {
    use rayon::prelude::*;
    let cellar_name_dir = paths.cellar().join(name);
    let results: Vec<Option<PathBuf>> = links
        .par_iter()
        .map(|rel| {
            let p = paths.prefix.join(rel);
            if p.is_symlink()
                && (points_into(&p, &cellar_name_dir) || std::fs::metadata(&p).is_err())
                && std::fs::remove_file(&p).is_ok()
            {
                return Some(p.parent().map(|d| d.to_path_buf()).unwrap_or_default());
            }
            None
        })
        .collect();
    let removed = results.iter().filter(|r| r.is_some()).count();
    let mut dirs: Vec<PathBuf> = results.into_iter().flatten().collect();
    dirs.sort();
    dirs.dedup();
    dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
    for d in dirs {
        // Never remove the top-level prefix directories.
        if d.parent() == Some(paths.prefix.as_path()) || d == paths.prefix {
            continue;
        }
        let _ = std::fs::remove_dir(&d); // only succeeds when empty
    }
    removed
}

/// Dangling symlinks in the prefix's link directories (e.g. after a manual
/// deletion). Returns prefix-relative paths.
pub fn dangling_links(paths: &Paths) -> Vec<String> {
    let mut out = Vec::new();
    for dir in LINK_DIRS.iter().chain(["opt"].iter()) {
        let mut stack = vec![paths.prefix.join(dir)];
        while let Some(d) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&d) else {
                continue;
            };
            for e in rd.flatten() {
                let p = e.path();
                let Ok(meta) = std::fs::symlink_metadata(&p) else {
                    continue;
                };
                if meta.is_symlink() {
                    if std::fs::metadata(&p).is_err() {
                        out.push(
                            p.strip_prefix(&paths.prefix)
                                .unwrap_or(&p)
                                .to_string_lossy()
                                .replace('\\', "/"),
                        );
                    }
                } else if meta.is_dir() {
                    stack.push(p);
                }
            }
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_targets() {
        assert_eq!(
            relative_target(
                Path::new("/opt/pour/bin/jq"),
                Path::new("/opt/pour/Cellar/jq/1.8/bin/jq")
            ),
            PathBuf::from("../Cellar/jq/1.8/bin/jq")
        );
        assert_eq!(
            relative_target(
                Path::new("/opt/pour/opt/jq"),
                Path::new("/opt/pour/Cellar/jq/1.8")
            ),
            PathBuf::from("../Cellar/jq/1.8")
        );
        assert_eq!(
            relative_target(
                Path::new("/opt/pour/lib/pkgconfig/a.pc"),
                Path::new("/opt/pour/Cellar/a/1/lib/pkgconfig/a.pc")
            ),
            PathBuf::from("../../Cellar/a/1/lib/pkgconfig/a.pc")
        );
    }

    #[test]
    fn modes_follow_homebrew() {
        assert_eq!(mode_for("share", "man/man1"), Mode::Mkpath);
        assert_eq!(mode_for("share", "doc"), Mode::Mkpath);
        assert_eq!(mode_for("share", "jq"), Mode::Link);
        assert_eq!(mode_for("lib", "pkgconfig"), Mode::Mkpath);
        assert_eq!(mode_for("lib", "python3.14"), Mode::Mkpath);
        assert_eq!(mode_for("lib", "libfoo.so"), Mode::Link);
        assert_eq!(mode_for("lib", "charset.alias"), Mode::SkipFile);
        assert_eq!(mode_for("bin", "sub"), Mode::SkipDir);
        assert_eq!(mode_for("include", "openssl"), Mode::Link);
        assert_eq!(mode_for("etc", "anything"), Mode::Mkpath);
    }

    #[cfg(unix)]
    #[test]
    fn link_unlink_cycle_with_conflicts() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path().join("pour"));
        paths.ensure().unwrap();
        let keg = paths.keg("demo", "1.0");
        std::fs::create_dir_all(keg.join("bin")).unwrap();
        std::fs::create_dir_all(keg.join("lib/pkgconfig")).unwrap();
        std::fs::create_dir_all(keg.join("share/man/man1")).unwrap();
        std::fs::create_dir_all(keg.join("share/demo/data")).unwrap();
        std::fs::create_dir_all(keg.join("include/demo")).unwrap();
        std::fs::write(keg.join("bin/demo"), "x").unwrap();
        std::fs::write(keg.join("lib/libdemo.so"), "x").unwrap();
        std::fs::write(keg.join("lib/pkgconfig/demo.pc"), "x").unwrap();
        std::fs::write(keg.join("share/man/man1/demo.1"), "x").unwrap();
        std::fs::write(keg.join("share/demo/data/a.txt"), "x").unwrap();
        std::fs::write(keg.join("include/demo/demo.h"), "x").unwrap();
        std::fs::write(keg.join("INSTALL_RECEIPT.json"), "{}").unwrap();
        std::fs::write(paths.bin().join("demo"), "someone else").unwrap();

        let r = link_keg(&paths, "demo", &keg, false, true).unwrap();
        assert_eq!(r.conflicts.len(), 1);
        assert_eq!(r.conflicts[0].0, "bin/demo");
        assert!(
            r.linked.contains(&"lib/pkgconfig/demo.pc".to_string()),
            "pkgconfig is mkpath"
        );
        assert!(
            r.linked.contains(&"share/demo".to_string()),
            "share/demo is one directory link"
        );
        assert!(!r.linked.contains(&"share/demo/data/a.txt".to_string()));
        assert!(r.linked.contains(&"include/demo".to_string()));
        assert!(r.linked.contains(&"share/man/man1/demo.1".to_string()));
        assert!(paths.prefix.join("share/demo").is_symlink());
        assert_eq!(
            std::fs::read_link(paths.opt_link("demo")).unwrap(),
            PathBuf::from("../Cellar/demo/1.0")
        );

        // overwrite replaces the foreign file
        let r = link_keg(&paths, "demo", &keg, true, true).unwrap();
        assert!(r.conflicts.is_empty());
        assert!(paths.bin().join("demo").is_symlink());
        Receipt {
            name: "demo".into(),
            version: "1.0".into(),
            links: r.linked.clone(),
            ..Default::default()
        }
        .write(&keg)
        .unwrap();

        // a second keg needs include/demo too: the directory link is expanded
        let keg2 = paths.keg("other", "2.0");
        std::fs::create_dir_all(keg2.join("include/demo")).unwrap();
        std::fs::write(keg2.join("include/demo/other.h"), "y").unwrap();
        let r2 = link_keg(&paths, "other", &keg2, false, true).unwrap();
        assert!(r2.conflicts.is_empty(), "{:?}", r2.conflicts);
        assert!(
            paths.prefix.join("include/demo").is_dir()
                && !paths.prefix.join("include/demo").is_symlink()
        );
        assert!(paths.prefix.join("include/demo/demo.h").is_symlink());
        assert!(paths.prefix.join("include/demo/other.h").is_symlink());
        let fixed = Receipt::read(&keg).unwrap();
        assert!(
            fixed.links.contains(&"include/demo/demo.h".to_string())
                && !fixed.links.contains(&"include/demo".to_string()),
            "{:?}",
            fixed.links
        );

        // upgrade: a new keg version replaces links of the old one silently
        let keg3 = paths.keg("demo", "3.0");
        std::fs::create_dir_all(keg3.join("bin")).unwrap();
        std::fs::create_dir_all(keg3.join("share/demo")).unwrap();
        std::fs::write(keg3.join("bin/demo"), "z").unwrap();
        std::fs::write(keg3.join("share/demo/b.txt"), "z").unwrap();
        let r3 = link_keg(&paths, "demo", &keg3, false, true).unwrap();
        assert!(r3.conflicts.is_empty(), "{:?}", r3.conflicts);
        assert_eq!(
            std::fs::read_to_string(paths.bin().join("demo")).unwrap(),
            "z"
        );
        assert!(paths.prefix.join("share/demo/b.txt").exists());

        let removed = unlink_files(&paths, "demo", &r3.linked);
        assert!(removed >= 2);
        assert!(!paths.bin().join("demo").exists());
        assert!(!paths.prefix.join("share/demo").exists());
        assert!(paths.bin().is_dir(), "top-level dirs survive");
        std::fs::remove_dir_all(&keg).unwrap();
        assert!(dangling_links(&paths).contains(&"lib/libdemo.so".to_string()));
    }
}
