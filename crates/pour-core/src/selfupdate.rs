//! `pour self-update`: fetch the newest release of Pour and replace the
//! running binary atomically.

use crate::net;
use crate::platform::{Arch, Os, Platform};
use std::path::PathBuf;

pub const RELEASE_FEED: &str = "https://ipconfig.co.network/pour/releases/latest.json";
pub const RELEASE_FEED_FALLBACK: &str =
    "https://api.github.com/repos/JKS-sys/pour-13-Oct-2026/releases/latest";

/// Asset name for this machine, as `update.sh` publishes them.
pub fn asset_name(p: &Platform) -> Option<String> {
    let os = match p.os {
        Os::MacOs => "macos",
        Os::Linux => "linux",
        Os::Windows => "windows",
        Os::FreeBsd => "freebsd",
        Os::Haiku => "haiku",
        _ => return None,
    };
    let arch = match p.arch {
        Arch::Arm64 => "arm64",
        Arch::X86_64 => "x86_64",
        Arch::X86 => "i686",
        Arch::Other => return None,
    };
    Some(format!(
        "pour-{os}-{arch}{}",
        if p.os == Os::Windows { ".exe" } else { "" }
    ))
}

#[derive(Debug, Clone)]
pub struct Release {
    pub version: String,
    pub url: String,
    pub sha256: Option<String>,
    pub notes: String,
}

/// Look up the newest release for this platform.
pub fn latest(agent: &ureq::Agent, p: &Platform) -> Result<Release, String> {
    let asset = asset_name(p).ok_or_else(|| format!("no prebuilt Pour for {}", p.describe()))?;
    // Pour's own feed: {"version": "0.1.0", "assets": {"pour-macos-arm64": {"url": ..., "sha256": ...}}, "notes": "..."}
    if let Ok(v) = net::get_json(agent, RELEASE_FEED, &[]) {
        if let (Some(ver), Some(a)) = (
            v.get("version").and_then(|x| x.as_str()),
            v.pointer(&format!("/assets/{asset}")),
        ) {
            if let Some(url) = a.get("url").and_then(|u| u.as_str()) {
                return Ok(Release {
                    version: ver.trim_start_matches('v').to_string(),
                    url: url.to_string(),
                    sha256: a.get("sha256").and_then(|s| s.as_str()).map(str::to_string),
                    notes: v
                        .get("notes")
                        .and_then(|n| n.as_str())
                        .unwrap_or("")
                        .to_string(),
                });
            }
        }
    }
    let v = net::get_json(
        agent,
        RELEASE_FEED_FALLBACK,
        &[("Accept", "application/vnd.github+json")],
    )
    .map_err(|e| e.to_string())?;
    let ver = v
        .get("tag_name")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .trim_start_matches('v')
        .to_string();
    let url = v
        .get("assets")
        .and_then(|a| a.as_array())
        .and_then(|a| {
            a.iter()
                .find(|x| x.get("name").and_then(|n| n.as_str()) == Some(asset.as_str()))
        })
        .and_then(|x| x.get("browser_download_url"))
        .and_then(|u| u.as_str())
        .ok_or_else(|| format!("release {ver} has no asset {asset}"))?
        .to_string();
    Ok(Release {
        version: ver,
        url,
        sha256: None,
        notes: v
            .get("body")
            .and_then(|b| b.as_str())
            .unwrap_or("")
            .to_string(),
    })
}

/// Download `rel` and swap it in for the running executable.
pub fn apply(
    agent: &ureq::Agent,
    rel: &Release,
    progress: &dyn net::Progress,
) -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    let tmp = exe.with_extension(format!("new-{}", std::process::id()));
    let fetch = |url: &str| {
        net::download(
            agent,
            url,
            &tmp,
            rel.sha256.as_deref(),
            &[],
            "pour",
            progress,
        )
    };
    if let Err(e) = fetch(&rel.url) {
        // A CDN may still hold the previous binary for a few minutes after a
        // release while the feed is already new: fetch past the cache once.
        let fresh = match (&e, &rel.sha256) {
            (net::NetError::Checksum { .. }, Some(sha)) => {
                let sep = if rel.url.contains('?') { '&' } else { '?' };
                Some(format!("{}{sep}v={}", rel.url, &sha[..sha.len().min(12)]))
            }
            _ => None,
        };
        let _ = std::fs::remove_file(&tmp);
        match fresh {
            Some(url) => fetch(&url).map_err(|e| e.to_string())?,
            None => return Err(e.to_string()),
        };
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| e.to_string())?;
    }
    // Keep the old binary one step away in case the new one is broken.
    let old = exe.with_extension("old");
    let _ = std::fs::remove_file(&old);
    std::fs::rename(&exe, &old).map_err(|e| format!("cannot replace {}: {e}", exe.display()))?;
    if let Err(e) = std::fs::rename(&tmp, &exe) {
        let _ = std::fs::rename(&old, &exe);
        return Err(e.to_string());
    }
    let _ = std::fs::remove_file(&old);
    Ok(exe)
}

/// Compare dotted versions: `Some(true)` when `b` is newer than `a`.
pub fn is_newer(a: &str, b: &str) -> bool {
    let parse = |s: &str| -> Vec<u64> {
        s.trim_start_matches('v')
            .split(['.', '-', '_'])
            .map(|x| {
                x.chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect::<String>()
                    .parse()
                    .unwrap_or(0)
            })
            .collect()
    };
    let (pa, pb) = (parse(a), parse(b));
    for i in 0..pa.len().max(pb.len()) {
        let (x, y) = (
            pa.get(i).copied().unwrap_or(0),
            pb.get(i).copied().unwrap_or(0),
        );
        if y != x {
            return y > x;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_versions() {
        assert_eq!(
            asset_name(&Platform {
                os: Os::MacOs,
                arch: Arch::Arm64,
                macos_major: 26,
                wsl: false,
                ish: false
            })
            .unwrap(),
            "pour-macos-arm64"
        );
        assert_eq!(
            asset_name(&Platform {
                os: Os::Windows,
                arch: Arch::X86_64,
                macos_major: 0,
                wsl: false,
                ish: false
            })
            .unwrap(),
            "pour-windows-x86_64.exe"
        );
        assert!(is_newer("0.1.0", "0.1.1"));
        assert!(is_newer("0.9.9", "1.0.0"));
        assert!(!is_newer("1.0.0", "1.0.0"));
        assert!(!is_newer("1.2.0", "1.1.9"));
    }
}
