//! The install engine for formulae: download bottles in parallel, unpack
//! and relocate each once into the store, clone into the Cellar, link,
//! write receipts, run post-install steps. Also uninstall.

use crate::bottle;
use crate::extract;
use crate::link;
use crate::lock::Lock;
use crate::model::{Cellar, Kind};
use crate::net::{self, Job, Progress};
use crate::paths::Paths;
use crate::platform::{Os, Platform};
use crate::postinstall;
use crate::receipt::{self, Receipt};
use crate::relocate::{self, Relocation};
use crate::resolve::{Action, Plan, Step};
use crate::store;
use rayon::prelude::*;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Instant;

#[derive(Debug, Clone)]
pub enum Event {
    Phase(String),
    Info(String),
    Warn(String),
    Note(String),
    Done {
        name: String,
        version: String,
        action: Action,
        ms: u128,
    },
}

pub trait Reporter: Progress {
    fn event(&self, e: Event);
}

#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error("another pour is running on this prefix (lock held)")]
    Locked,
    #[error("prefix {0} is not writable — run `pour init` or `sudo chown -R $(whoami) {0}`")]
    Unwritable(String),
    #[error("download failed: {0}")]
    Download(String),
    #[error("{name}: {msg}")]
    Step { name: String, msg: String },
    #[error("{0} is not installed")]
    NotInstalled(String),
    #[error("{name} is required by {by}; pass --ignore-dependencies to remove it anyway")]
    Required { name: String, by: String },
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, InstallError>;

#[derive(Debug, Default)]
pub struct Options {
    pub threads: usize,
    pub skip_post_install: bool,
    pub overwrite_links: bool,
    pub keep_old_versions: bool,
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct Summary {
    pub installed: Vec<(String, String, String)>,
    pub caveats: Vec<(String, String)>,
    pub warnings: Vec<String>,
    pub bytes_downloaded: u64,
    pub store_hits: usize,
    pub ms: u128,
}

pub struct Engine<'a> {
    pub paths: &'a Paths,
    pub platform: &'a Platform,
    pub reporter: &'a dyn Reporter,
}

fn bottle_file(paths: &Paths, s: &Step) -> PathBuf {
    paths.downloads().join(format!(
        "{}--{}.{}.bottle.tar.gz",
        s.formula.name,
        s.formula.pkg_version(),
        s.bottle.tag
    ))
}

impl Engine<'_> {
    fn lock(&self) -> Result<Lock> {
        if !self.paths.writable() {
            if self.paths.prefix.exists() {
                return Err(InstallError::Unwritable(self.paths.prefix_str()));
            }
            self.paths
                .ensure()
                .map_err(|_| InstallError::Unwritable(self.paths.prefix_str()))?;
        }
        self.paths.ensure()?;
        Lock::try_acquire(&self.paths.lock_file())?.ok_or(InstallError::Locked)
    }

    /// Execute a plan.
    pub fn install(&self, plan: &Plan, opts: &Options) -> Result<Summary> {
        let started = Instant::now();
        let _lock = self.lock()?;
        let mut summary = Summary::default();
        for w in &plan.warnings {
            self.reporter.event(Event::Warn(w.clone()));
        }
        let steps: Vec<&Step> = plan.pending().collect();
        if steps.is_empty() {
            summary.ms = started.elapsed().as_millis();
            return Ok(summary);
        }
        if self.platform.os == Os::Linux {
            if let Err(e) = crate::linuxsetup::ensure_ld_so(self.paths, self.platform.arch) {
                summary
                    .warnings
                    .push(format!("could not set up lib/ld.so: {e}"));
            }
        }

        // Phase 1: fetch what the store does not already have.
        let missing: Vec<&Step> = steps
            .iter()
            .copied()
            .filter(|s| !store::is_complete(&self.paths.store(), &s.bottle.sha256))
            .collect();
        summary.store_hits = steps.len() - missing.len();
        let agent = net::agent();
        let tabs: Mutex<Vec<(String, Option<bottle::Tab>)>> = Mutex::new(Vec::new());
        if !missing.is_empty() {
            self.reporter.event(Event::Phase(format!(
                "Downloading {} bottle{}",
                missing.len(),
                if missing.len() == 1 { "" } else { "s" }
            )));
            let jobs: Vec<Job> = missing
                .iter()
                .map(|s| {
                    let url = bottle::blob_url(&s.formula, &s.bottle);
                    Job {
                        label: s.formula.name.clone(),
                        headers: bottle::headers(&url),
                        url,
                        dest: bottle_file(self.paths, s),
                        sha256: Some(s.bottle.sha256.clone()),
                    }
                })
                .collect();
            let threads = if opts.threads == 0 { 16 } else { opts.threads };
            // Manifests (tabs) in parallel with the blobs.
            let tab_handle = {
                let agent = agent.clone();
                let items: Vec<(crate::model::Formula, crate::model::Bottle)> = missing
                    .iter()
                    .map(|s| (s.formula.clone(), s.bottle.clone()))
                    .collect();
                std::thread::spawn(move || -> Vec<(String, Option<bottle::Tab>)> {
                    let pool = rayon::ThreadPoolBuilder::new().num_threads(8).build().ok();
                    let run = || {
                        items
                            .par_iter()
                            .map(|(f, b)| (f.name.clone(), bottle::fetch_tab(&agent, f, b)))
                            .collect::<Vec<_>>()
                    };
                    match pool {
                        Some(p) => p.install(run),
                        None => run(),
                    }
                })
            };
            let progress: &dyn Progress = self.reporter;
            let results = net::download_all(&jobs, threads, progress);
            let mut first_err = None;
            for (job, r) in jobs.iter().zip(results) {
                match r {
                    Ok(_) => {
                        summary.bytes_downloaded +=
                            std::fs::metadata(&job.dest).map(|m| m.len()).unwrap_or(0)
                    }
                    Err(e) => {
                        if first_err.is_none() {
                            first_err = Some(e.to_string());
                        }
                    }
                }
            }
            *tabs.lock().unwrap() = tab_handle.join().unwrap_or_default();
            if let Some(e) = first_err {
                return Err(InstallError::Download(e));
            }

            // Phase 2: unpack + relocate into the store, in parallel.
            self.reporter.event(Event::Phase(format!(
                "Pouring {} bottle{}",
                missing.len(),
                if missing.len() == 1 { "" } else { "s" }
            )));
            let tabs = tabs.lock().unwrap();
            let errors: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());
            let warnings: Mutex<Vec<String>> = Mutex::new(Vec::new());
            missing.par_iter().for_each(|s| {
                let tab = tabs
                    .iter()
                    .find(|(n, _)| n == &s.formula.name)
                    .and_then(|(_, t)| t.clone());
                match self.prepare_store(s, tab.as_ref()) {
                    Ok(w) => warnings.lock().unwrap().extend(w),
                    Err(e) => errors
                        .lock()
                        .unwrap()
                        .push((s.formula.name.clone(), e.to_string())),
                }
            });
            summary.warnings.extend(warnings.into_inner().unwrap());
            if let Some((name, msg)) = errors.into_inner().unwrap().into_iter().next() {
                return Err(InstallError::Step { name, msg });
            }
        }

        // Phase 3: clone into the Cellar, link, receipts; post-install steps
        // run on their own threads, each waiting for the steps of its
        // dependencies, so a slow one (ca-certificates) never holds the rest.
        self.reporter.event(Event::Phase(format!(
            "Installing {} package{}",
            steps.len(),
            if steps.len() == 1 { "" } else { "s" }
        )));
        let gates: std::collections::HashMap<String, std::sync::Arc<Gate>> = steps
            .iter()
            .map(|s| (s.formula.name.clone(), std::sync::Arc::new(Gate::default())))
            .collect();
        let post_reports: Mutex<Vec<(String, postinstall::StepReport)>> = Mutex::new(Vec::new());
        std::thread::scope(|scope| -> Result<()> {
            for s in &steps {
                let t = Instant::now();
                let name = s.formula.name.clone();
                let version = s.formula.pkg_version();
                let trace = std::env::var_os("POUR_TRACE").is_some();
                let mut lap = Instant::now();
                let tick = |what: &str, lap: &mut Instant| {
                    if trace {
                        eprintln!(
                            "    [trace] {name} {what}: {} ms",
                            lap.elapsed().as_millis()
                        );
                        *lap = Instant::now();
                    }
                };
                let old: Vec<receipt::Installed> = receipt::installed_formulae(self.paths)
                    .into_iter()
                    .filter(|i| i.receipt.name == name)
                    .collect();
                tick("scan installed", &mut lap);
                for o in &old {
                    link::unlink_files(self.paths, &name, &o.full().links);
                    if !opts.keep_old_versions || o.receipt.version == version {
                        store::remove_tree(&o.path)?;
                    }
                }
                tick("unlink old", &mut lap);
                let keg = self.paths.keg(&name, &version);
                if keg.exists() || keg.is_symlink() {
                    store::remove_tree(&keg)?;
                }
                let entry = store::entry_dir(&self.paths.store(), &s.bottle.sha256);
                let method = store::materialize(&entry, &keg).map_err(|e| InstallError::Step {
                    name: name.clone(),
                    msg: format!("could not place keg: {e}"),
                })?;
                tick(&format!("materialize ({method:?})"), &mut lap);
                let report = link::link_keg(
                    self.paths,
                    &name,
                    &keg,
                    opts.overwrite_links,
                    !s.formula.keg_only,
                )?;
                tick("link", &mut lap);
                if !report.conflicts.is_empty() {
                    let list: Vec<String> = report
                        .conflicts
                        .iter()
                        .take(5)
                        .map(|(f, owner)| format!("{f} (→ {owner})"))
                        .collect();
                    summary.warnings.push(format!(
                        "{name}: {} file{} not linked because something else owns them: {}{}. Run `pour link --overwrite {name}` to take them over.",
                        report.conflicts.len(),
                        if report.conflicts.len() == 1 { "" } else { "s" },
                        list.join(", "),
                        if report.conflicts.len() > 5 { ", …" } else { "" }
                    ));
                }
                let rcpt = Receipt {
                    kind: Kind::Formula,
                    name: name.clone(),
                    version: version.clone(),
                    tap: s.formula.tap.clone(),
                    desc: s.formula.desc.clone(),
                    homepage: s.formula.homepage.clone(),
                    sha256: Some(s.bottle.sha256.clone()),
                    tag: s.bottle.tag.clone(),
                    cellar: Some(s.bottle.cellar.as_str()),
                    installed_at: receipt::now(),
                    installed_on_request: s.on_request
                        || old.iter().any(|o| o.receipt.installed_on_request),
                    installed_as_dependency: !s.on_request,
                    runtime_deps: s.deps.clone(),
                    keg_only: s.formula.keg_only,
                    links: report.linked.clone(),
                    artifacts: Vec::new(),
                    relocated_files: 0,
                    deep_relocation: matches!(s.bottle.cellar, Cellar::Fixed(_)),
                    pour_version: env!("CARGO_PKG_VERSION").into(),
                    platform: self.platform.native_tag(),
                    pinned: old.iter().any(|o| o.receipt.pinned),
                    native_kind: None,
                    links_len: 0,
                };
                rcpt.write(&keg)?;
                tick("receipt", &mut lap);
                let gate = gates[&name].clone();
                match (&s.formula.post_install, opts.skip_post_install) {
                    (Some(steps_json), false) => {
                        let dep_gates: Vec<std::sync::Arc<Gate>> = s
                            .deps
                            .iter()
                            .filter_map(|d| gates.get(d))
                            .cloned()
                            .collect();
                        let ctx = postinstall::Ctx {
                            paths: self.paths,
                            platform: self.platform,
                            name: name.clone(),
                            version: s.formula.version.clone(),
                            keg: keg.clone(),
                        };
                        let steps_json = steps_json.clone();
                        let reports = &post_reports;
                        let is_linux = self.platform.os == Os::Linux;
                        let paths = self.paths;
                        let arch = self.platform.arch;
                        scope.spawn(move || {
                            for g in &dep_gates {
                                g.wait();
                            }
                            let rep = postinstall::run(&ctx, &steps_json);
                            if is_linux && (ctx.name == "gcc" || ctx.name == "glibc") {
                                let _ = crate::linuxsetup::ensure_ld_so(paths, arch);
                                let _ = crate::linuxsetup::ensure_gcc_runtime(paths);
                            }
                            reports.lock().unwrap().push((ctx.name.clone(), rep));
                            gate.open();
                        });
                    }
                    _ => gate.open(),
                }
                if let Some(c) = &s.formula.caveats {
                    summary.caveats.push((name.clone(), c.clone()));
                }
                if s.formula.keg_only {
                    summary.caveats.push((
                        name.clone(),
                        {
                            let p = self.paths.prefix_str();
                            let why = s
                                .formula
                                .keg_only_reason
                                .clone()
                                .filter(|r| !r.trim().is_empty())
                                .unwrap_or_else(|| {
                                    if name.contains('@') {
                                        "it is an alternative version, so it does not replace the default one".into()
                                    } else {
                                        "something on the system provides the same files".into()
                                    }
                                });
                            format!(
                                "{name} is keg-only: it is not linked into {p}, because {why}.\nTo use it, put it first on your PATH:\n  export PATH=\"{p}/opt/{name}/bin:$PATH\""
                            )
                        },
                    ));
                }
                summary.installed.push((
                    name.clone(),
                    version.clone(),
                    format!("{:?}", s.action).to_lowercase(),
                ));
                self.reporter.event(Event::Done {
                    name,
                    version,
                    action: s.action,
                    ms: t.elapsed().as_millis(),
                });
            }
            Ok(())
        })?;
        let mut reports = post_reports.into_inner().unwrap();
        reports.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, rep) in reports {
            for w in rep.warnings {
                summary.warnings.push(format!("{name}: {w}"));
            }
            for n in rep.notes {
                self.reporter.event(Event::Note(format!("{name}: {n}")));
            }
        }
        if self.platform.os == Os::Linux {
            let _ = crate::linuxsetup::ensure_gcc_runtime(self.paths);
        }
        summary.ms = started.elapsed().as_millis();
        Ok(summary)
    }

    /// Unpack and relocate a bottle into `store/<sha>` (atomic via rename).
    fn prepare_store(&self, s: &Step, tab: Option<&bottle::Tab>) -> Result<Vec<String>> {
        let store = self.paths.store();
        let final_dir = store::entry_dir(&store, &s.bottle.sha256);
        let tmp = store::tmp_dir(&store, &s.bottle.sha256);
        if let Some(shard) = final_dir.parent() {
            std::fs::create_dir_all(shard)?;
        }
        if tmp.exists() {
            store::remove_tree(&tmp)?;
        }
        if final_dir.exists() {
            store::remove_tree(&final_dir)?; // incomplete leftover
        }
        let archive = bottle_file(self.paths, s);
        extract::untar_gz(&archive, &tmp, 2).map_err(|e| InstallError::Step {
            name: s.formula.name.clone(),
            msg: format!("unpack failed: {e}"),
        })?;
        let mut warnings = Vec::new();
        let deep = matches!(s.bottle.cellar, Cellar::Fixed(_));
        let mut relocated = false;
        // macOS bottles marked `:any_skip_relocation` really need nothing.
        // Linux ones built before Homebrew 5.1.15 were labelled wrongly (their
        // interpreter is still a placeholder), so on Linux every bottle is
        // scanned unless the manifest proves it was built later.
        let tab_hb_version = tab
            .and_then(|t| t.homebrew_version.clone())
            .unwrap_or_default();
        let trust_skip = s.bottle.cellar == Cellar::AnySkipRelocation
            && (self.platform.os != Os::Linux || version_at_least(&tab_hb_version, &[5, 1, 15]));
        if !trust_skip
            || tab
                .and_then(|t| t.files())
                .map(|f| !f.is_empty())
                .unwrap_or(false)
        {
            let mut rel = Relocation::for_formula(self.platform, self.paths, &s.deps, deep);
            // The manifest lists the files that carry placeholders. Linux
            // bottles record changed text files but not linkage, so there we
            // scan every file (as Homebrew does).
            if !deep {
                rel.files = match tab {
                    Some(t) if t.linkage_files.is_some() => t.files(),
                    _ => None,
                };
            }
            let report = relocate::relocate_keg(&tmp, &rel);
            relocated = report.changed() > 0;
            for w in report.warnings {
                warnings.push(format!("{}: {w}", s.formula.name));
            }
        }
        let meta = store::StoreMeta {
            name: s.formula.name.clone(),
            version: s.formula.pkg_version(),
            tag: s.bottle.tag.clone(),
            sha256: s.bottle.sha256.clone(),
            created_at: receipt::now(),
            relocated,
            pour_version: env!("CARGO_PKG_VERSION").into(),
        };
        store::write_meta(&tmp, &meta)?;
        std::fs::rename(&tmp, &final_dir)?;
        // The store entry now IS the cache (a warm install clones it), so the
        // compressed bottle only doubles the space. Keep it with POUR_KEEP_DOWNLOADS=1.
        if std::env::var_os("POUR_KEEP_DOWNLOADS").is_none() {
            let _ = std::fs::remove_file(&archive);
        }
        Ok(warnings)
    }

    /// Remove installed formulae (all versions).
    pub fn uninstall(&self, names: &[String], ignore_dependencies: bool) -> Result<Vec<String>> {
        receipt::invalidate(self.paths);
        let _lock = self.lock()?;
        let all = receipt::installed_formulae(self.paths);
        let mut removed = Vec::new();
        for name in names {
            let kegs: Vec<&receipt::Installed> =
                all.iter().filter(|i| &i.receipt.name == name).collect();
            if kegs.is_empty() {
                return Err(InstallError::NotInstalled(name.clone()));
            }
            if !ignore_dependencies {
                let by: Vec<String> = all
                    .iter()
                    .filter(|i| {
                        i.active
                            && !names.contains(&i.receipt.name)
                            && i.receipt.runtime_deps.contains(name)
                    })
                    .map(|i| i.receipt.name.clone())
                    .collect();
                if !by.is_empty() {
                    return Err(InstallError::Required {
                        name: name.clone(),
                        by: by.join(", "),
                    });
                }
            }
            for k in kegs {
                link::unlink_files(self.paths, name, &k.full().links);
                store::remove_tree(&k.path)?;
            }
            let opt = self.paths.opt_link(name);
            if opt.is_symlink() {
                let _ = std::fs::remove_file(&opt);
            }
            let _ = std::fs::remove_dir(self.paths.cellar().join(name));
            removed.push(name.clone());
        }
        Ok(removed)
    }

    /// Remove store entries no installed keg was poured from, and stale
    /// downloads. Returns bytes freed.
    pub fn cleanup(&self, keep_downloads: bool) -> Result<u64> {
        let _lock = self.lock()?;
        let live: std::collections::HashSet<String> = receipt::installed_formulae(self.paths)
            .into_iter()
            .filter_map(|i| i.receipt.sha256)
            .collect();
        let mut freed = 0u64;
        for (sha, _, dir) in store::entries(&self.paths.store()) {
            if !live.contains(&sha) || sha.contains(".tmp-") {
                freed += store::tree_size(&dir);
                store::remove_tree(&dir)?;
            }
        }
        if !keep_downloads {
            if let Ok(rd) = std::fs::read_dir(self.paths.downloads()) {
                for e in rd.flatten() {
                    let p = e.path();
                    let keep = p
                        .file_name()
                        .map(|n| n.to_string_lossy().ends_with(".bottle.tar.gz"))
                        .unwrap_or(false)
                        && live.iter().any(|_| false);
                    if !keep {
                        freed += std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
                        let _ = store::remove_tree(&p);
                    }
                }
            }
        }
        Ok(freed)
    }
}

/// `"5.1.15"` >= `[5, 1, 15]`? Non-numeric or empty strings are "older".
fn version_at_least(v: &str, min: &[u32]) -> bool {
    let parts: Vec<u32> = v
        .split(['.', '-'])
        .map(|x| {
            x.chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse()
                .unwrap_or(0)
        })
        .collect();
    if parts.is_empty() || v.is_empty() {
        return false;
    }
    for (i, &m) in min.iter().enumerate() {
        let a = parts.get(i).copied().unwrap_or(0);
        if a != m {
            return a > m;
        }
    }
    true
}

/// A one-shot latch: post-install threads wait on their dependencies' gates.
#[derive(Default)]
pub struct Gate {
    done: Mutex<bool>,
    cv: std::sync::Condvar,
}

impl Gate {
    pub fn open(&self) {
        *self.done.lock().unwrap() = true;
        self.cv.notify_all();
    }
    pub fn wait(&self) {
        let mut d = self.done.lock().unwrap();
        while !*d {
            d = self.cv.wait(d).unwrap();
        }
    }
}

/// A reporter that prints nothing (tests, `--quiet`).
pub struct Silent;
impl Progress for Silent {}
impl Reporter for Silent {
    fn event(&self, _e: Event) {}
}
