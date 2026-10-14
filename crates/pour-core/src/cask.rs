//! Casks: macOS applications from Homebrew's cask registry, plus Pour-native
//! registries that also carry Windows, Linux and FreeBSD installers.

use crate::install::{Event, Reporter};
use crate::lock::Lock;
use crate::model::{Cask, Kind, NativeInstaller};
use crate::net::{self, Progress};
use crate::paths::Paths;
use crate::platform::{Arch, Os, Platform};
use crate::receipt::{self, Receipt};
use crate::store;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, thiserror::Error)]
pub enum CaskError {
    #[error("another pour is running on this prefix (lock held)")]
    Locked,
    #[error("{token} needs macOS {need} or newer (this is macOS {have})")]
    MacosTooOld {
        token: String,
        need: String,
        have: u32,
    },
    #[error("{token} is not available for this CPU ({arch})")]
    WrongArch { token: String, arch: String },
    #[error("{token} has no installer for {platform}")]
    NoInstaller { token: String, platform: String },
    #[error("{0}")]
    Download(String),
    #[error("{token}: {msg}")]
    Failed { token: String, msg: String },
    #[error("{0} is already installed at {1}; pass --force to replace it")]
    Exists(String, String),
    #[error("{0} is not installed")]
    NotInstalled(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, CaskError>;

#[derive(Debug, Default)]
pub struct CaskOptions {
    pub force: bool,
    pub skip_quarantine_clear: bool,
    pub appdir: Option<PathBuf>,
    pub dry_run: bool,
}

#[derive(Debug, Default, Clone)]
pub struct CaskSummary {
    pub token: String,
    pub version: String,
    pub artifacts: Vec<String>,
    pub caveats: Option<String>,
    pub warnings: Vec<String>,
    pub bytes: u64,
    pub ms: u128,
    /// Formulae the cask depends on; the caller installs them first.
    pub needs_formulae: Vec<String>,
    pub needs_casks: Vec<String>,
}

pub struct CaskEngine<'a> {
    pub paths: &'a Paths,
    pub platform: &'a Platform,
    pub reporter: &'a dyn Reporter,
}

fn sh(cmd: &mut Command) -> std::result::Result<String, String> {
    match cmd.output() {
        Ok(o) if o.status.success() => Ok(String::from_utf8_lossy(&o.stdout).into_owned()),
        Ok(o) => Err(format!(
            "{} failed: {}",
            cmd.get_program().to_string_lossy(),
            String::from_utf8_lossy(&o.stderr).trim()
        )),
        Err(e) => Err(format!("{}: {e}", cmd.get_program().to_string_lossy())),
    }
}

fn sudo_prefix() -> Vec<&'static str> {
    #[cfg(unix)]
    {
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } == 0 {
            return vec![];
        }
    }
    vec!["sudo", "--"]
}

fn run_privileged(args: &[&str]) -> std::result::Result<String, String> {
    let s = sudo_prefix();
    let mut all: Vec<&str> = s.clone();
    all.extend_from_slice(args);
    let mut cmd = Command::new(all[0]);
    cmd.args(&all[1..]);
    sh(&mut cmd)
}

fn expand_home(p: &str) -> String {
    let home = crate::paths::home().to_string_lossy().into_owned();
    p.replace("$HOME", &home)
        .replacen("~", &home, if p.starts_with('~') { 1 } else { 0 })
}

/// `depends_on` checks.
fn check_depends_on(cask: &Cask, platform: &Platform, summary: &mut CaskSummary) -> Result<()> {
    let Some(dep) = &cask.depends_on else {
        return Ok(());
    };
    let v: Value = serde_json::from_str(dep).unwrap_or(Value::Null);
    if let Some(macos) = v.get("macos").and_then(|m| m.as_object()) {
        if platform.os == Os::MacOs {
            for (op, vals) in macos {
                let need: Vec<u32> = vals
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str())
                            .filter_map(|s| s.split('.').next())
                            .filter_map(|s| s.parse().ok())
                            .collect()
                    })
                    .unwrap_or_default();
                let ok = match op.as_str() {
                    ">=" => need.iter().all(|n| platform.macos_major >= *n),
                    ">" => need.iter().all(|n| platform.macos_major > *n),
                    "<=" => need.iter().all(|n| platform.macos_major <= *n),
                    "<" => need.iter().all(|n| platform.macos_major < *n),
                    "==" => need.contains(&platform.macos_major),
                    _ => true,
                };
                if !ok {
                    return Err(CaskError::MacosTooOld {
                        token: cask.token.clone(),
                        need: need
                            .iter()
                            .map(|n| n.to_string())
                            .collect::<Vec<_>>()
                            .join("/"),
                        have: platform.macos_major,
                    });
                }
            }
        }
    }
    if let Some(arch) = v.get("arch").and_then(|a| a.as_array()) {
        let ok = arch.iter().any(|a| {
            matches!(
                (a.get("type").and_then(|t| t.as_str()), platform.arch),
                (Some("arm"), Arch::Arm64) | (Some("intel" | "x86_64"), Arch::X86_64)
            )
        });
        if !ok && !arch.is_empty() {
            return Err(CaskError::WrongArch {
                token: cask.token.clone(),
                arch: platform.arch.to_string(),
            });
        }
    }
    if let Some(f) = v.get("formula").and_then(|f| f.as_array()) {
        summary.needs_formulae = f
            .iter()
            .filter_map(|x| x.as_str().map(str::to_string))
            .collect();
    }
    if let Some(c) = v.get("cask").and_then(|c| c.as_array()) {
        summary.needs_casks = c
            .iter()
            .filter_map(|x| x.as_str().map(str::to_string))
            .collect();
    }
    Ok(())
}

/// Platform key for Pour-native installers: `macos-arm64`, `windows-x86_64`…
pub fn native_key(p: &Platform) -> String {
    format!("{}-{}", p.os, p.arch)
}

/// Live version feeds (`latest.json`) as the Pour-native registries publish
/// them: Tauri-updater style `platforms`, plus `files` / `downloads` maps
/// with names relative to the feed. Returns (version, url, sha256) for this
/// machine; url is `None` when the feed has no download for it.
pub fn livecheck(
    agent: &ureq::Agent,
    feed: &str,
    p: &Platform,
) -> Option<(String, Option<String>, Option<String>)> {
    let v = net::get_json(agent, feed, &[]).ok()?;
    let version = v
        .get("version")
        .and_then(|x| x.as_str())?
        .trim_start_matches('v')
        .to_string();
    let base = feed
        .rsplit_once('/')
        .map(|(b, _)| format!("{b}/"))
        .unwrap_or_default();
    let abs = |u: &str| -> String {
        if u.starts_with("http://") || u.starts_with("https://") {
            u.to_string()
        } else {
            format!("{base}{u}")
        }
    };
    // key -> (url, sha256)
    let mut map: Vec<(String, String, Option<String>)> = Vec::new();
    for section in ["platforms", "files", "downloads"] {
        if let Some(obj) = v.get(section).and_then(|x| x.as_object()) {
            for (k, e) in obj {
                let (u, sha) = match e {
                    Value::String(s) => (Some(s.clone()), None),
                    Value::Object(o) => (
                        o.get("url")
                            .or_else(|| o.get("file"))
                            .or_else(|| o.get("name"))
                            .and_then(|x| x.as_str())
                            .map(str::to_string),
                        o.get("sha256")
                            .and_then(|x| x.as_str())
                            .map(|x| x.to_ascii_lowercase()),
                    ),
                    _ => (None, None),
                };
                if let Some(u) = u {
                    if !u.is_empty() {
                        map.push((k.to_ascii_lowercase(), abs(&u), sha));
                    }
                }
            }
        }
    }
    let has_dpkg = which("dpkg");
    let has_rpm = which("rpm") || which("dnf");
    let keys: Vec<&str> = match (p.os, p.arch) {
        (Os::MacOs, Arch::Arm64) => vec![
            "darwin-aarch64",
            "darwin-aarch64-app",
            "macos-aarch64",
            "macos-arm64",
            "mac_arm",
            "macos-apple-silicon",
        ],
        (Os::MacOs, _) => vec![
            "darwin-x86_64",
            "darwin-x86_64-app",
            "macos-x86_64",
            "macos-intel",
            "mac_intel",
        ],
        (Os::Linux, Arch::Arm64) if has_dpkg => vec![
            "linux-aarch64-deb",
            "deb_arm",
            "chromeos_arm",
            "linux-aarch64",
            "linux-arm64",
        ],
        (Os::Linux, Arch::Arm64) => vec!["linux-aarch64", "linux-arm64", "linux-aarch64-appimage"],
        (Os::Linux, _) if has_dpkg => vec![
            "linux-x86_64-deb",
            "deb",
            "chromeos",
            "linux-x86_64",
            "linux-x86_64-appimage",
            "appimage",
        ],
        (Os::Linux, _) if has_rpm => vec![
            "linux-x86_64-rpm",
            "rpm",
            "linux-x86_64",
            "linux-x86_64-appimage",
            "appimage",
        ],
        (Os::Linux, _) => vec![
            "linux-x86_64",
            "linux-x86_64-appimage",
            "appimage",
            "linux-x86_64-deb",
            "deb",
        ],
        (Os::Windows, Arch::Arm64) => {
            vec!["windows-aarch64", "windows-arm64", "windows-aarch64-nsis"]
        }
        (Os::Windows, _) => vec!["windows-x86_64", "windows-x86_64-nsis", "windows", "win"],
        (Os::FreeBsd, _) => vec!["freebsd", "freebsd-x86_64", "freebsd_amd64"],
        _ => vec![],
    };
    let hit = keys
        .iter()
        .find_map(|k| map.iter().find(|(mk, _, _)| mk == k));
    match hit {
        Some((_, url, sha)) => Some((version, Some(url.clone()), sha.clone())),
        None => Some((version, None, None)),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Container {
    Dmg,
    Zip,
    Pkg,
    Tar,
    Naked,
    Exe,
    Msi,
    Deb,
    Rpm,
    AppImage,
}

fn container_for(url: &str, hint: Option<&str>, kind: Option<&str>) -> Container {
    let path = url
        .split(['?', '#'])
        .next()
        .unwrap_or(url)
        .to_ascii_lowercase();
    let by_hint = match hint {
        Some("dmg") => Some(Container::Dmg),
        Some("zip") => Some(Container::Zip),
        Some("pkg") => Some(Container::Pkg),
        Some("tar") | Some("tgz") | Some("tbz") | Some("txz") | Some("gzip") | Some("bzip2")
        | Some("xz") => Some(Container::Tar),
        Some("naked") => Some(Container::Naked),
        _ => None,
    };
    let by_kind = match kind {
        Some("exe") => Some(Container::Exe),
        Some("msi") => Some(Container::Msi),
        Some("deb") => Some(Container::Deb),
        Some("rpm") => Some(Container::Rpm),
        Some("appimage") => Some(Container::AppImage),
        Some("dmg") => Some(Container::Dmg),
        Some("zip") => Some(Container::Zip),
        Some("tar.gz") | Some("tar") | Some("tgz") => Some(Container::Tar),
        Some("pkg") => Some(Container::Pkg),
        _ => None,
    };
    if let Some(c) = by_kind.or(by_hint) {
        return c;
    }
    if path.ends_with(".dmg") {
        Container::Dmg
    } else if path.ends_with(".zip") {
        Container::Zip
    } else if path.ends_with(".pkg") || path.ends_with(".mpkg") {
        Container::Pkg
    } else if path.ends_with(".tar.gz")
        || path.ends_with(".tgz")
        || path.ends_with(".tar.xz")
        || path.ends_with(".tar.bz2")
        || path.ends_with(".tbz")
        || path.ends_with(".tar")
        || path.ends_with(".txz")
    {
        Container::Tar
    } else if path.ends_with(".exe") {
        Container::Exe
    } else if path.ends_with(".msi") {
        Container::Msi
    } else if path.ends_with(".deb") {
        Container::Deb
    } else if path.ends_with(".rpm") {
        Container::Rpm
    } else if path.ends_with(".appimage") {
        Container::AppImage
    } else {
        Container::Naked
    }
}

fn basename_from_url(url: &str) -> String {
    let p = url.split(['?', '#']).next().unwrap_or(url);
    let b = p.rsplit('/').next().unwrap_or("download");
    let b = b.replace("%20", " ");
    if b.is_empty() {
        "download".into()
    } else {
        b
    }
}

/// Find `name` at the top of `dir`, else anywhere below it (first match).
fn find_in(dir: &Path, name: &str) -> Option<PathBuf> {
    let direct = dir.join(name);
    if direct.exists() {
        return Some(direct);
    }
    let mut stack = vec![dir.to_path_buf()];
    let mut depth = 0;
    while let Some(d) = stack.pop() {
        depth += 1;
        if depth > 5000 {
            break;
        }
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.file_name().map(|n| n == name).unwrap_or(false) {
                return Some(p);
            }
            if p.is_dir() && !p.extension().map(|x| x == "app").unwrap_or(false) {
                stack.push(p);
            }
        }
    }
    None
}

fn move_or_copy(src: &Path, dest: &Path) -> std::io::Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if std::fs::rename(src, dest).is_ok() {
        return Ok(());
    }
    // Across devices: ditto keeps resource forks and code signatures on macOS.
    if Path::new("/usr/bin/ditto").exists() {
        sh(Command::new("/usr/bin/ditto").arg(src).arg(dest)).map_err(std::io::Error::other)?;
    } else {
        sh(Command::new("cp").arg("-R").arg("-p").arg(src).arg(dest))
            .map_err(std::io::Error::other)?;
    }
    let _ = store::remove_tree(src);
    Ok(())
}

impl CaskEngine<'_> {
    fn lock(&self) -> Result<Lock> {
        self.paths.ensure()?;
        Lock::try_acquire(&self.paths.lock_file())?.ok_or(CaskError::Locked)
    }

    fn appdir(&self, opts: &CaskOptions) -> PathBuf {
        opts.appdir
            .clone()
            .unwrap_or_else(|| self.paths.applications())
    }

    /// Install one cask. Dependencies are reported back, not installed here.
    pub fn install(&self, cask: &Cask, source: &str, opts: &CaskOptions) -> Result<CaskSummary> {
        let started = std::time::Instant::now();
        let _lock = self.lock()?;
        let mut summary = CaskSummary {
            token: cask.token.clone(),
            version: cask.version.clone(),
            ..Default::default()
        };
        check_depends_on(cask, self.platform, &mut summary)?;
        let agent = net::agent();

        // Which download?
        let (mut url, mut sha, mut version, art_override) =
            cask.download_for(&self.platform.cask_variations());
        let mut native: Option<NativeInstaller> = None;
        if self.platform.os != Os::MacOs {
            let key = native_key(self.platform);
            native = cask.native.iter().find(|n| n.platform == key).cloned();
            if native.is_none() && cask.livecheck_url.is_none() {
                return Err(CaskError::NoInstaller {
                    token: cask.token.clone(),
                    platform: self.platform.describe(),
                });
            }
            if let Some(n) = &native {
                url = n.url.clone();
                sha = n.sha256.clone();
            }
        }
        // Live version feed (Pour-native registries).
        if let Some(lc) = &cask.livecheck_url {
            if let Some((v, u, live_sha)) = livecheck(&agent, lc, self.platform) {
                if !v.is_empty() && v != version {
                    self.reporter.event(Event::Info(format!(
                        "  {}: live version {v} (registry said {version})",
                        cask.token
                    )));
                    url = url.replace(&version, &v);
                    version = v.clone();
                    sha = None;
                }
                if let Some(u) = u {
                    // The feed is the source of truth for where the newest build lives.
                    url = u;
                    sha = live_sha;
                }
                if native.is_some() {
                    // Keep the installer kind from the registry, with the live URL.
                    if let Some(n) = native.as_mut() {
                        n.url = url.clone();
                    }
                }
            }
        }
        if url.is_empty() {
            return Err(CaskError::NoInstaller {
                token: cask.token.clone(),
                platform: self.platform.describe(),
            });
        }
        summary.version = version.clone();
        if opts.dry_run {
            summary.ms = started.elapsed().as_millis();
            return Ok(summary);
        }

        // Already installed?
        if let Some(existing) = receipt::find_cask(self.paths, &cask.token) {
            if existing.receipt.version == version && !opts.force {
                return Err(CaskError::Exists(
                    cask.token.clone(),
                    existing.path.display().to_string(),
                ));
            }
            self.remove_artifacts(&existing.full(), false, &mut summary.warnings);
            let _ = store::remove_tree(&existing.path);
        }

        // Download.
        self.reporter.event(Event::Phase(format!(
            "Downloading {} {}",
            cask.token, version
        )));
        let file = self.paths.downloads().join(format!(
            "{}--{}--{}",
            cask.token,
            version,
            basename_from_url(&url)
        ));
        let progress: &dyn Progress = self.reporter;
        let user_agent_hdr = [("Accept", "*/*")];
        net::download(
            &agent,
            &url,
            &file,
            sha.as_deref(),
            &user_agent_hdr,
            &cask.token,
            progress,
        )
        .map_err(|e| CaskError::Download(e.to_string()))?;
        summary.bytes = std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
        // Gatekeeper parity with Homebrew: mark the download as quarantined so
        // macOS verifies the app on first launch (unless the registry says the
        // app is the creator's own and asks for the mark to be cleared).
        if self.platform.os == Os::MacOs && !cask.clear_quarantine {
            let _ = Command::new("/usr/bin/xattr")
                .args(["-w", "com.apple.quarantine", &quarantine_value()])
                .arg(&file)
                .output();
        }
        if sha.is_none() {
            summary.warnings.push(format!("{}: no checksum published for this download (sha256 :no_check); verified only by HTTPS", cask.token));
        }

        // Stage.
        self.reporter.event(Event::Phase(format!(
            "Installing {} {}",
            cask.token, version
        )));
        let stage =
            self.paths
                .cache()
                .join("stage")
                .join(format!("{}-{}", cask.token, std::process::id()));
        let _ = store::remove_tree(&stage);
        std::fs::create_dir_all(&stage)?;
        let container = container_for(
            &url,
            cask.container.as_deref(),
            native.as_ref().map(|n| n.kind.as_str()),
        );
        let mut mounted: Option<PathBuf> = None;
        let unpack_result = self.unpack(&file, &stage, container, &mut mounted);
        let artifacts_json = art_override.unwrap_or_else(|| cask.artifacts.clone());
        let result = match unpack_result {
            Ok(()) => self.place(
                cask,
                &artifacts_json,
                &stage,
                &file,
                container,
                native.as_ref(),
                opts,
                &mut summary,
            ),
            Err(e) => Err(e),
        };
        if let Some(m) = mounted {
            let _ = sh(Command::new("/usr/bin/hdiutil")
                .args(["detach", "-force"])
                .arg(&m));
        }
        let _ = store::remove_tree(&stage);
        let room = self.paths.caskroom().join(&cask.token).join(&version);
        if let Err(e) = result {
            if !room.join(crate::receipt::RECEIPT).exists() {
                let _ = store::remove_tree(&room);
                let _ = std::fs::remove_dir(self.paths.caskroom().join(&cask.token));
            }
            return Err(e);
        }

        // Receipt in the Caskroom.
        std::fs::create_dir_all(&room)?;
        let rcpt = Receipt {
            kind: Kind::Cask,
            name: cask.token.clone(),
            version: version.clone(),
            tap: source.to_string(),
            desc: cask.desc.clone(),
            homepage: cask.homepage.clone(),
            sha256: sha,
            tag: self.platform.native_tag(),
            cellar: None,
            installed_at: receipt::now(),
            installed_on_request: true,
            installed_as_dependency: false,
            runtime_deps: summary.needs_formulae.clone(),
            keg_only: false,
            links: Vec::new(),
            artifacts: summary.artifacts.clone(),
            relocated_files: 0,
            deep_relocation: false,
            pour_version: env!("CARGO_PKG_VERSION").into(),
            platform: self.platform.native_tag(),
            pinned: false,
            native_kind: native.map(|n| n.kind),
            links_len: 0,
        };
        rcpt.write(&room)?;
        // Keep the cask definition for uninstall (artifacts, zap).
        let _ = std::fs::write(room.join("cask.json"), serde_json::to_vec_pretty(&serde_json::json!({"token": cask.token, "artifacts": serde_json::from_str::<Value>(&artifacts_json).unwrap_or(Value::Null)})).unwrap_or_default());
        summary.caveats = cask.caveats.clone();
        summary.ms = started.elapsed().as_millis();
        Ok(summary)
    }

    fn unpack(
        &self,
        file: &Path,
        stage: &Path,
        container: Container,
        mounted: &mut Option<PathBuf>,
    ) -> Result<()> {
        let fail = |msg: String| CaskError::Failed {
            token: file
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            msg,
        };
        match container {
            Container::Dmg => {
                let mount = stage.join("mount");
                std::fs::create_dir_all(&mount)?;
                // Agree to any licence prompt by feeding "y".
                let mut child = Command::new("/usr/bin/hdiutil")
                    .args([
                        "attach",
                        "-nobrowse",
                        "-readonly",
                        "-noautoopen",
                        "-noverify",
                        "-mountpoint",
                    ])
                    .arg(&mount)
                    .arg(file)
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                    .map_err(|e| fail(format!("hdiutil: {e}")))?;
                if let Some(mut stdin) = child.stdin.take() {
                    use std::io::Write;
                    let _ = stdin.write_all(b"y\n");
                }
                let out = child.wait_with_output().map_err(|e| fail(e.to_string()))?;
                if !out.status.success() {
                    return Err(fail(format!(
                        "could not mount disk image: {}",
                        String::from_utf8_lossy(&out.stderr).trim()
                    )));
                }
                *mounted = Some(mount);
                Ok(())
            }
            Container::Zip => {
                if Path::new("/usr/bin/ditto").exists() {
                    sh(Command::new("/usr/bin/ditto")
                        .args(["-x", "-k"])
                        .arg(file)
                        .arg(stage))
                    .map_err(fail)?;
                } else if Path::new("/usr/bin/unzip").exists() || which("unzip") {
                    sh(Command::new("unzip")
                        .args(["-q", "-o"])
                        .arg(file)
                        .arg("-d")
                        .arg(stage))
                    .map_err(fail)?;
                } else {
                    sh(Command::new("tar")
                        .arg("-xf")
                        .arg(file)
                        .arg("-C")
                        .arg(stage))
                    .map_err(fail)?;
                }
                Ok(())
            }
            Container::Tar => {
                sh(Command::new("tar")
                    .arg("-xf")
                    .arg(file)
                    .arg("-C")
                    .arg(stage))
                .map_err(fail)?;
                Ok(())
            }
            Container::Pkg
            | Container::Naked
            | Container::Exe
            | Container::Msi
            | Container::Deb
            | Container::Rpm
            | Container::AppImage => Ok(()),
        }
    }

    /// Run the artifact stanzas against the staged content.
    #[allow(clippy::too_many_arguments)]
    fn place(
        &self,
        cask: &Cask,
        artifacts_json: &str,
        stage: &Path,
        file: &Path,
        container: Container,
        native: Option<&NativeInstaller>,
        opts: &CaskOptions,
        summary: &mut CaskSummary,
    ) -> Result<()> {
        let token = cask.token.clone();
        let fail = |msg: String| CaskError::Failed {
            token: token.clone(),
            msg,
        };
        let content_root = if container == Container::Dmg {
            stage.join("mount")
        } else {
            stage.to_path_buf()
        };
        let appdir = self.appdir(opts);

        // Non-macOS: a native installer is the whole story.
        if let Some(n) = native {
            return self.install_native(cask, n, file, stage, container, summary);
        }
        if self.platform.os != Os::MacOs {
            return Err(CaskError::NoInstaller {
                token: cask.token.clone(),
                platform: self.platform.describe(),
            });
        }

        let artifacts: Vec<Value> = serde_json::from_str(artifacts_json).unwrap_or_default();
        // Naked downloads (a bare binary or font): the file itself is the artifact.
        if container == Container::Naked {
            let dest = stage.join(basename_from_url(&cask.url));
            let _ = std::fs::copy(file, &dest);
        }
        let mut placed_apps: Vec<PathBuf> = Vec::new();
        for art in &artifacts {
            let Some(obj) = art.as_object() else { continue };
            for (kind, val) in obj {
                let target_override = obj.get("target").and_then(|t| t.as_str()).map(expand_home);
                match kind.as_str() {
                    "app" | "suite" => {
                        for name in val
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|v| v.as_str())
                        {
                            let src = find_in(&content_root, name)
                                .ok_or_else(|| fail(format!("{name} not found in the download")))?;
                            let dest = target_override
                                .clone()
                                .map(PathBuf::from)
                                .unwrap_or_else(|| appdir.join(name));
                            if dest.exists() {
                                if opts.force
                                    || receipt::find_cask(self.paths, &cask.token).is_some()
                                {
                                    store::remove_tree(&dest)?;
                                } else {
                                    return Err(CaskError::Exists(
                                        name.to_string(),
                                        dest.display().to_string(),
                                    ));
                                }
                            }
                            move_or_copy(&src, &dest)?;
                            if cask.clear_quarantine && !opts.skip_quarantine_clear {
                                let _ = Command::new("/usr/bin/xattr")
                                    .args(["-dr", "com.apple.quarantine"])
                                    .arg(&dest)
                                    .output();
                            } else {
                                let _ = Command::new("/usr/bin/xattr")
                                    .args(["-w", "-r", "com.apple.quarantine", &quarantine_value()])
                                    .arg(&dest)
                                    .output();
                            }
                            summary.artifacts.push(dest.display().to_string());
                            placed_apps.push(dest);
                        }
                    }
                    "pkg" => {
                        for name in val
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|v| v.as_str())
                        {
                            let src = if container == Container::Pkg {
                                file.to_path_buf()
                            } else {
                                find_in(&content_root, name).ok_or_else(|| {
                                    fail(format!("{name} not found in the download"))
                                })?
                            };
                            run_privileged(&[
                                "/usr/sbin/installer",
                                "-pkg",
                                &src.to_string_lossy(),
                                "-target",
                                "/",
                            ])
                            .map_err(fail)?;
                            summary.artifacts.push(format!("pkg:{name}"));
                        }
                    }
                    "binary" | "manpage" | "font" | "qlplugin" | "prefpane" | "screen_saver"
                    | "dictionary" | "colorpicker" | "input_method" | "keyboard_layout"
                    | "mdimporter" | "vst_plugin" | "vst3_plugin" | "audio_unit_plugin"
                    | "artifact" => {
                        let mut items: Vec<String> = Vec::new();
                        let mut target_in_list: Option<String> = None;
                        for v in val.as_array().into_iter().flatten() {
                            match v {
                                Value::String(s) => items.push(s.clone()),
                                Value::Object(o) => {
                                    target_in_list =
                                        o.get("target").and_then(|t| t.as_str()).map(expand_home)
                                }
                                _ => {}
                            }
                        }
                        let target = target_override.clone().or(target_in_list);
                        for item in items {
                            let src_spec = item
                                .replace("$APPDIR", &appdir.to_string_lossy())
                                .replace("$HOMEBREW_PREFIX", &self.paths.prefix_str());
                            let src = if Path::new(&src_spec).is_absolute() {
                                PathBuf::from(&src_spec)
                            } else {
                                find_in(
                                    &content_root,
                                    Path::new(&src_spec)
                                        .file_name()
                                        .map(|f| f.to_string_lossy().into_owned())
                                        .unwrap_or_default()
                                        .as_str(),
                                )
                                .unwrap_or(content_root.join(&src_spec))
                            };
                            let default_dir = match kind.as_str() {
                                "binary" => self.paths.bin(),
                                "manpage" => {
                                    let section = src
                                        .extension()
                                        .and_then(|e| e.to_str())
                                        .unwrap_or("1")
                                        .to_string();
                                    self.paths
                                        .prefix
                                        .join("share/man")
                                        .join(format!("man{section}"))
                                }
                                "font" => crate::paths::home().join("Library/Fonts"),
                                "qlplugin" => crate::paths::home().join("Library/QuickLook"),
                                "prefpane" => crate::paths::home().join("Library/PreferencePanes"),
                                "screen_saver" => {
                                    crate::paths::home().join("Library/Screen Savers")
                                }
                                "dictionary" => crate::paths::home().join("Library/Dictionaries"),
                                "colorpicker" => crate::paths::home().join("Library/ColorPickers"),
                                "input_method" => {
                                    crate::paths::home().join("Library/Input Methods")
                                }
                                "keyboard_layout" => {
                                    crate::paths::home().join("Library/Keyboard Layouts")
                                }
                                "mdimporter" => crate::paths::home().join("Library/Spotlight"),
                                "vst_plugin" => {
                                    crate::paths::home().join("Library/Audio/Plug-Ins/VST")
                                }
                                "vst3_plugin" => {
                                    crate::paths::home().join("Library/Audio/Plug-Ins/VST3")
                                }
                                "audio_unit_plugin" => {
                                    crate::paths::home().join("Library/Audio/Plug-Ins/Components")
                                }
                                _ => self
                                    .paths
                                    .caskroom()
                                    .join(&cask.token)
                                    .join(&summary.version),
                            };
                            let dest = match &target {
                                Some(t) => {
                                    let t = t
                                        .replace("$HOMEBREW_PREFIX", &self.paths.prefix_str())
                                        .replace("$APPDIR", &appdir.to_string_lossy());
                                    let t = t.trim_start_matches("//").to_string();
                                    let t = if t.starts_with('/') {
                                        t
                                    } else {
                                        format!("/{t}")
                                    };
                                    PathBuf::from(t)
                                }
                                None => default_dir.join(src.file_name().unwrap_or_default()),
                            };
                            if let Some(d) = dest.parent() {
                                std::fs::create_dir_all(d)?;
                            }
                            if dest.exists() || dest.is_symlink() {
                                let _ = store::remove_tree(&dest);
                            }
                            if kind == "binary" || kind == "manpage" {
                                if !src.exists() {
                                    summary.warnings.push(format!(
                                        "{}: {} not found, skipped",
                                        cask.token,
                                        src.display()
                                    ));
                                    continue;
                                }
                                #[cfg(unix)]
                                std::os::unix::fs::symlink(&src, &dest)?;
                                #[cfg(unix)]
                                {
                                    use std::os::unix::fs::PermissionsExt;
                                    if kind == "binary" {
                                        if let Ok(meta) = std::fs::metadata(&src) {
                                            let _ = std::fs::set_permissions(
                                                &src,
                                                std::fs::Permissions::from_mode(
                                                    meta.permissions().mode() | 0o111,
                                                ),
                                            );
                                        }
                                    }
                                }
                            } else if src.exists() {
                                move_or_copy(&src, &dest)?;
                            } else {
                                summary.warnings.push(format!(
                                    "{}: {} not found, skipped",
                                    cask.token,
                                    src.display()
                                ));
                                continue;
                            }
                            summary.artifacts.push(dest.display().to_string());
                        }
                    }
                    "command_wrapper" => {
                        let mut name = String::new();
                        let mut exe = String::new();
                        for v in val.as_array().into_iter().flatten() {
                            match v {
                                Value::String(s) => name = s.clone(),
                                Value::Object(o) => {
                                    exe = o
                                        .get("executable")
                                        .and_then(|e| e.as_str())
                                        .unwrap_or("")
                                        .to_string()
                                }
                                _ => {}
                            }
                        }
                        if !name.is_empty() && !exe.is_empty() {
                            let exe = exe.replace("$APPDIR", &appdir.to_string_lossy());
                            let dest = target_override
                                .clone()
                                .map(|t| {
                                    PathBuf::from(
                                        t.replace("$HOMEBREW_PREFIX", &self.paths.prefix_str()),
                                    )
                                })
                                .unwrap_or_else(|| self.paths.bin().join(&name));
                            std::fs::write(&dest, format!("#!/bin/sh\nexec \"{exe}\" \"$@\"\n"))?;
                            #[cfg(unix)]
                            {
                                use std::os::unix::fs::PermissionsExt;
                                std::fs::set_permissions(
                                    &dest,
                                    std::fs::Permissions::from_mode(0o755),
                                )?;
                            }
                            summary.artifacts.push(dest.display().to_string());
                        }
                    }
                    "installer" => {
                        for inst in val.as_array().into_iter().flatten() {
                            if let Some(script) = inst.get("script") {
                                let exe = script
                                    .get("executable")
                                    .and_then(|e| e.as_str())
                                    .unwrap_or("");
                                let path = find_in(
                                    &content_root,
                                    Path::new(exe)
                                        .file_name()
                                        .map(|f| f.to_string_lossy().into_owned())
                                        .unwrap_or_default()
                                        .as_str(),
                                )
                                .unwrap_or(content_root.join(exe));
                                let args: Vec<String> = script
                                    .get("args")
                                    .and_then(|a| a.as_array())
                                    .map(|a| {
                                        a.iter()
                                            .filter_map(|x| x.as_str().map(str::to_string))
                                            .collect()
                                    })
                                    .unwrap_or_default();
                                let sudo = script
                                    .get("sudo")
                                    .and_then(|s| s.as_bool())
                                    .unwrap_or(false);
                                let mut argv: Vec<String> =
                                    vec![path.to_string_lossy().into_owned()];
                                argv.extend(args);
                                if sudo {
                                    let refs: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
                                    run_privileged(&refs).map_err(fail)?;
                                } else {
                                    sh(Command::new(&argv[0]).args(&argv[1..])).map_err(fail)?;
                                }
                                summary.artifacts.push(format!("installer:{exe}"));
                            } else if let Some(manual) = inst.get("manual").and_then(|m| m.as_str())
                            {
                                let room = self
                                    .paths
                                    .caskroom()
                                    .join(&cask.token)
                                    .join(&summary.version);
                                std::fs::create_dir_all(&room)?;
                                if let Some(src) = find_in(&content_root, manual) {
                                    let _ = move_or_copy(&src, &room.join(manual));
                                }
                                summary.warnings.push(format!(
                                    "{}: finish the installation by hand: open {}",
                                    cask.token,
                                    room.join(manual).display()
                                ));
                            }
                        }
                    }
                    "postflight_steps"
                    | "preflight_steps"
                    | "uninstall"
                    | "zap"
                    | "uninstall_preflight_steps"
                    | "uninstall_postflight_steps"
                    | "target"
                    | "service"
                    | "generate_completions_from_executable"
                    | "bash_completion"
                    | "zsh_completion"
                    | "fish_completion"
                    | "generated_script" => {}
                    other => summary.warnings.push(format!(
                        "{}: artifact `{other}` is not handled by Pour yet",
                        cask.token
                    )),
                }
            }
        }
        if placed_apps.is_empty() && summary.artifacts.is_empty() {
            return Err(fail(
                "the cask produced no artifacts Pour knows how to place".into(),
            ));
        }
        Ok(())
    }

    /// Windows / Linux / FreeBSD installers from Pour-native registries.
    fn install_native(
        &self,
        cask: &Cask,
        n: &NativeInstaller,
        file: &Path,
        stage: &Path,
        container: Container,
        summary: &mut CaskSummary,
    ) -> Result<()> {
        let token = cask.token.clone();
        let fail = |msg: String| CaskError::Failed {
            token: token.clone(),
            msg,
        };
        let room = self
            .paths
            .caskroom()
            .join(&cask.token)
            .join(&summary.version);
        std::fs::create_dir_all(&room)?;
        match container {
            Container::Exe => {
                #[cfg(windows)]
                {
                    let status = Command::new(file)
                        .arg("/S")
                        .status()
                        .or_else(|_| Command::new(file).status())
                        .map_err(|e| fail(e.to_string()))?;
                    if !status.success() {
                        return Err(fail(format!("installer exited with {status}")));
                    }
                }
                #[cfg(not(windows))]
                {
                    let keep = room.join(file.file_name().unwrap_or_default());
                    let _ = std::fs::copy(file, &keep);
                    summary.warnings.push(format!(
                        "{}: Windows installer saved to {} — run it on Windows",
                        cask.token,
                        keep.display()
                    ));
                }
                summary.artifacts.push(format!("exe:{}", file.display()));
            }
            Container::Msi => {
                #[cfg(windows)]
                {
                    let status = Command::new("msiexec")
                        .args(["/i"])
                        .arg(file)
                        .args(["/passive", "/norestart"])
                        .status()
                        .map_err(|e| fail(e.to_string()))?;
                    if !status.success() {
                        return Err(fail(format!("msiexec exited with {status}")));
                    }
                }
                summary.artifacts.push(format!("msi:{}", file.display()));
            }
            Container::Deb => {
                let f = file.to_string_lossy().into_owned();
                if which("apt-get") {
                    // apt resolves the package's dependencies; a stale or empty
                    // package list is the usual reason it refuses, so refresh once.
                    if let Err(e) = run_privileged(&["apt-get", "install", "-y", &f]) {
                        let _ = run_privileged(&["apt-get", "update"]);
                        run_privileged(&["apt-get", "install", "-y", &f])
                            .map_err(|e2| fail(format!("{e2} (first attempt: {e})")))?;
                    }
                } else {
                    run_privileged(&["dpkg", "-i", &f]).map_err(fail)?;
                }
                let pkg = sh(Command::new("dpkg-deb")
                    .args(["-f"])
                    .arg(file)
                    .arg("Package"))
                .unwrap_or_default()
                .trim()
                .to_string();
                summary.artifacts.push(format!(
                    "deb:{}",
                    if pkg.is_empty() {
                        cask.token.clone()
                    } else {
                        pkg
                    }
                ));
            }
            Container::Rpm => {
                let tool = if which("dnf") { "dnf" } else { "rpm" };
                let args: Vec<&str> = if tool == "dnf" {
                    vec!["dnf", "install", "-y"]
                } else {
                    vec!["rpm", "-Uvh"]
                };
                let mut a: Vec<&str> = args.clone();
                let f = file.to_string_lossy().into_owned();
                a.push(&f);
                run_privileged(&a).map_err(fail)?;
                summary.artifacts.push(format!("rpm:{}", cask.token));
            }
            Container::AppImage => {
                let dest = room.join(format!("{}.AppImage", cask.token));
                std::fs::copy(file, &dest)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755))?;
                    let link = self.paths.bin().join(&cask.token);
                    let _ = std::fs::remove_file(&link);
                    std::os::unix::fs::symlink(&dest, &link)?;
                    summary.artifacts.push(link.display().to_string());
                }
                summary.artifacts.push(dest.display().to_string());
            }
            Container::Tar | Container::Zip => {
                // Extracted into the stage already: keep it in the Caskroom and
                // link executables named like the token into bin.
                let dest = room.join("files");
                let _ = store::remove_tree(&dest);
                move_or_copy(stage, &dest)?;
                summary.artifacts.push(dest.display().to_string());
                let mut stack = vec![dest.clone()];
                while let Some(d) = stack.pop() {
                    let Ok(rd) = std::fs::read_dir(&d) else {
                        continue;
                    };
                    for e in rd.flatten() {
                        let p = e.path();
                        if p.is_dir() {
                            stack.push(p);
                            continue;
                        }
                        let fname = p
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .to_ascii_lowercase();
                        if fname == cask.token.to_ascii_lowercase()
                            || fname == format!("{}.exe", cask.token.to_ascii_lowercase())
                        {
                            #[cfg(unix)]
                            {
                                use std::os::unix::fs::PermissionsExt;
                                let _ = std::fs::set_permissions(
                                    &p,
                                    std::fs::Permissions::from_mode(0o755),
                                );
                                let link = self.paths.bin().join(&cask.token);
                                let _ = std::fs::remove_file(&link);
                                std::os::unix::fs::symlink(&p, &link)?;
                                summary.artifacts.push(link.display().to_string());
                            }
                        }
                    }
                }
            }
            other => {
                return Err(fail(format!(
                    "installer kind {other:?} ({}) is not supported on {}",
                    n.kind,
                    self.platform.describe()
                )))
            }
        }
        Ok(())
    }

    /// Remove what a cask placed. `zap` also runs the cask's `zap` stanza.
    pub fn uninstall(&self, token: &str, zap: bool) -> Result<Vec<String>> {
        receipt::invalidate(self.paths);
        let _lock = self.lock()?;
        let installed = receipt::find_cask(self.paths, token)
            .ok_or_else(|| CaskError::NotInstalled(token.to_string()))?;
        let mut warnings = Vec::new();
        let def: Value = std::fs::read_to_string(installed.path.join("cask.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or(Value::Null);
        let artifacts = def
            .get("artifacts")
            .and_then(|a| a.as_array())
            .cloned()
            .unwrap_or_default();
        // uninstall stanza first (quit, launchctl, pkgutil…)
        for art in &artifacts {
            if let Some(list) = art.get("uninstall").and_then(|u| u.as_array()) {
                for u in list {
                    self.run_uninstall_stanza(u, &mut warnings);
                }
            }
        }
        self.remove_artifacts(&installed.full(), true, &mut warnings);
        if zap {
            for art in &artifacts {
                if let Some(list) = art.get("zap").and_then(|u| u.as_array()) {
                    for z in list {
                        self.run_uninstall_stanza(z, &mut warnings);
                    }
                }
            }
        }
        let _ = store::remove_tree(&installed.path);
        let _ = std::fs::remove_dir(self.paths.caskroom().join(token));
        Ok(warnings)
    }

    fn run_uninstall_stanza(&self, u: &Value, warnings: &mut Vec<String>) {
        let strs = |k: &str| -> Vec<String> {
            match u.get(k) {
                Some(Value::String(s)) => vec![s.clone()],
                Some(Value::Array(a)) => a
                    .iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect(),
                _ => Vec::new(),
            }
        };
        if self.platform.os == Os::MacOs {
            for id in strs("quit") {
                let _ = Command::new("/usr/bin/osascript")
                    .args(["-e", &format!("tell application id \"{id}\" to quit")])
                    .output();
            }
            for sig in u
                .get("signal")
                .and_then(|s| s.as_array())
                .cloned()
                .unwrap_or_default()
            {
                if let Some(a) = sig.as_array() {
                    if a.len() == 2 {
                        let _ = Command::new("pkill")
                            .arg(format!("-{}", a[0].as_str().unwrap_or("TERM")))
                            .arg("-x")
                            .arg(a[1].as_str().unwrap_or(""))
                            .output();
                    }
                }
            }
            for label in strs("launchctl") {
                #[cfg(unix)]
                let uid = unsafe { libc::getuid() };
                #[cfg(not(unix))]
                let uid = 501;
                let _ = Command::new("/bin/launchctl")
                    .args(["bootout", &format!("gui/{uid}/{label}")])
                    .output();
                let _ = Command::new("/bin/launchctl")
                    .args(["remove", &label])
                    .output();
                for dir in ["Library/LaunchAgents", "Library/LaunchDaemons"] {
                    let p = crate::paths::home()
                        .join(dir)
                        .join(format!("{label}.plist"));
                    let _ = std::fs::remove_file(p);
                }
            }
            for pkg in strs("pkgutil") {
                if let Ok(out) = sh(Command::new("/usr/sbin/pkgutil").arg(format!("--pkgs={pkg}")))
                {
                    for id in out.lines().map(str::trim).filter(|l| !l.is_empty()) {
                        if let Ok(files) = sh(Command::new("/usr/sbin/pkgutil").args([
                            "--only-files",
                            "--files",
                            id,
                        ])) {
                            let mut list: Vec<String> =
                                files.lines().map(|l| format!("/{}", l.trim())).collect();
                            list.sort();
                            if !list.is_empty() {
                                let mut args = vec!["rm", "-f"];
                                let refs: Vec<&str> = list.iter().map(|s| s.as_str()).collect();
                                args.extend(refs);
                                let _ = run_privileged(&args);
                            }
                        }
                        let _ = run_privileged(&["/usr/sbin/pkgutil", "--forget", id]);
                    }
                }
            }
            for item in strs("login_item") {
                let _ = Command::new("/usr/bin/osascript")
                    .args([
                        "-e",
                        &format!(
                            "tell application \"System Events\" to delete login item \"{item}\""
                        ),
                    ])
                    .output();
            }
        }
        for p in strs("delete") {
            for g in glob_expand(&expand_home(&p)) {
                if store::remove_tree(&g).is_err() {
                    let _ = run_privileged(&["rm", "-rf", &g.to_string_lossy()]);
                }
            }
        }
        for p in strs("trash") {
            let trash = crate::paths::home().join(".Trash");
            for g in glob_expand(&expand_home(&p)) {
                let dest = trash.join(g.file_name().unwrap_or_default());
                if std::fs::rename(&g, &dest).is_err() {
                    let _ = store::remove_tree(&g);
                }
            }
        }
        for p in strs("rmdir") {
            for g in glob_expand(&expand_home(&p)) {
                let _ = std::fs::remove_file(g.join(".DS_Store"));
                let _ = std::fs::remove_dir(&g);
            }
        }
        if let Some(script) = u.get("script") {
            let exe = script
                .get("executable")
                .and_then(|e| e.as_str())
                .unwrap_or("");
            if !exe.is_empty() {
                let args: Vec<String> = script
                    .get("args")
                    .and_then(|a| a.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                let mut argv = vec![expand_home(exe)];
                argv.extend(args);
                let refs: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
                let r = if script
                    .get("sudo")
                    .and_then(|s| s.as_bool())
                    .unwrap_or(false)
                {
                    run_privileged(&refs)
                } else {
                    sh(Command::new(&argv[0]).args(&argv[1..]))
                };
                if let Err(e) = r {
                    warnings.push(format!("uninstall script: {e}"));
                }
            }
        }
    }

    fn remove_artifacts(&self, r: &Receipt, report: bool, warnings: &mut Vec<String>) {
        for a in &r.artifacts {
            if let Some(rest) = a.strip_prefix("deb:") {
                let _ = run_privileged(&["dpkg", "-r", rest]);
            } else if let Some(rest) = a.strip_prefix("rpm:") {
                let _ = run_privileged(&["rpm", "-e", rest]);
            } else if a.starts_with("pkg:")
                || a.starts_with("installer:")
                || a.starts_with("exe:")
                || a.starts_with("msi:")
            {
                if report {
                    warnings.push(format!(
                        "{}: {} was installed by an installer; its own uninstaller may be needed",
                        r.name, a
                    ));
                }
            } else {
                let p = PathBuf::from(a);
                if (p.is_symlink() || p.exists()) && store::remove_tree(&p).is_err() {
                    let _ = run_privileged(&["rm", "-rf", a]);
                }
            }
        }
    }
}

/// `0083;<hex seconds>;Pour;` — the quarantine attribute format macOS uses
/// (flags, timestamp, agent name, UUID), the same shape Homebrew writes.
fn quarantine_value() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("0083;{secs:08x};Pour;")
}

fn glob_expand(p: &str) -> Vec<PathBuf> {
    if p.contains(['*', '?', '[']) {
        glob::glob(p)
            .map(|it| it.flatten().collect())
            .unwrap_or_default()
    } else {
        let pb = PathBuf::from(p);
        if pb.exists() || pb.is_symlink() {
            vec![pb]
        } else {
            Vec::new()
        }
    }
}

pub fn which(name: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| {
            std::env::split_paths(&p)
                .any(|d| d.join(name).is_file() || d.join(format!("{name}.exe")).is_file())
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_detection() {
        assert_eq!(
            container_for("https://x/y/Firefox%20157.dmg", None, None),
            Container::Dmg
        );
        assert_eq!(
            container_for("https://x/iTerm2-3_7_4.zip?dl=1", None, None),
            Container::Zip
        );
        assert_eq!(
            container_for("https://x/Keel.app.tar.gz", None, None),
            Container::Tar
        );
        assert_eq!(
            container_for("https://x/stable", Some("dmg"), None),
            Container::Dmg
        );
        assert_eq!(
            container_for("https://x/Setup.exe", None, Some("exe")),
            Container::Exe
        );
        assert_eq!(
            container_for("https://x/FiraCode.ttf", None, None),
            Container::Naked
        );
        assert_eq!(
            basename_from_url("https://a/b/Firefox%20157.0.1.dmg?x=1"),
            "Firefox 157.0.1.dmg"
        );
        assert_eq!(
            native_key(&Platform {
                os: Os::Windows,
                arch: Arch::X86_64,
                macos_major: 0,
                wsl: false,
                ish: false
            }),
            "windows-x86_64"
        );
    }

    #[test]
    fn depends_on_checks() {
        let p = Platform {
            os: Os::MacOs,
            arch: Arch::Arm64,
            macos_major: 14,
            wsl: false,
            ish: false,
        };
        let mut c = Cask {
            token: "x".into(),
            depends_on: Some(r#"{"macos": {">=": ["15"]}}"#.into()),
            ..Default::default()
        };
        let mut s = CaskSummary::default();
        assert!(matches!(
            check_depends_on(&c, &p, &mut s),
            Err(CaskError::MacosTooOld { .. })
        ));
        c.depends_on =
            Some(r#"{"macos": {">=": ["13"]}, "arch": [{"type": "intel", "bits": 64}]}"#.into());
        assert!(matches!(
            check_depends_on(&c, &p, &mut s),
            Err(CaskError::WrongArch { .. })
        ));
        c.depends_on = Some(r#"{"formula": ["ffmpeg"], "cask": ["other"]}"#.into());
        check_depends_on(&c, &p, &mut s).unwrap();
        assert_eq!(s.needs_formulae, vec!["ffmpeg"]);
        assert_eq!(s.needs_casks, vec!["other"]);
    }

    #[test]
    fn find_in_searches_below() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sub/Deep.app")).unwrap();
        assert!(find_in(dir.path(), "Deep.app")
            .unwrap()
            .ends_with("sub/Deep.app"));
        assert!(find_in(dir.path(), "Nope.app").is_none());
        assert_eq!(
            expand_home("~/Library/X"),
            format!("{}/Library/X", crate::paths::home().display())
        );
    }
}
