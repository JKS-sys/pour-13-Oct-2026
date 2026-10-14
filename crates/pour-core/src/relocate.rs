//! Relocation: make a Homebrew bottle run from Pour's prefix.
//!
//! Bottles carry `@@HOMEBREW_PREFIX@@`-style placeholders (relocatable
//! bottles) or Homebrew's real build prefix (bottles pinned to one cellar).
//! Pour rewrites both, in text files, Mach-O load commands and ELF
//! interpreter/rpath strings, all in process and in parallel, then re-signs
//! patched Mach-O files once per file.

use crate::elf;
use crate::macho;
use crate::paths::Paths;
use crate::platform::{Os, Platform};
use memchr::memmem;
use rayon::prelude::*;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub const PREFIX_PLACEHOLDER: &str = "@@HOMEBREW_PREFIX@@";
pub const CELLAR_PLACEHOLDER: &str = "@@HOMEBREW_CELLAR@@";
pub const REPOSITORY_PLACEHOLDER: &str = "@@HOMEBREW_REPOSITORY@@";
pub const LIBRARY_PLACEHOLDER: &str = "@@HOMEBREW_LIBRARY@@";
pub const PERL_PLACEHOLDER: &str = "@@HOMEBREW_PERL@@";
pub const JAVA_PLACEHOLDER: &str = "@@HOMEBREW_JAVA@@";

/// Ordered search/replace pairs with precompiled finders.
pub struct Replacer {
    pairs: Vec<(String, String)>,
    finders: Vec<memmem::Finder<'static>>,
}

impl Replacer {
    /// Longer patterns are applied first so `/opt/homebrew/Cellar` wins over
    /// `/opt/homebrew`.
    pub fn new(mut pairs: Vec<(String, String)>) -> Replacer {
        pairs.retain(|(a, b)| !a.is_empty() && a != b);
        pairs.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
        pairs.dedup_by(|a, b| a.0 == b.0);
        let finders = pairs
            .iter()
            .map(|(a, _)| memmem::Finder::new(a.as_bytes()).into_owned())
            .collect();
        Replacer { pairs, finders }
    }

    pub fn pairs(&self) -> &[(String, String)] {
        &self.pairs
    }

    /// Does any pattern occur in `data`?
    pub fn matches(&self, data: &[u8]) -> bool {
        self.finders.iter().any(|f| f.find(data).is_some())
    }

    /// Apply every pair to a string; `None` when nothing changed.
    pub fn replace_str(&self, s: &str) -> Option<String> {
        let mut out = s.to_string();
        let mut changed = false;
        for (a, b) in &self.pairs {
            if out.contains(a.as_str()) {
                out = out.replace(a, b);
                changed = true;
            }
        }
        changed.then_some(out)
    }

    /// Apply every pair to raw bytes (text files). Returns the new bytes when
    /// something changed.
    pub fn replace_bytes(&self, data: &[u8]) -> Option<Vec<u8>> {
        let mut cur: Vec<u8> = data.to_vec();
        let mut changed = false;
        for (i, (a, b)) in self.pairs.iter().enumerate() {
            let f = &self.finders[i];
            if f.find(&cur).is_none() {
                continue;
            }
            let mut out = Vec::with_capacity(cur.len());
            let mut last = 0;
            for pos in f.find_iter(&cur) {
                out.extend_from_slice(&cur[last..pos]);
                out.extend_from_slice(b.as_bytes());
                last = pos + a.len();
            }
            out.extend_from_slice(&cur[last..]);
            cur = out;
            changed = true;
        }
        changed.then_some(cur)
    }

    /// Rewrite NUL-terminated C strings inside a binary in place. Only used
    /// for bottles pinned to a fixed cellar, whose binaries embed real paths.
    /// Each replacement is shorter than the original, so the tail of the
    /// string is shifted left and the gap NUL padded.
    pub fn patch_cstrings(&self, data: &mut [u8]) -> usize {
        let mut count = 0;
        for (i, (a, b)) in self.pairs.iter().enumerate() {
            if b.len() > a.len() {
                continue;
            }
            let f = &self.finders[i];
            let mut from = 0;
            while let Some(pos) = f.find(&data[from..]) {
                let pos = from + pos;
                let end = pos
                    + data[pos..]
                        .iter()
                        .position(|&x| x == 0)
                        .unwrap_or(data.len() - pos);
                let mut start = pos;
                while start > 0 && data[start - 1] != 0 && pos - start < 4096 {
                    start -= 1;
                }
                let original = data[start..end].to_vec();
                let replaced = match self.replace_bytes(&original) {
                    Some(r) if r.len() <= original.len() => r,
                    _ => {
                        from = end;
                        continue;
                    }
                };
                data[start..end].fill(0);
                data[start..start + replaced.len()].copy_from_slice(&replaced);
                count += 1;
                from = end;
                if from >= data.len() {
                    break;
                }
            }
        }
        count
    }
}

/// Everything needed to relocate one keg.
pub struct Relocation {
    pub replacer: Replacer,
    pub prefix: String,
    pub platform: Platform,
    /// Bottle was built for one exact cellar: also patch embedded C strings.
    pub deep: bool,
    /// Only these files (relative to the keg) carry placeholders, per the
    /// bottle's manifest. `None` means scan every file.
    pub files: Option<Vec<String>>,
    /// Linux: interpreter to set on executables.
    pub interpreter: Option<String>,
}

impl Relocation {
    /// Build the replacement table for a formula being poured into `paths`.
    pub fn for_formula(
        platform: &Platform,
        paths: &Paths,
        deps: &[String],
        deep: bool,
    ) -> Relocation {
        let prefix = paths.prefix_str();
        let cellar = paths.cellar().to_string_lossy().into_owned();
        let mut pairs = vec![
            (CELLAR_PLACEHOLDER.to_string(), cellar.clone()),
            (PREFIX_PLACEHOLDER.to_string(), prefix.clone()),
            (REPOSITORY_PLACEHOLDER.to_string(), prefix.clone()),
            (LIBRARY_PLACEHOLDER.to_string(), format!("{prefix}/Library")),
        ];
        // Perl: Homebrew's perl if installed, else the system one on macOS.
        let brewed_perl = paths.opt_link("perl").join("bin/perl");
        let perl = if brewed_perl.exists() || platform.os != Os::MacOs {
            brewed_perl.to_string_lossy().into_owned()
        } else {
            "/usr/bin/perl".to_string()
        };
        pairs.push((PERL_PLACEHOLDER.to_string(), perl));
        let openjdk = deps
            .iter()
            .find(|d| d.starts_with("openjdk"))
            .cloned()
            .unwrap_or_else(|| "openjdk".to_string());
        pairs.push((
            JAVA_PLACEHOLDER.to_string(),
            format!("{prefix}/opt/{openjdk}/libexec"),
        ));
        // Real build prefixes, for bottles pinned to a cellar and for any
        // stray path a relocatable bottle still carries.
        for build in ["/opt/homebrew", "/usr/local", "/home/linuxbrew/.linuxbrew"] {
            if build == "/usr/local" && platform.os != Os::MacOs {
                continue; // /usr/local is a real system path on Linux
            }
            if build == "/usr/local" && !deep {
                continue; // too generic to rewrite in relocatable bottles
            }
            if build.len() >= prefix.len() {
                pairs.push((format!("{build}/Cellar"), cellar.clone()));
                pairs.push((build.to_string(), prefix.clone()));
            }
        }
        let interpreter = if platform.os == Os::Linux {
            Some(format!("{prefix}/lib/ld.so"))
        } else {
            None
        };
        Relocation {
            replacer: Replacer::new(pairs),
            prefix,
            platform: platform.clone(),
            deep,
            files: None,
            interpreter,
        }
    }
}

#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct Report {
    pub text_files: Vec<String>,
    /// Symlinks whose target was re-pointed from a placeholder.
    pub symlinks: Vec<String>,
    pub macho_files: Vec<String>,
    pub elf_files: Vec<String>,
    pub deep_patched: Vec<String>,
    pub codesigned: usize,
    pub warnings: Vec<String>,
}

impl Report {
    pub fn changed(&self) -> usize {
        self.text_files.len()
            + self.symlinks.len()
            + self.macho_files.len()
            + self.elf_files.len()
            + self.deep_patched.len()
    }
}

fn looks_like_text(data: &[u8]) -> bool {
    let head = &data[..data.len().min(8192)];
    !head.contains(&0)
}

fn looks_like_rust_or_go(data: &[u8]) -> bool {
    memmem::find(data, b"rust_eh_personality").is_some()
        || memmem::find(data, b"__rust_alloc").is_some()
        || memmem::find(data, b".gopclntab").is_some()
        || memmem::find(data, b"go:buildid").is_some()
}

/// Write `data` over `path` in place (keeping the inode, so hard links stay
/// in step), temporarily adding write permission when needed.
fn write_in_place(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let meta = std::fs::symlink_metadata(path)?;
    let perms = meta.permissions();
    let readonly = perms.readonly();
    if readonly {
        let mut p = perms.clone();
        #[allow(clippy::permissions_set_readonly_false)]
        p.set_readonly(false);
        std::fs::set_permissions(path, p)?;
    }
    let result = (|| {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(path)?;
        f.write_all(data)?;
        f.flush()
    })();
    if readonly {
        std::fs::set_permissions(path, perms)?;
    }
    result
}

/// Collect candidate files: the manifest's list when known, else every
/// regular file in the keg (hard links counted once).
fn candidates(keg: &Path, files: &Option<Vec<String>>) -> Vec<PathBuf> {
    if let Some(list) = files {
        return list
            .iter()
            .map(|f| keg.join(f))
            .filter(|p| p.is_file() && !p.is_symlink())
            .collect();
    }
    let mut out = Vec::new();
    let mut seen: HashSet<(u64, u64)> = HashSet::new();
    let mut stack = vec![keg.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let p = entry.path();
            let Ok(meta) = std::fs::symlink_metadata(&p) else {
                continue;
            };
            if meta.is_symlink() {
                continue;
            }
            if meta.is_dir() {
                stack.push(p);
            } else if meta.is_file() {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    if !seen.insert((meta.dev(), meta.ino())) {
                        continue;
                    }
                }
                let _ = &mut seen;
                out.push(p);
            }
        }
    }
    out
}

/// Every symlink inside `keg`.
#[cfg(unix)]
fn symlinks(keg: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![keg.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let p = entry.path();
            let Ok(meta) = std::fs::symlink_metadata(&p) else {
                continue;
            };
            if meta.is_symlink() {
                out.push(p);
            } else if meta.is_dir() {
                stack.push(p);
            }
        }
    }
    out
}

/// Relocate every file of `keg` according to `rel`.
pub fn relocate_keg(keg: &Path, rel: &Relocation) -> Report {
    let files = candidates(keg, &rel.files);
    let report = Mutex::new(Report::default());
    let to_sign: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
    let rel_name = |p: &Path| {
        p.strip_prefix(keg)
            .unwrap_or(p)
            .to_string_lossy()
            .into_owned()
    };

    files.par_iter().for_each(|path| {
        let data = match std::fs::read(path) {
            Ok(d) => d,
            Err(e) => {
                report
                    .lock()
                    .unwrap()
                    .warnings
                    .push(format!("{}: {e}", rel_name(path)));
                return;
            }
        };
        if data.is_empty() || !rel.replacer.matches(&data) {
            return;
        }
        let name = rel_name(path);
        if macho::is_macho(&data) {
            let mut buf = data;
            let mut touched = false;
            match macho::patch(&mut buf, &rel.replacer) {
                Ok(p) if p.load_commands > 0 => touched = true,
                Ok(_) => {}
                Err(e) => report.lock().unwrap().warnings.push(format!("{name}: {e}")),
            }
            if rel.deep && !looks_like_rust_or_go(&buf) && rel.replacer.patch_cstrings(&mut buf) > 0
            {
                touched = true;
                report.lock().unwrap().deep_patched.push(name.clone());
            }
            if touched {
                match write_in_place(path, &buf) {
                    Ok(()) => {
                        report.lock().unwrap().macho_files.push(name);
                        to_sign.lock().unwrap().push(path.clone());
                    }
                    Err(e) => report.lock().unwrap().warnings.push(format!("{name}: {e}")),
                }
            }
        } else if elf::is_elf(&data) {
            let mut buf = data;
            let mut touched = false;
            match elf::patch(
                &mut buf,
                &rel.replacer,
                &rel.prefix,
                rel.interpreter.as_deref(),
            ) {
                Ok(p) if p.interpreter.is_some() || !p.rpaths.is_empty() => touched = true,
                Ok(_) => {}
                Err(e) => report.lock().unwrap().warnings.push(format!("{name}: {e}")),
            }
            if rel.deep && !looks_like_rust_or_go(&buf) && rel.replacer.patch_cstrings(&mut buf) > 0
            {
                touched = true;
                report.lock().unwrap().deep_patched.push(name.clone());
            }
            if touched {
                match write_in_place(path, &buf) {
                    Ok(()) => report.lock().unwrap().elf_files.push(name),
                    Err(e) => report.lock().unwrap().warnings.push(format!("{name}: {e}")),
                }
            }
        } else if looks_like_text(&data) {
            if let Some(new) = rel.replacer.replace_bytes(&data) {
                match write_in_place(path, &new) {
                    Ok(()) => report.lock().unwrap().text_files.push(name),
                    Err(e) => report.lock().unwrap().warnings.push(format!("{name}: {e}")),
                }
            }
        } else if rel.deep {
            let mut buf = data;
            if !looks_like_rust_or_go(&buf) && rel.replacer.patch_cstrings(&mut buf) > 0 {
                match write_in_place(path, &buf) {
                    Ok(()) => report.lock().unwrap().deep_patched.push(name),
                    Err(e) => report.lock().unwrap().warnings.push(format!("{name}: {e}")),
                }
            }
        }
    });

    let mut report = report.into_inner().unwrap();
    // Symlinks whose target names the placeholder prefix (python's
    // pip/_vendor/certifi/cacert.pem → @@HOMEBREW_PREFIX@@/etc/ca-certificates/cert.pem)
    // are re-pointed, as Homebrew does; relative links are left alone.
    #[cfg(unix)]
    for link in symlinks(keg) {
        let Ok(target) = std::fs::read_link(&link) else {
            continue;
        };
        let t = target.to_string_lossy();
        if !t.starts_with('/') && !t.contains("@@") {
            continue;
        }
        if let Some(new) = rel.replacer.replace_str(&t) {
            let name = rel_name(&link);
            let res =
                std::fs::remove_file(&link).and_then(|_| std::os::unix::fs::symlink(&new, &link));
            match res {
                Ok(()) => report.symlinks.push(name),
                Err(e) => report.warnings.push(format!("{name}: {e}")),
            }
        }
    }
    let to_sign = to_sign.into_inner().unwrap();
    if rel.platform.os == Os::MacOs && !to_sign.is_empty() {
        let (signed, warnings) = codesign(&to_sign);
        report.codesigned = signed;
        report.warnings.extend(warnings);
    }
    report.text_files.sort();
    report.macho_files.sort();
    report.elf_files.sort();
    report.deep_patched.sort();
    report
}

/// Ad-hoc re-sign patched Mach-O files, 32 per `codesign` call, in parallel.
pub fn codesign(files: &[PathBuf]) -> (usize, Vec<String>) {
    let codesign = Path::new("/usr/bin/codesign");
    if !codesign.exists() {
        return (
            0,
            vec!["codesign not found; patched binaries are unsigned".into()],
        );
    }
    let results: Vec<(usize, Vec<String>)> = files
        .par_chunks(32)
        .map(|chunk| {
            let run = |preserve: bool| {
                let mut cmd = std::process::Command::new(codesign);
                cmd.arg("--sign").arg("-").arg("--force");
                if preserve {
                    cmd.arg("--preserve-metadata=entitlements,requirements,flags,runtime");
                }
                cmd.args(chunk);
                cmd.output()
            };
            match run(true) {
                Ok(o) if o.status.success() => (chunk.len(), Vec::new()),
                _ => match run(false) {
                    Ok(o) if o.status.success() => (chunk.len(), Vec::new()),
                    Ok(o) => (
                        0,
                        vec![format!(
                            "codesign failed: {}",
                            String::from_utf8_lossy(&o.stderr).trim()
                        )],
                    ),
                    Err(e) => (0, vec![format!("codesign failed: {e}")]),
                },
            }
        })
        .collect();
    let mut n = 0;
    let mut warnings = Vec::new();
    for (c, w) in results {
        n += c;
        warnings.extend(w);
    }
    (n, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::Arch;

    fn rep() -> Replacer {
        Replacer::new(vec![
            ("@@HOMEBREW_PREFIX@@".into(), "/opt/pour".into()),
            ("@@HOMEBREW_CELLAR@@".into(), "/opt/pour/Cellar".into()),
            ("/opt/homebrew".into(), "/opt/pour".into()),
            ("/opt/homebrew/Cellar".into(), "/opt/pour/Cellar".into()),
        ])
    }

    #[test]
    fn longest_pattern_first_and_text_replacement() {
        let r = rep();
        assert_eq!(r.pairs()[0].0, "/opt/homebrew/Cellar");
        let s = r.replace_str("#!@@HOMEBREW_PREFIX@@/bin/bash\nX=@@HOMEBREW_CELLAR@@/jq /opt/homebrew/Cellar/a /opt/homebrew/bin").unwrap();
        assert_eq!(
            s,
            "#!/opt/pour/bin/bash\nX=/opt/pour/Cellar/jq /opt/pour/Cellar/a /opt/pour/bin"
        );
        assert!(r.replace_str("/usr/bin/env").is_none());
        assert!(r.matches(b"xx@@HOMEBREW_PREFIX@@yy"));
        assert!(!r.matches(b"nothing here"));
    }

    #[test]
    fn cstring_patch_shifts_tail_and_pads() {
        let r = rep();
        let mut data = b"AAA\0--prefix=/opt/homebrew/share/foo\0BBB\0".to_vec();
        let n = r.patch_cstrings(&mut data);
        assert_eq!(n, 1);
        assert_eq!(
            &data[..],
            b"AAA\0--prefix=/opt/pour/share/foo\0\0\0\0\0BBB\0"
        );
    }

    #[test]
    fn relocates_a_fake_keg() {
        let dir = tempfile::tempdir().unwrap();
        let keg = dir.path().join("Cellar/demo/1.0");
        std::fs::create_dir_all(keg.join("bin")).unwrap();
        std::fs::create_dir_all(keg.join("lib/pkgconfig")).unwrap();
        std::fs::write(
            keg.join("bin/demo"),
            "#!@@HOMEBREW_PREFIX@@/bin/sh\necho @@HOMEBREW_CELLAR@@/demo\n",
        )
        .unwrap();
        std::fs::write(
            keg.join("lib/pkgconfig/demo.pc"),
            "prefix=@@HOMEBREW_PREFIX@@\nlibdir=${prefix}/lib\n",
        )
        .unwrap();
        std::fs::write(
            keg.join("lib/binary.dat"),
            b"\x00\x01\x02 no placeholders \x00",
        )
        .unwrap();
        std::fs::write(keg.join("README"), "plain text, nothing to do").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                keg.join("lib/pkgconfig/demo.pc"),
                std::fs::Permissions::from_mode(0o444),
            )
            .unwrap();
            // python's certifi link: absolute target through the placeholder;
            // a relative link stays as it is
            std::os::unix::fs::symlink(
                "@@HOMEBREW_PREFIX@@/etc/ca-certificates/cert.pem",
                keg.join("lib/cacert.pem"),
            )
            .unwrap();
            std::os::unix::fs::symlink("demo", keg.join("bin/demo-link")).unwrap();
        }
        let platform = Platform {
            os: Os::Linux,
            arch: Arch::X86_64,
            macos_major: 0,
            wsl: false,
            ish: false,
        };
        let paths = Paths::new("/opt/pour");
        let rel = Relocation::for_formula(&platform, &paths, &[], false);
        let report = relocate_keg(&keg, &rel);
        assert_eq!(
            report.text_files,
            vec!["bin/demo".to_string(), "lib/pkgconfig/demo.pc".to_string()]
        );
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        assert_eq!(
            std::fs::read_to_string(keg.join("bin/demo")).unwrap(),
            "#!/opt/pour/bin/sh\necho /opt/pour/Cellar/demo\n"
        );
        assert_eq!(
            std::fs::read_to_string(keg.join("lib/pkgconfig/demo.pc")).unwrap(),
            "prefix=/opt/pour\nlibdir=${prefix}/lib\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(keg.join("lib/pkgconfig/demo.pc"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o444
            );
            assert_eq!(report.symlinks, vec!["lib/cacert.pem".to_string()]);
            assert_eq!(
                std::fs::read_link(keg.join("lib/cacert.pem")).unwrap(),
                PathBuf::from("/opt/pour/etc/ca-certificates/cert.pem")
            );
            assert_eq!(
                std::fs::read_link(keg.join("bin/demo-link")).unwrap(),
                PathBuf::from("demo")
            );
        }
    }

    #[test]
    fn linux_relocation_uses_pour_ld_so_and_skips_usr_local() {
        let platform = Platform {
            os: Os::Linux,
            arch: Arch::X86_64,
            macos_major: 0,
            wsl: false,
            ish: false,
        };
        let rel = Relocation::for_formula(
            &platform,
            &Paths::new("/opt/pour"),
            &["openjdk@21".into()],
            true,
        );
        assert_eq!(rel.interpreter.as_deref(), Some("/opt/pour/lib/ld.so"));
        assert!(rel
            .replacer
            .pairs()
            .iter()
            .any(|(a, b)| a == "/home/linuxbrew/.linuxbrew" && b == "/opt/pour"));
        assert!(!rel.replacer.pairs().iter().any(|(a, _)| a == "/usr/local"));
        assert!(rel
            .replacer
            .pairs()
            .iter()
            .any(|(a, b)| a == JAVA_PLACEHOLDER && b == "/opt/pour/opt/openjdk@21/libexec"));
    }
}
