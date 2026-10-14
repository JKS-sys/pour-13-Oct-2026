//! Host detection: operating system, CPU, macOS release and the Homebrew
//! bottle tags this machine can pour.
//!
//! Homebrew builds one bottle per `tag` (`arm64_sequoia`, `x86_64_linux`, …).
//! A newer macOS can run a bottle built on an older release of the same
//! architecture, so [`Platform::bottle_tags`] lists every acceptable tag in
//! order of preference, exactly like Homebrew's `Utils::Bottles.tag`.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Os {
    MacOs,
    Linux,
    Windows,
    FreeBsd,
    Haiku,
    Ios,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Arch {
    Arm64,
    X86_64,
    X86,
    Other,
}

/// macOS releases Homebrew knows bottles for, newest first.
/// `(bottle name, major version)`.
pub const MACOS_RELEASES: &[(&str, u32)] = &[
    ("golden_gate", 27),
    ("tahoe", 26),
    ("sequoia", 15),
    ("sonoma", 14),
    ("ventura", 13),
    ("monterey", 12),
    ("big_sur", 11),
    ("catalina", 10),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Platform {
    pub os: Os,
    pub arch: Arch,
    /// macOS major version (26 for Tahoe); 0 elsewhere.
    pub macos_major: u32,
    /// Running inside Windows Subsystem for Linux.
    pub wsl: bool,
    /// Running inside iSH or a-Shell on iOS (Linux user land emulated).
    pub ish: bool,
}

impl Platform {
    /// Detect the machine Pour is running on.
    pub fn detect() -> Platform {
        let os = match std::env::consts::OS {
            "macos" => Os::MacOs,
            "linux" => Os::Linux,
            "windows" => Os::Windows,
            "freebsd" => Os::FreeBsd,
            "haiku" => Os::Haiku,
            "ios" => Os::Ios,
            _ => Os::Other,
        };
        let arch = match std::env::consts::ARCH {
            "aarch64" => Arch::Arm64,
            "x86_64" => Arch::X86_64,
            "x86" => Arch::X86,
            _ => Arch::Other,
        };
        let macos_major = if os == Os::MacOs {
            macos_version().0
        } else {
            0
        };
        let wsl = os == Os::Linux && is_wsl();
        let ish = os == Os::Linux && is_ish();
        Platform {
            os,
            arch,
            macos_major,
            wsl,
            ish,
        }
    }

    /// Bottle tags this machine can pour, best match first.
    /// Returns an empty list where Homebrew publishes no bottles (Windows,
    /// FreeBSD, Haiku, iOS, 32-bit).
    pub fn bottle_tags(&self) -> Vec<String> {
        let mut tags = Vec::new();
        match (self.os, self.arch) {
            (Os::MacOs, Arch::Arm64) => {
                for (name, major) in MACOS_RELEASES {
                    if *major <= self.macos_major {
                        tags.push(format!("arm64_{name}"));
                    }
                }
            }
            (Os::MacOs, Arch::X86_64) => {
                for (name, major) in MACOS_RELEASES {
                    if *major <= self.macos_major {
                        tags.push((*name).to_string());
                    }
                }
            }
            (Os::Linux, Arch::X86_64) => tags.push("x86_64_linux".into()),
            (Os::Linux, Arch::Arm64) => tags.push("arm64_linux".into()),
            _ => {}
        }
        tags.push("all".into());
        tags
    }

    /// The tag Homebrew would call this machine (`arm64_tahoe`, `x86_64_linux`).
    pub fn native_tag(&self) -> String {
        self.bottle_tags()
            .into_iter()
            .next()
            .unwrap_or_else(|| "all".into())
    }

    /// Cask variation keys for this machine, most specific first
    /// (`arm64_tahoe`, `tahoe`, …). Casks are macOS and Linux only.
    pub fn cask_variations(&self) -> Vec<String> {
        let mut v = Vec::new();
        match (self.os, self.arch) {
            (Os::MacOs, Arch::Arm64) => {
                for (name, major) in MACOS_RELEASES {
                    if *major == self.macos_major {
                        v.push(format!("arm64_{name}"));
                        v.push((*name).to_string());
                    }
                }
            }
            (Os::MacOs, Arch::X86_64) => {
                for (name, major) in MACOS_RELEASES {
                    if *major == self.macos_major {
                        v.push((*name).to_string());
                    }
                }
            }
            (Os::Linux, Arch::X86_64) => v.push("x86_64_linux".into()),
            (Os::Linux, Arch::Arm64) => v.push("arm64_linux".into()),
            _ => {}
        }
        v
    }

    /// Can this machine pour Homebrew bottles at all?
    pub fn pours_bottles(&self) -> bool {
        matches!(
            (self.os, self.arch),
            (Os::MacOs, Arch::Arm64 | Arch::X86_64) | (Os::Linux, Arch::Arm64 | Arch::X86_64)
        ) && !self.ish
    }

    /// Can this machine install Homebrew casks (macOS apps)?
    pub fn installs_casks(&self) -> bool {
        self.os == Os::MacOs
    }

    /// Short human name, e.g. `macOS 26 (arm64)`.
    pub fn describe(&self) -> String {
        let os = match self.os {
            Os::MacOs => {
                let name = MACOS_RELEASES
                    .iter()
                    .find(|(_, m)| *m == self.macos_major)
                    .map(|(n, _)| title_case(n))
                    .unwrap_or_default();
                if name.is_empty() {
                    format!("macOS {}", self.macos_major)
                } else {
                    format!("macOS {} {}", self.macos_major, name)
                }
            }
            Os::Linux if self.wsl => "Linux (WSL)".to_string(),
            Os::Linux if self.ish => "Linux (iSH on iOS)".to_string(),
            Os::Linux => "Linux".to_string(),
            Os::Windows => "Windows".to_string(),
            Os::FreeBsd => "FreeBSD".to_string(),
            Os::Haiku => "Haiku".to_string(),
            Os::Ios => "iOS".to_string(),
            Os::Other => std::env::consts::OS.to_string(),
        };
        format!("{os} ({})", self.arch)
    }

    /// The Homebrew prefix bottles were built for on this platform; text and
    /// binaries still carrying it are rewritten to Pour's prefix.
    pub fn homebrew_build_prefix(&self) -> &'static str {
        match (self.os, self.arch) {
            (Os::MacOs, Arch::Arm64) => "/opt/homebrew",
            (Os::MacOs, _) => "/usr/local",
            _ => "/home/linuxbrew/.linuxbrew",
        }
    }
}

impl fmt::Display for Arch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Arch::Arm64 => "arm64",
            Arch::X86_64 => "x86_64",
            Arch::X86 => "i686",
            Arch::Other => std::env::consts::ARCH,
        })
    }
}

impl fmt::Display for Os {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Os::MacOs => "macos",
            Os::Linux => "linux",
            Os::Windows => "windows",
            Os::FreeBsd => "freebsd",
            Os::Haiku => "haiku",
            Os::Ios => "ios",
            Os::Other => "other",
        })
    }
}

fn title_case(s: &str) -> String {
    s.split('_')
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// `(major, minor)` of the running macOS, read from the system version
/// plist so no process is spawned. `(0, 0)` elsewhere.
pub fn macos_version() -> (u32, u32) {
    if let Ok(v) = std::env::var("POUR_MACOS_VERSION") {
        return parse_version(&v);
    }
    let plist = std::fs::read_to_string("/System/Library/CoreServices/SystemVersion.plist")
        .unwrap_or_default();
    if let Some(i) = plist.find("<key>ProductVersion</key>") {
        let rest = &plist[i..];
        if let (Some(s), Some(e)) = (rest.find("<string>"), rest.find("</string>")) {
            return parse_version(&rest[s + 8..e]);
        }
    }
    (0, 0)
}

fn parse_version(v: &str) -> (u32, u32) {
    let mut it = v.trim().split('.');
    let major = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
    let minor = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
    (major, minor)
}

fn is_wsl() -> bool {
    std::fs::read_to_string("/proc/version")
        .map(|v| v.to_ascii_lowercase().contains("microsoft"))
        .unwrap_or(false)
}

fn is_ish() -> bool {
    std::fs::read_to_string("/proc/ish/version").is_ok()
        || std::fs::read_to_string("/proc/version")
            .map(|v| v.contains("iSH"))
            .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arm64_tahoe_accepts_older_bottles_newest_first() {
        let p = Platform {
            os: Os::MacOs,
            arch: Arch::Arm64,
            macos_major: 26,
            wsl: false,
            ish: false,
        };
        let tags = p.bottle_tags();
        assert_eq!(tags[0], "arm64_tahoe");
        assert_eq!(tags[1], "arm64_sequoia");
        assert!(tags.contains(&"arm64_big_sur".to_string()));
        assert!(!tags.contains(&"arm64_golden_gate".to_string()));
        assert_eq!(tags.last().unwrap(), "all");
    }

    #[test]
    fn linux_has_one_tag_plus_all() {
        let p = Platform {
            os: Os::Linux,
            arch: Arch::X86_64,
            macos_major: 0,
            wsl: false,
            ish: false,
        };
        assert_eq!(
            p.bottle_tags(),
            vec!["x86_64_linux".to_string(), "all".to_string()]
        );
        assert!(p.pours_bottles());
    }

    #[test]
    fn windows_pours_nothing() {
        let p = Platform {
            os: Os::Windows,
            arch: Arch::X86_64,
            macos_major: 0,
            wsl: false,
            ish: false,
        };
        assert!(!p.pours_bottles());
        assert_eq!(p.bottle_tags(), vec!["all".to_string()]);
    }

    #[test]
    fn version_parsing() {
        assert_eq!(parse_version("26.0.1"), (26, 0));
        assert_eq!(parse_version("15.6"), (15, 6));
    }
}
