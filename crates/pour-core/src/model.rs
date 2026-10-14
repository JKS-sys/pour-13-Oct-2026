//! Package records as stored in the index. Complex nested Homebrew data
//! (artifacts, services, post-install steps) is kept as raw JSON text and
//! decoded only when it is needed, so a lookup never pays for it.

use serde::{Deserialize, Serialize};

/// Where a bottle expects to live. Homebrew calls this the cellar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Cellar {
    /// Needs no relocation at all: no prefix paths inside.
    AnySkipRelocation,
    /// Relocatable: text and load commands carry `@@HOMEBREW_*@@` placeholders.
    Any,
    /// Built for one exact cellar path; Pour relocates it deeply.
    Fixed(String),
}

impl Cellar {
    pub fn parse(s: &str) -> Cellar {
        match s {
            ":any_skip_relocation" => Cellar::AnySkipRelocation,
            ":any" => Cellar::Any,
            other => Cellar::Fixed(other.to_string()),
        }
    }
    pub fn as_str(&self) -> String {
        match self {
            Cellar::AnySkipRelocation => ":any_skip_relocation".into(),
            Cellar::Any => ":any".into(),
            Cellar::Fixed(p) => p.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bottle {
    pub tag: String,
    pub cellar: Cellar,
    /// Lowercase hex SHA-256 of the bottle tarball; also the ghcr blob digest.
    /// Stored as 32 raw bytes in the index.
    #[serde(with = "hex32")]
    pub sha256: String,
}

/// Serialize a 64-character hex digest as 32 bytes (half the size, and
/// incompressible hex no longer bloats the gzip).
pub mod hex32 {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(v: &str, s: S) -> Result<S::Ok, S::Error> {
        match hex::decode(v) {
            Ok(b) if b.len() == 32 => serde_bytes_like::Bytes(b).serialize(s),
            _ => serde_bytes_like::Bytes(v.as_bytes().to_vec()).serialize(s),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
        let b = serde_bytes_like::Bytes::deserialize(d)?.0;
        Ok(if b.len() == 32 {
            hex::encode(b)
        } else {
            String::from_utf8_lossy(&b).into_owned()
        })
    }

    /// Minimal `Bytes` newtype so postcard writes a length-prefixed byte run
    /// instead of a sequence of u8s.
    mod serde_bytes_like {
        use serde::de::{self, Visitor};
        use serde::{Deserialize, Deserializer, Serialize, Serializer};
        pub struct Bytes(pub Vec<u8>);
        impl Serialize for Bytes {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_bytes(&self.0)
            }
        }
        impl<'de> Deserialize<'de> for Bytes {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                struct V;
                impl<'de> Visitor<'de> for V {
                    type Value = Bytes;
                    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                        f.write_str("bytes")
                    }
                    fn visit_bytes<E: de::Error>(self, v: &[u8]) -> Result<Bytes, E> {
                        Ok(Bytes(v.to_vec()))
                    }
                    fn visit_byte_buf<E: de::Error>(self, v: Vec<u8>) -> Result<Bytes, E> {
                        Ok(Bytes(v))
                    }
                    fn visit_seq<A: de::SeqAccess<'de>>(
                        self,
                        mut seq: A,
                    ) -> Result<Bytes, A::Error> {
                        let mut out = Vec::new();
                        while let Some(b) = seq.next_element::<u8>()? {
                            out.push(b);
                        }
                        Ok(Bytes(out))
                    }
                }
                d.deserialize_bytes(V)
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsesFromMacos {
    pub name: String,
    /// `build`, `test` or empty for a run-time dependency.
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Requirement {
    pub name: String,
    pub version: Option<String>,
    pub cask: Option<String>,
    pub download: Option<String>,
    pub contexts: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Formula {
    pub name: String,
    pub full_name: String,
    pub tap: String,
    pub desc: String,
    pub homepage: String,
    pub license: String,
    pub version: String,
    pub revision: u32,
    pub version_scheme: u32,
    pub aliases: Vec<String>,
    pub oldnames: Vec<String>,
    /// Run-time dependencies on macOS.
    pub deps: Vec<String>,
    pub build_deps: Vec<String>,
    pub uses_from_macos: Vec<UsesFromMacos>,
    /// Run-time dependencies on Linux when they differ (`variations`).
    pub linux_deps: Option<Vec<String>>,
    pub bottles: Vec<Bottle>,
    pub root_url: String,
    pub rebuild: u32,
    pub keg_only: bool,
    pub keg_only_reason: Option<String>,
    pub caveats: Option<String>,
    /// `service` block as JSON text.
    pub service: Option<String>,
    /// Declarative `post_install_steps` as JSON text.
    pub post_install: Option<String>,
    pub conflicts_with: Vec<String>,
    pub deprecated: bool,
    pub deprecation_reason: Option<String>,
    pub disabled: bool,
    pub disable_reason: Option<String>,
    pub requirements: Vec<Requirement>,
    pub pour_bottle_only_if: Option<String>,
    pub executables: Vec<String>,
    pub link_overwrite: Vec<String>,
}

impl Formula {
    /// `1.8.2` or `1.8.2_1` when there is a revision, the way Homebrew names kegs.
    pub fn pkg_version(&self) -> String {
        if self.revision > 0 {
            format!("{}_{}", self.version, self.revision)
        } else {
            self.version.clone()
        }
    }

    /// Pick the best bottle for the given tag preference list.
    pub fn bottle_for(&self, tags: &[String]) -> Option<&Bottle> {
        tags.iter()
            .find_map(|t| self.bottles.iter().find(|b| &b.tag == t))
    }

    /// Run-time dependencies for the given OS.
    pub fn runtime_deps(&self, linux: bool) -> Vec<String> {
        let mut deps: Vec<String> = match (&self.linux_deps, linux) {
            (Some(l), true) => l.clone(),
            _ => self.deps.clone(),
        };
        if linux {
            for u in &self.uses_from_macos {
                if u.kind.is_empty() && !deps.contains(&u.name) {
                    deps.push(u.name.clone());
                }
            }
        }
        deps
    }
}

/// One platform-specific download of a cask (`variations` in the API).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaskVariation {
    pub tag: String,
    pub url: Option<String>,
    pub sha256: Option<String>,
    pub version: Option<String>,
    /// Replacement `artifacts` JSON when the variation changes them.
    pub artifacts: Option<String>,
}

/// An installer Pour can run on a system Homebrew does not cover
/// (Windows, Linux desktop, FreeBSD). Used by Pour-native registries such
/// as `pour-apps`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeInstaller {
    /// `windows-x86_64`, `linux-x86_64`, `linux-arm64`, `freebsd-x86_64`, …
    pub platform: String,
    pub url: String,
    pub sha256: Option<String>,
    /// `exe`, `msi`, `deb`, `appimage`, `tar.gz`, `zip`, `dmg`.
    pub kind: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cask {
    pub token: String,
    pub full_token: String,
    pub tap: String,
    pub names: Vec<String>,
    pub old_tokens: Vec<String>,
    pub desc: String,
    pub homepage: String,
    pub version: String,
    /// Lowercase hex, or `None` for `sha256 :no_check`.
    pub sha256: Option<String>,
    pub url: String,
    pub container: Option<String>,
    /// `depends_on` as JSON text (`{"macos": {">=": ["13"]}, "arch": [...]}`).
    pub depends_on: Option<String>,
    /// `artifacts` as JSON text.
    pub artifacts: String,
    pub auto_updates: bool,
    pub caveats: Option<String>,
    pub conflicts_with: Option<String>,
    pub deprecated: bool,
    pub deprecation_reason: Option<String>,
    pub disabled: bool,
    pub disable_reason: Option<String>,
    pub variations: Vec<CaskVariation>,
    pub languages: Vec<String>,
    pub native: Vec<NativeInstaller>,
    /// Optional live version feed (`latest.json`) Pour-native registries use.
    pub livecheck_url: Option<String>,
    /// Pour-native registries: remove the Gatekeeper quarantine mark after
    /// placing the app (for apps not notarised with Apple).
    pub clear_quarantine: bool,
}

impl Cask {
    /// The download for this machine: the matching variation wins over the
    /// default `url`/`sha256`/`version`.
    pub fn download_for(
        &self,
        variations: &[String],
    ) -> (String, Option<String>, String, Option<String>) {
        for v in variations {
            if let Some(var) = self.variations.iter().find(|x| &x.tag == v) {
                return (
                    var.url.clone().unwrap_or_else(|| self.url.clone()),
                    var.sha256.clone().or_else(|| self.sha256.clone()),
                    var.version.clone().unwrap_or_else(|| self.version.clone()),
                    var.artifacts.clone(),
                );
            }
        }
        (
            self.url.clone(),
            self.sha256.clone(),
            self.version.clone(),
            None,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Kind {
    Formula,
    Cask,
}

impl std::fmt::Display for Kind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Kind::Formula => "formula",
            Kind::Cask => "cask",
        })
    }
}

/// Either record, as handed out by index lookups.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pkg {
    Formula(Formula),
    Cask(Cask),
}

impl Pkg {
    pub fn kind(&self) -> Kind {
        match self {
            Pkg::Formula(_) => Kind::Formula,
            Pkg::Cask(_) => Kind::Cask,
        }
    }
    pub fn name(&self) -> &str {
        match self {
            Pkg::Formula(f) => &f.name,
            Pkg::Cask(c) => &c.token,
        }
    }
    pub fn desc(&self) -> &str {
        match self {
            Pkg::Formula(f) => &f.desc,
            Pkg::Cask(c) => &c.desc,
        }
    }
    pub fn homepage(&self) -> &str {
        match self {
            Pkg::Formula(f) => &f.homepage,
            Pkg::Cask(c) => &c.homepage,
        }
    }
    pub fn version(&self) -> String {
        match self {
            Pkg::Formula(f) => f.pkg_version(),
            Pkg::Cask(c) => c.version.clone(),
        }
    }
    pub fn tap(&self) -> &str {
        match self {
            Pkg::Formula(f) => &f.tap,
            Pkg::Cask(c) => &c.tap,
        }
    }
    pub fn deprecated(&self) -> bool {
        match self {
            Pkg::Formula(f) => f.deprecated,
            Pkg::Cask(c) => c.deprecated,
        }
    }
    pub fn disabled(&self) -> bool {
        match self {
            Pkg::Formula(f) => f.disabled,
            Pkg::Cask(c) => c.disabled,
        }
    }
}
