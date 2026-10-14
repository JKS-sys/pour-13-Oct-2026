//! Registries: the core index (every Homebrew formula and cask, rebuilt
//! every few hours by `pour-registry`) plus any number of taps — small
//! JSON registries such as `pour-apps`, compiled locally into the same
//! binary format. `pour update` refreshes them all with conditional GETs.

use crate::api;
use crate::index::{Hit, Index, IndexBuilder, IndexError};
use crate::model::{Kind, Pkg};
use crate::net;
use crate::paths::Paths;
use flate2::read::MultiGzDecoder;
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::{Path, PathBuf};

pub const CORE_INDEX_URLS: &[&str] = &[
    "https://ipconfig.co.network/pour/registry/index.pour.gz",
    "https://github.com/JKS-sys/pour-registry-13-Oct-2026/releases/latest/download/index.pour.gz",
];

pub const DEFAULT_TAPS: &[(&str, &str)] = &[(
    "jks-sys/pour-apps",
    "https://ipconfig.co.network/pour/apps/index.json",
)];
pub const TAP_FALLBACKS: &[(&str, &str)] = &[(
    "jks-sys/pour-apps",
    "https://raw.githubusercontent.com/JKS-sys/pour-apps-13-Oct-2026/main/index.json",
)];

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("no index yet — run `pour update` first")]
    Missing,
    #[error(transparent)]
    Index(#[from] IndexError),
    #[error(transparent)]
    Net(#[from] net::NetError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, RegistryError>;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Tap {
    pub name: String,
    pub url: String,
}

pub struct Source {
    pub name: String,
    pub index: Index,
}

pub struct Registry {
    pub sources: Vec<Source>,
}

/// Tap list on disk (`taps.json`); the defaults when the file is missing.
pub fn taps(paths: &Paths) -> Vec<Tap> {
    let file = paths.state().join("taps.json");
    if let Ok(text) = std::fs::read_to_string(&file) {
        if let Ok(t) = serde_json::from_str::<Vec<Tap>>(&text) {
            return t;
        }
    }
    DEFAULT_TAPS
        .iter()
        .map(|(n, u)| Tap {
            name: n.to_string(),
            url: u.to_string(),
        })
        .collect()
}

pub fn save_taps(paths: &Paths, list: &[Tap]) -> std::io::Result<()> {
    std::fs::create_dir_all(paths.state())?;
    std::fs::write(
        paths.state().join("taps.json"),
        serde_json::to_vec_pretty(list).unwrap_or_default(),
    )
}

fn tap_file(paths: &Paths, name: &str) -> PathBuf {
    paths
        .taps_dir()
        .join(format!("{}.pour", name.replace('/', "--")))
}

impl Registry {
    /// Load the core index and every tap that has been fetched.
    pub fn load(paths: &Paths) -> Result<Registry> {
        let core_path = paths.index_file();
        if !core_path.is_file() {
            return Err(RegistryError::Missing);
        }
        let core = match Index::open(&core_path) {
            Ok(i) => i,
            Err(IndexError::BadMagic) | Err(IndexError::Corrupt(_)) => {
                // Older schema or damaged file: fetch a fresh one.
                let _ = std::fs::remove_file(&core_path);
                let _ = std::fs::remove_file(core_path.with_extension("etag"));
                return Err(RegistryError::Missing);
            }
            Err(e) => return Err(e.into()),
        };
        let mut sources = vec![Source {
            name: "homebrew".into(),
            index: core,
        }];
        for tap in taps(paths) {
            let f = tap_file(paths, &tap.name);
            if f.is_file() {
                if let Ok(index) = Index::open(&f) {
                    sources.push(Source {
                        name: tap.name,
                        index,
                    });
                }
            }
        }
        Ok(Registry { sources })
    }

    pub fn core(&self) -> &Index {
        &self.sources[0].index
    }

    /// `name`, `tap/name` or `user/repo/name`; formula before cask unless a
    /// kind is forced.
    pub fn find(&self, spec: &str, kind: Option<Kind>) -> Option<(String, Pkg)> {
        let (tap, name) = split_spec(spec);
        for s in &self.sources {
            if let Some(t) = tap {
                if !source_matches(&s.name, t) {
                    continue;
                }
            }
            let hit = match kind {
                Some(k) => s.index.find(k, name),
                None => s.index.find_any(name),
            };
            if let Some(p) = hit {
                return Some((s.name.clone(), p));
            }
        }
        None
    }

    pub fn search(&self, query: &str, kind: Option<Kind>, limit: usize) -> Vec<(String, Hit)> {
        let mut all: Vec<(String, Hit)> = Vec::new();
        for s in &self.sources {
            for h in s.index.search(query, kind, limit) {
                all.push((s.name.clone(), h));
            }
        }
        all.sort_by(|a, b| {
            b.1.score
                .cmp(&a.1.score)
                .then_with(|| a.1.name.cmp(&b.1.name))
        });
        all.truncate(limit);
        all
    }

    pub fn suggest(&self, name: &str, limit: usize) -> Vec<(Kind, String)> {
        let mut out = Vec::new();
        for s in &self.sources {
            out.extend(s.index.suggest(name, limit));
        }
        out.truncate(limit);
        out
    }

    pub fn counts(&self) -> (usize, usize) {
        self.sources.iter().fold((0, 0), |(f, c), s| {
            (f + s.index.len(Kind::Formula), c + s.index.len(Kind::Cask))
        })
    }
}

fn source_matches(source: &str, tap: &str) -> bool {
    let s = source.to_ascii_lowercase();
    let t = tap.to_ascii_lowercase();
    s == t
        || s.ends_with(&format!("/{t}"))
        || (t == "homebrew/core" || t == "homebrew/cask" || t == "core" || t == "cask")
            && s == "homebrew"
}

/// `homebrew/core/jq` → (Some("homebrew/core"), "jq"); `jq` → (None, "jq").
pub fn split_spec(spec: &str) -> (Option<&str>, &str) {
    match spec.rfind('/') {
        Some(i) if i > 0 => (Some(&spec[..i]), &spec[i + 1..]),
        _ => (None, spec),
    }
}

/// Build an index from Homebrew-API-shaped JSON. `formulae` and `casks`
/// are arrays of API objects; either may be `Null`.
pub fn build_from_json(
    formulae: &serde_json::Value,
    casks: &serde_json::Value,
    meta: serde_json::Value,
) -> Result<Vec<u8>> {
    let mut b = IndexBuilder::new();
    if let Some(a) = formulae.as_array() {
        for v in a {
            if let Some(f) = api::formula_from_api(v) {
                b.add_formula(f);
            }
        }
    }
    if let Some(a) = casks.as_array() {
        for v in a {
            if let Some(c) = api::cask_from_api(v) {
                b.add_cask(c);
            }
        }
    }
    let (nf, nc) = b.counts();
    let mut meta = meta;
    if let Some(o) = meta.as_object_mut() {
        o.insert("formulae".into(), nf.into());
        o.insert("casks".into(), nc.into());
    }
    b.meta = meta;
    Ok(b.build()?)
}

/// Accept a raw index, a gzip-compressed index, or registry JSON
/// (`{"formulae": [...], "casks": [...]}` or a bare array) and return a
/// raw index.
pub fn normalize_payload(bytes: &[u8], source: &str) -> Result<Vec<u8>> {
    if bytes.starts_with(crate::index::MAGIC) {
        return Ok(bytes.to_vec());
    }
    if bytes.starts_with(&[0x1f, 0x8b]) {
        let mut out = Vec::new();
        MultiGzDecoder::new(bytes).read_to_end(&mut out)?;
        return normalize_payload(&out, source);
    }
    let v: serde_json::Value = serde_json::from_slice(bytes).map_err(|e| {
        RegistryError::Other(format!("{source}: not an index or JSON registry: {e}"))
    })?;
    let (f, c) = match &v {
        serde_json::Value::Array(_) => {
            // A bare array: casks have "token", formulae have "name" + "versions".
            let f: Vec<_> = v
                .as_array()
                .unwrap()
                .iter()
                .filter(|x| x.get("token").is_none())
                .cloned()
                .collect();
            let c: Vec<_> = v
                .as_array()
                .unwrap()
                .iter()
                .filter(|x| x.get("token").is_some())
                .cloned()
                .collect();
            (serde_json::Value::Array(f), serde_json::Value::Array(c))
        }
        serde_json::Value::Object(o) => (
            o.get("formulae")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
            o.get("casks").cloned().unwrap_or(serde_json::Value::Null),
        ),
        _ => return Err(RegistryError::Other(format!("{source}: unexpected JSON"))),
    };
    let meta = v
        .get("meta")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({"source": source}));
    build_from_json(&f, &c, meta)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Updated { formulae: usize, casks: usize },
    Unchanged,
    Failed(String),
}

#[derive(Debug, Default)]
pub struct UpdateReport {
    pub core: Option<Status>,
    pub taps: Vec<(String, Status)>,
}

fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&tmp, data)?;
    std::fs::rename(tmp, path)
}

fn fetch_into(
    agent: &ureq::Agent,
    urls: &[String],
    dest: &Path,
    source: &str,
    force: bool,
) -> Status {
    let etag_file = dest.with_extension("etag");
    let etag = if force || !dest.is_file() {
        None
    } else {
        std::fs::read_to_string(&etag_file).ok()
    };
    let mut last_err = String::new();
    for url in urls {
        match net::get_if_changed(agent, url, etag.as_deref()) {
            Ok(None) => return Status::Unchanged,
            Ok(Some((body, tag))) => match normalize_payload(&body, source) {
                Ok(raw) => match Index::from_vec(raw.clone()) {
                    Ok(idx) => {
                        let (f, c) = (idx.len(Kind::Formula), idx.len(Kind::Cask));
                        drop(idx);
                        if let Err(e) = write_atomic(dest, &raw) {
                            return Status::Failed(e.to_string());
                        }
                        match tag {
                            Some(t) => {
                                let _ = std::fs::write(&etag_file, t);
                            }
                            None => {
                                let _ = std::fs::remove_file(&etag_file);
                            }
                        }
                        return Status::Updated {
                            formulae: f,
                            casks: c,
                        };
                    }
                    Err(e) => last_err = format!("{url}: {e}"),
                },
                Err(e) => last_err = e.to_string(),
            },
            Err(e) => last_err = e.to_string(),
        }
    }
    Status::Failed(last_err)
}

/// Keep only what this machine can use: the bottle digests for its own
/// tags and the cask variations for its own macOS versions. Every Homebrew
/// record carries six or more platforms; dropping the others roughly halves
/// the index on disk without changing a single answer on this machine.
pub fn slim_index_file(
    path: &Path,
    platform: &crate::platform::Platform,
) -> std::io::Result<usize> {
    let idx = Index::open(path).map_err(|e| std::io::Error::other(e.to_string()))?;
    let tags = platform.bottle_tags();
    let vars = platform.cask_variations();
    let mut b = IndexBuilder::new();
    b.meta = idx.meta();
    for i in 0..idx.len(Kind::Formula) {
        if let Ok(Pkg::Formula(mut f)) = idx.get(Kind::Formula, i) {
            f.bottles.retain(|bt| tags.contains(&bt.tag));
            b.add_formula(f);
        }
    }
    for i in 0..idx.len(Kind::Cask) {
        if let Ok(Pkg::Cask(mut c)) = idx.get(Kind::Cask, i) {
            c.variations.retain(|v| vars.contains(&v.tag));
            b.add_cask(c);
        }
    }
    drop(idx);
    let bytes = b
        .build()
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let n = bytes.len();
    write_atomic(path, &bytes)?;
    Ok(n)
}

/// Refresh the core index and every tap.
pub fn update(paths: &Paths, force: bool) -> UpdateReport {
    let agent = net::agent();
    let mut report = UpdateReport::default();
    let core_urls: Vec<String> = match std::env::var("POUR_INDEX_URL") {
        Ok(u) if !u.trim().is_empty() => vec![u],
        _ => CORE_INDEX_URLS.iter().map(|s| s.to_string()).collect(),
    };
    report.core = Some(fetch_into(
        &agent,
        &core_urls,
        &paths.index_file(),
        "homebrew",
        force,
    ));
    if matches!(report.core, Some(Status::Updated { .. }))
        && std::env::var_os("POUR_FULL_INDEX").is_none()
    {
        let platform = crate::platform::Platform::detect();
        let _ = slim_index_file(&paths.index_file(), &platform);
    }
    let list = taps(paths);
    let _ = std::fs::create_dir_all(paths.taps_dir());
    for tap in list {
        let mut urls = vec![tap.url.clone()];
        for (n, u) in TAP_FALLBACKS {
            if *n == tap.name && !urls.contains(&u.to_string()) {
                urls.push(u.to_string());
            }
        }
        let st = fetch_into(&agent, &urls, &tap_file(paths, &tap.name), &tap.name, force);
        report.taps.push((tap.name, st));
    }
    report
}

/// Install a registry payload from a local file or raw bytes (offline use,
/// tests, `pour registry import`).
pub fn import_core(paths: &Paths, bytes: &[u8]) -> Result<(usize, usize)> {
    let raw = normalize_payload(bytes, "import")?;
    let idx = Index::from_vec(raw.clone())?;
    let counts = (idx.len(Kind::Formula), idx.len(Kind::Cask));
    drop(idx);
    write_atomic(&paths.index_file(), &raw)?;
    let _ = std::fs::remove_file(paths.index_file().with_extension("etag"));
    Ok(counts)
}

pub fn import_tap(paths: &Paths, name: &str, bytes: &[u8]) -> Result<(usize, usize)> {
    let raw = normalize_payload(bytes, name)?;
    let idx = Index::from_vec(raw.clone())?;
    let counts = (idx.len(Kind::Formula), idx.len(Kind::Cask));
    drop(idx);
    write_atomic(&tap_file(paths, name), &raw)?;
    Ok(counts)
}

pub fn remove_tap_file(paths: &Paths, name: &str) {
    let _ = std::fs::remove_file(tap_file(paths, name));
    let _ = std::fs::remove_file(tap_file(paths, name).with_extension("etag"));
}

/// gzip an index for publishing.
pub fn gzip(data: &[u8]) -> std::io::Result<Vec<u8>> {
    use flate2::write::GzEncoder;
    use std::io::Write;
    let mut enc = GzEncoder::new(Vec::new(), flate2::Compression::best());
    enc.write_all(data)?;
    enc.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn registry_json() -> Vec<u8> {
        json!({"meta": {"source": "test"}, "formulae": [
            {"name": "jq", "full_name": "jq", "tap": "homebrew/core", "desc": "JSON", "homepage": "h", "versions": {"stable": "1.8.2"}, "revision": 0,
             "bottle": {"stable": {"rebuild": 0, "root_url": "https://ghcr.io/v2/homebrew/core", "files": {"all": {"cellar": ":any_skip_relocation", "sha256": "ab".repeat(32)}}}},
             "dependencies": [], "build_dependencies": [], "uses_from_macos": []}
        ], "casks": [
            {"token": "peek", "full_token": "jks-sys/pour-apps/peek", "tap": "jks-sys/pour-apps", "name": ["Peek"], "desc": "Viewer", "homepage": "h", "version": "1", "sha256": "no_check", "url": "u", "artifacts": [{"app": ["Peek.app"]}]}
        ]}).to_string().into_bytes()
    }

    #[test]
    fn import_and_lookup_across_sources() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path().join("pour"));
        paths.ensure().unwrap();
        assert!(matches!(
            Registry::load(&paths),
            Err(RegistryError::Missing)
        ));
        let core = build_from_json(&json!([{"name": "wget", "full_name": "wget", "tap": "homebrew/core", "desc": "Internet file retriever", "homepage": "h", "versions": {"stable": "1.25.0"}, "revision": 0, "dependencies": ["openssl@3"], "build_dependencies": [], "uses_from_macos": []}]), &json!(null), json!({"source": "core"})).unwrap();
        let gz = gzip(&core).unwrap();
        let (f, c) = import_core(&paths, &gz).unwrap();
        assert_eq!((f, c), (1, 0));
        let (f, c) = import_tap(&paths, "jks-sys/pour-apps", &registry_json()).unwrap();
        assert_eq!((f, c), (1, 1));
        save_taps(
            &paths,
            &[Tap {
                name: "jks-sys/pour-apps".into(),
                url: "x".into(),
            }],
        )
        .unwrap();
        let reg = Registry::load(&paths).unwrap();
        assert_eq!(reg.sources.len(), 2);
        assert_eq!(reg.counts(), (2, 1));
        assert_eq!(reg.find("wget", None).unwrap().0, "homebrew");
        assert_eq!(reg.find("peek", None).unwrap().1.kind(), Kind::Cask);
        assert_eq!(
            reg.find("jks-sys/pour-apps/jq", None).unwrap().0,
            "jks-sys/pour-apps"
        );
        assert!(
            reg.find("homebrew/core/jq", None).is_none(),
            "qualified lookups stay in their tap"
        );
        assert_eq!(reg.search("json", None, 5)[0].1.name, "jq");
        assert_eq!(split_spec("a/b/c"), (Some("a/b"), "c"));
        assert_eq!(split_spec("jq"), (None, "jq"));
    }
}
