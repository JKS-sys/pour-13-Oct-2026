//! HTTP: one shared agent, streaming downloads with SHA-256 verification,
//! retries with backoff, and a parallel downloader that fetches a whole
//! install plan at once.

use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Anonymous token GitHub Packages accepts for public Homebrew bottles.
pub const GHCR_AUTH: &str = "Bearer QQ==";

#[derive(Debug, thiserror::Error)]
pub enum NetError {
    #[error("{url}: {msg}")]
    Http { url: String, msg: String },
    #[error("{url}: HTTP {status}")]
    Status { url: String, status: u16 },
    #[error("checksum mismatch for {file}: expected {expected}, got {actual}")]
    Checksum {
        file: String,
        expected: String,
        actual: String,
    },
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, NetError>;

pub fn user_agent() -> String {
    format!(
        "pour/{} ({}; {})",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}

/// Trust roots: Mozilla's bundle plus any PEM bundles the environment names
/// (`POUR_CA_BUNDLE`, `SSL_CERT_FILE`, `CURL_CA_BUNDLE`, `NODE_EXTRA_CA_CERTS`)
/// and the usual Linux system bundles, so TLS-inspecting corporate proxies
/// work the way they do for curl.
pub fn tls_config() -> std::sync::Arc<rustls::ClientConfig> {
    use rustls_pki_types::pem::PemObject;
    use rustls_pki_types::CertificateDer;
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let mut files: Vec<String> = [
        "POUR_CA_BUNDLE",
        "SSL_CERT_FILE",
        "CURL_CA_BUNDLE",
        "NODE_EXTRA_CA_CERTS",
        "REQUESTS_CA_BUNDLE",
    ]
    .iter()
    .filter_map(|k| std::env::var(k).ok())
    .filter(|v| !v.trim().is_empty())
    .collect();
    for sys in [
        "/etc/ssl/certs/ca-certificates.crt",
        "/etc/pki/tls/certs/ca-bundle.crt",
        "/etc/ssl/cert.pem",
        "/usr/local/share/certs/ca-root-nss.crt",
        "/boot/system/data/ssl/CARootCertificates.pem",
    ] {
        if Path::new(sys).is_file() {
            files.push(sys.to_string());
        }
    }
    for f in files {
        if let Ok(iter) = CertificateDer::pem_file_iter(&f) {
            for cert in iter.flatten() {
                let _ = roots.add(cert);
            }
        }
    }
    std::sync::Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
}

pub fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .tls_config(tls_config())
        .user_agent(&user_agent())
        .timeout_connect(Duration::from_secs(20))
        .timeout_read(Duration::from_secs(120))
        .timeout_write(Duration::from_secs(120))
        .max_idle_connections_per_host(8)
        .redirects(8)
        .build()
}

/// Progress sink: bytes received so far and total when known.
pub trait Progress: Send + Sync {
    fn start(&self, _label: &str, _total: Option<u64>) {}
    fn advance(&self, _label: &str, _delta: u64) {}
    fn finish(&self, _label: &str) {}
}

pub struct NoProgress;
impl Progress for NoProgress {}

fn retry<T>(url: &str, mut f: impl FnMut() -> Result<T>) -> Result<T> {
    let mut delay = Duration::from_millis(400);
    let mut last = None;
    for attempt in 0..4 {
        match f() {
            Ok(v) => return Ok(v),
            Err(NetError::Status { status, .. })
                if (400..500).contains(&status) && status != 429 =>
            {
                return Err(NetError::Status {
                    url: url.into(),
                    status,
                })
            }
            Err(e) => {
                last = Some(e);
                if attempt < 3 {
                    std::thread::sleep(delay);
                    delay *= 3;
                }
            }
        }
    }
    Err(last.unwrap_or(NetError::Http {
        url: url.into(),
        msg: "failed".into(),
    }))
}

fn send(agent: &ureq::Agent, url: &str, headers: &[(&str, &str)]) -> Result<ureq::Response> {
    let mut req = agent.get(url);
    for (k, v) in headers {
        req = req.set(k, v);
    }
    match req.call() {
        Ok(r) => Ok(r),
        Err(ureq::Error::Status(code, _)) => Err(NetError::Status {
            url: url.into(),
            status: code,
        }),
        Err(e) => Err(NetError::Http {
            url: url.into(),
            msg: e.to_string(),
        }),
    }
}

/// GET a small text/JSON body.
pub fn get_text(agent: &ureq::Agent, url: &str, headers: &[(&str, &str)]) -> Result<String> {
    retry(url, || {
        let r = send(agent, url, headers)?;
        r.into_string().map_err(|e| NetError::Http {
            url: url.into(),
            msg: e.to_string(),
        })
    })
}

/// POST a JSON body; returns (status, body) for any HTTP answer, Err only when there was none.
pub fn post_json(
    agent: &ureq::Agent,
    url: &str,
    body: &serde_json::Value,
) -> Result<(u16, String)> {
    match agent
        .post(url)
        .set("content-type", "application/json")
        .send_string(&body.to_string())
    {
        Ok(r) => {
            let c = r.status();
            Ok((c, r.into_string().unwrap_or_default()))
        }
        Err(ureq::Error::Status(c, r)) => Ok((c, r.into_string().unwrap_or_default())),
        Err(e) => Err(NetError::Http {
            url: url.into(),
            msg: e.to_string(),
        }),
    }
}

pub fn get_json(
    agent: &ureq::Agent,
    url: &str,
    headers: &[(&str, &str)],
) -> Result<serde_json::Value> {
    let t = get_text(agent, url, headers)?;
    serde_json::from_str(&t).map_err(|e| NetError::Http {
        url: url.into(),
        msg: format!("bad JSON: {e}"),
    })
}

/// Conditional GET: returns `None` when the server answers 304 for `etag`.
/// On success returns the body and the new ETag.
pub fn get_if_changed(
    agent: &ureq::Agent,
    url: &str,
    etag: Option<&str>,
) -> Result<Option<(Vec<u8>, Option<String>)>> {
    retry(url, || {
        let mut req = agent.get(url);
        if let Some(e) = etag {
            req = req.set("If-None-Match", e);
        }
        match req.call() {
            // 304 is below ureq's error threshold: it arrives as Ok with an empty body
            Ok(r) if r.status() == 304 => Ok(None),
            Ok(r) => {
                let tag = r.header("ETag").map(str::to_string);
                let mut body = Vec::new();
                r.into_reader().read_to_end(&mut body)?;
                Ok(Some((body, tag)))
            }
            Err(ureq::Error::Status(304, _)) => Ok(None),
            Err(ureq::Error::Status(code, _)) => Err(NetError::Status {
                url: url.into(),
                status: code,
            }),
            Err(e) => Err(NetError::Http {
                url: url.into(),
                msg: e.to_string(),
            }),
        }
    })
}

/// Stream `url` into `dest`, verifying SHA-256 when `expected` is given.
/// Writes to a temporary sibling and renames, so a half download never
/// looks complete. Returns the hex digest.
pub fn download(
    agent: &ureq::Agent,
    url: &str,
    dest: &Path,
    expected: Option<&str>,
    headers: &[(&str, &str)],
    label: &str,
    progress: &dyn Progress,
) -> Result<String> {
    if let (Some(exp), true) = (expected, dest.is_file()) {
        if let Ok(actual) = sha256_file(dest) {
            if actual == exp {
                return Ok(actual);
            }
        }
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = dest.with_extension(format!("part-{}", std::process::id()));
    let result = retry(url, || {
        let r = send(agent, url, headers)?;
        let total = r
            .header("Content-Length")
            .and_then(|v| v.parse::<u64>().ok());
        progress.start(label, total);
        let mut reader = r.into_reader();
        let mut file = File::create(&tmp)?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            let n = reader.read(&mut buf).map_err(|e| NetError::Http {
                url: url.into(),
                msg: e.to_string(),
            })?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            file.write_all(&buf[..n])?;
            progress.advance(label, n as u64);
        }
        file.flush()?;
        Ok(hex::encode(hasher.finalize()))
    });
    let actual = match result {
        Ok(a) => a,
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            progress.finish(label);
            return Err(e);
        }
    };
    if let Some(exp) = expected {
        if actual != exp.to_ascii_lowercase() {
            let _ = std::fs::remove_file(&tmp);
            progress.finish(label);
            return Err(NetError::Checksum {
                file: dest.display().to_string(),
                expected: exp.into(),
                actual,
            });
        }
    }
    std::fs::rename(&tmp, dest)?;
    progress.finish(label);
    Ok(actual)
}

pub fn sha256_file(p: &Path) -> std::io::Result<String> {
    let mut f = File::open(p)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// One file to fetch as part of a parallel batch.
#[derive(Debug, Clone)]
pub struct Job {
    pub label: String,
    pub url: String,
    pub dest: PathBuf,
    pub sha256: Option<String>,
    pub headers: Vec<(String, String)>,
}

/// Fetch every job, `threads` at a time. Errors are returned per job so one
/// failure does not hide the others.
pub fn download_all(jobs: &[Job], threads: usize, progress: &dyn Progress) -> Vec<Result<String>> {
    use rayon::prelude::*;
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads.max(1))
        .build();
    let agent = agent();
    let run = |j: &Job| {
        let headers: Vec<(&str, &str)> = j
            .headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        download(
            &agent,
            &j.url,
            &j.dest,
            j.sha256.as_deref(),
            &headers,
            &j.label,
            progress,
        )
    };
    match pool {
        Ok(pool) => pool.install(|| jobs.par_iter().map(run).collect()),
        Err(_) => jobs.iter().map(run).collect(),
    }
}

/// Byte counter usable as a shared [`Progress`] when no terminal is attached.
#[derive(Default)]
pub struct Counter {
    pub bytes: Arc<AtomicU64>,
}

impl Progress for Counter {
    fn advance(&self, _label: &str, delta: u64) {
        self.bytes.fetch_add(delta, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_of_known_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a");
        std::fs::write(&p, b"abc").unwrap();
        assert_eq!(
            sha256_file(&p).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn user_agent_names_pour() {
        assert!(user_agent().starts_with("pour/"));
    }
}
