//! Command implementations.

use crate::ui::{arrow, human_bytes, human_ms, Ui};
use crate::{BundleAction, Cli, Command, KindFlags, RegistryAction, TapAction};
use console::style;
use pour_core::cask::{CaskEngine, CaskOptions};
use pour_core::install::{Engine, Options};
use pour_core::model::{Kind, Pkg};
use pour_core::paths::Paths;
use pour_core::platform::{Os, Platform};
use pour_core::receipt;
use pour_core::registry::{self, Registry};
use pour_core::resolve::{self, PlanOptions};
use std::collections::BTreeMap;
use std::fmt::Write as _;

/// Names split into formulae and (source, cask) pairs.
type Split = (Vec<String>, Vec<(String, pour_core::model::Cask)>);
/// Brewfile entries: formulae, casks, taps (name, url).
pub type Brewfile = (Vec<String>, Vec<String>, Vec<(String, Option<String>)>);

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

struct Ctx<'a> {
    ui: &'a Ui,
    paths: Paths,
    platform: Platform,
}

fn err(msg: impl Into<String>) -> Box<dyn std::error::Error> {
    Box::<dyn std::error::Error>::from(msg.into())
}

impl Ctx<'_> {
    fn registry(&self) -> Result<Registry> {
        match Registry::load(&self.paths) {
            Ok(r) => Ok(r),
            Err(registry::RegistryError::Missing) => {
                self.ui.head("No index yet — fetching it");
                self.update(false)?;
                Ok(Registry::load(&self.paths)?)
            }
            Err(e) => Err(e.into()),
        }
    }

    fn engine(&self) -> Engine<'_> {
        Engine {
            paths: &self.paths,
            platform: &self.platform,
            reporter: self.ui,
        }
    }

    fn casks(&self) -> CaskEngine<'_> {
        CaskEngine {
            paths: &self.paths,
            platform: &self.platform,
            reporter: self.ui,
        }
    }

    fn kind(&self, flags: &KindFlags) -> Option<Kind> {
        if flags.cask {
            Some(Kind::Cask)
        } else if flags.formula {
            Some(Kind::Formula)
        } else {
            None
        }
    }

    fn update(&self, force: bool) -> Result<()> {
        let t = std::time::Instant::now();
        let rep = registry::update(&self.paths, force);
        let show = |name: &str, st: &registry::Status| match st {
            registry::Status::Updated { formulae, casks } => self
                .ui
                .ok(format!("{name}: {formulae} formulae, {casks} casks")),
            registry::Status::Unchanged => self.ui.note(format!("{name}: already up to date")),
            registry::Status::Failed(m) => self.ui.warn(format!("{name}: {m}")),
        };
        if let Some(c) = &rep.core {
            show("homebrew (core + cask)", c);
        }
        for (n, st) in &rep.taps {
            show(n, st);
        }
        if let Some(registry::Status::Failed(m)) = &rep.core {
            if !self.paths.index_file().exists() {
                return Err(err(format!("could not fetch the index: {m}")));
            }
        }
        self.ui
            .note(format!("done in {}", human_ms(t.elapsed().as_millis())));
        Ok(())
    }

    /// Split names into formulae and casks according to the index.
    fn classify(&self, reg: &Registry, names: &[String], kind: Option<Kind>) -> Result<Split> {
        let mut formulae = Vec::new();
        let mut casks = Vec::new();
        for n in names {
            match reg.find(n, kind) {
                Some((src, Pkg::Cask(c))) => casks.push((src, c)),
                Some((_, Pkg::Formula(f))) => formulae.push(f.name.clone()),
                None => {
                    let s = reg.suggest(n, 3);
                    let hint = if s.is_empty() {
                        String::new()
                    } else {
                        format!(
                            ". Did you mean: {}?",
                            s.iter()
                                .map(|(k, x)| if *k == Kind::Cask {
                                    format!("--cask {x}")
                                } else {
                                    x.clone()
                                })
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    };
                    return Err(err(format!("No formula or cask named \"{n}\"{hint}")));
                }
            }
        }
        Ok((formulae, casks))
    }

    fn install_casks(
        &self,
        reg: &Registry,
        casks: &[(String, pour_core::model::Cask)],
        force: bool,
        dry_run: bool,
    ) -> Result<()> {
        if casks.is_empty() {
            return Ok(());
        }
        for (_, c) in casks {
            if c.url.is_empty() && c.native.is_empty() {
                return Err(err(c
                    .caveats
                    .clone()
                    .unwrap_or_else(|| format!("{} has no download yet", c.token))));
            }
        }
        if !self.platform.installs_casks()
            && !casks
                .iter()
                .all(|(_, c)| !c.native.is_empty() || c.livecheck_url.is_some())
        {
            return Err(err(format!("casks are macOS apps; on {} Pour can install only packages that ship their own installer for this system", self.platform.describe())));
        }
        let eng = self.casks();
        for (src, c) in casks {
            if c.disabled {
                return Err(err(format!(
                    "{} has been disabled{}",
                    c.token,
                    c.disable_reason
                        .clone()
                        .map(|r| format!(": {r}"))
                        .unwrap_or_default()
                )));
            }
            if c.deprecated {
                self.ui.warn(format!(
                    "{} is deprecated{}",
                    c.token,
                    c.deprecation_reason
                        .clone()
                        .map(|r| format!(": {r}"))
                        .unwrap_or_default()
                ));
            }
            let opts = CaskOptions {
                force,
                dry_run: true,
                ..Default::default()
            };
            let probe = eng.install(c, src, &opts)?;
            if !probe.needs_formulae.is_empty() {
                self.ui.head(format!(
                    "{} needs formulae: {}",
                    c.token,
                    probe.needs_formulae.join(", ")
                ));
                let plan = resolve::plan(
                    reg,
                    &self.paths,
                    &self.platform,
                    &probe.needs_formulae,
                    &PlanOptions::default(),
                )?;
                if !dry_run {
                    self.engine().install(
                        &plan,
                        &Options {
                            threads: 16,
                            ..Default::default()
                        },
                    )?;
                }
            }
            if !probe.needs_casks.is_empty() {
                let deps: Vec<(String, pour_core::model::Cask)> = probe
                    .needs_casks
                    .iter()
                    .filter_map(|t| match reg.find(t, Some(Kind::Cask)) {
                        Some((s, Pkg::Cask(cc))) => Some((s, cc)),
                        _ => None,
                    })
                    .filter(|(_, cc)| receipt::find_cask(&self.paths, &cc.token).is_none())
                    .collect();
                self.install_casks(reg, &deps, force, dry_run)?;
            }
            if dry_run {
                self.ui.println(format!(
                    "  would install cask {} {}",
                    style(&c.token).bold(),
                    probe.version
                ));
                continue;
            }
            let opts = CaskOptions {
                force,
                dry_run: false,
                ..Default::default()
            };
            let s = eng.install(c, src, &opts)?;
            for w in &s.warnings {
                self.ui.warn(w);
            }
            let what = s
                .artifacts
                .iter()
                .filter(|a| !a.contains(':'))
                .map(|a| a.rsplit('/').next().unwrap_or(a).to_string())
                .collect::<Vec<_>>()
                .join(", ");
            self.ui.println(format!(
                "  {} {} {} {} {}",
                style("🍶").dim(),
                style(&c.token).bold(),
                style(&s.version).dim(),
                style("installed").green(),
                style(format!("{} · {}", human_bytes(s.bytes), human_ms(s.ms))).dim()
            ));
            if !what.is_empty() {
                self.ui.note(what);
            }
            if let Some(cv) = &s.caveats {
                self.ui.println(format!(
                    "\n{} Caveats for {}\n{}",
                    arrow(),
                    c.token,
                    cv.trim()
                ));
            }
        }
        Ok(())
    }
}

pub fn run(cli: &Cli, ui: &Ui) -> Result<()> {
    let platform = Platform::detect();
    let paths = Paths::detect(&platform);
    let ctx = Ctx {
        ui,
        paths,
        platform,
    };

    if let Some(p) = &cli.prefix {
        if p.is_empty() {
            println!("{}", ctx.paths.prefix_str());
        } else {
            let i = receipt::find_formula(&ctx.paths, p)
                .ok_or_else(|| err(format!("{p} is not installed")))?;
            println!("{}", ctx.paths.opt_link(&i.receipt.name).display());
        }
        return Ok(());
    }
    if cli.cellar {
        println!("{}", ctx.paths.cellar().display());
        return Ok(());
    }
    if cli.cache {
        println!("{}", ctx.paths.cache().display());
        return Ok(());
    }
    let Some(command) = &cli.command else {
        return Ok(());
    };

    match command {
        Command::Install {
            kind,
            reinstall,
            only_dependencies,
            skip_post_install,
            overwrite,
            force,
            jobs,
            dry_run,
            names,
        } => {
            let reg = ctx.registry()?;
            let (formulae, casks) = ctx.classify(&reg, names, ctx.kind(kind))?;
            if !formulae.is_empty() {
                let popts = PlanOptions {
                    reinstall: *reinstall,
                    only_dependencies: *only_dependencies,
                    ..Default::default()
                };
                let plan = resolve::plan(&reg, &ctx.paths, &ctx.platform, &formulae, &popts)?;
                print_plan(&ctx, &plan);
                if plan.pending_count() == 0 {
                    for f in &formulae {
                        if let Some(i) = receipt::find_formula(&ctx.paths, f) {
                            ui.ok(format!(
                                "{} {} is already installed and up to date",
                                i.receipt.name, i.receipt.version
                            ));
                        }
                    }
                } else if !*dry_run {
                    let summary = ctx.engine().install(
                        &plan,
                        &Options {
                            threads: *jobs,
                            skip_post_install: *skip_post_install,
                            overwrite_links: *overwrite,
                            keep_old_versions: false,
                        },
                    )?;
                    print_summary(&ctx, &summary);
                }
            }
            ctx.install_casks(&reg, &casks, *force, *dry_run)?;
        }
        Command::Uninstall {
            kind,
            ignore_dependencies,
            zap,
            names,
        } => {
            let mut formulae = Vec::new();
            let mut casks = Vec::new();
            for n in names {
                let k = ctx.kind(kind);
                let is_cask = k == Some(Kind::Cask)
                    || (k.is_none()
                        && receipt::find_formula(&ctx.paths, n).is_none()
                        && receipt::find_cask(&ctx.paths, n).is_some());
                if is_cask {
                    casks.push(n.clone());
                } else {
                    formulae.push(n.clone());
                }
            }
            if !formulae.is_empty() {
                let removed = ctx.engine().uninstall(&formulae, *ignore_dependencies)?;
                for r in removed {
                    ui.ok(format!("uninstalled {r}"));
                }
            }
            for c in casks {
                let warnings = ctx.casks().uninstall(&c, *zap)?;
                for w in warnings {
                    ui.warn(w);
                }
                ui.ok(format!(
                    "uninstalled {c}{}",
                    if *zap { " (zapped)" } else { "" }
                ));
            }
        }
        Command::Reinstall { kind, names } => {
            let reg = ctx.registry()?;
            let (formulae, casks) = ctx.classify(&reg, names, ctx.kind(kind))?;
            if !formulae.is_empty() {
                let plan = resolve::plan(
                    &reg,
                    &ctx.paths,
                    &ctx.platform,
                    &formulae,
                    &PlanOptions {
                        reinstall: true,
                        ..Default::default()
                    },
                )?;
                print_plan(&ctx, &plan);
                let summary = ctx.engine().install(
                    &plan,
                    &Options {
                        threads: 16,
                        ..Default::default()
                    },
                )?;
                print_summary(&ctx, &summary);
            }
            ctx.install_casks(&reg, &casks, true, false)?;
        }
        Command::Upgrade {
            kind,
            greedy,
            dry_run,
            names,
        } => {
            let reg = ctx.registry()?;
            let k = ctx.kind(kind);
            if k != Some(Kind::Cask) {
                let outdated = resolve::outdated(&reg, &ctx.paths);
                let targets: Vec<String> = if names.is_empty() {
                    outdated
                        .iter()
                        .filter(|(i, _)| i.receipt.installed_on_request || true)
                        .map(|(i, _)| i.receipt.name.clone())
                        .collect()
                } else {
                    names
                        .iter()
                        .filter(|n| {
                            outdated.iter().any(|(i, _)| &i.receipt.name == *n)
                                || receipt::find_formula(&ctx.paths, n).is_none()
                        })
                        .cloned()
                        .collect()
                };
                if targets.is_empty() {
                    if names.is_empty() || k == Some(Kind::Formula) {
                        ui.ok("all formulae are up to date");
                    }
                } else {
                    for (i, f) in &outdated {
                        if targets.contains(&i.receipt.name) {
                            ui.println(format!(
                                "  {} {} → {}",
                                style(&i.receipt.name).bold(),
                                style(&i.receipt.version).dim(),
                                style(f.pkg_version()).green()
                            ));
                        }
                    }
                    let plan = resolve::plan(
                        &reg,
                        &ctx.paths,
                        &ctx.platform,
                        &targets,
                        &PlanOptions {
                            upgrade: true,
                            ..Default::default()
                        },
                    )?;
                    if !*dry_run {
                        let summary = ctx.engine().install(
                            &plan,
                            &Options {
                                threads: 16,
                                ..Default::default()
                            },
                        )?;
                        print_summary(&ctx, &summary);
                    }
                }
            }
            if k != Some(Kind::Formula) {
                let mut casks = Vec::new();
                for i in receipt::installed_casks(&ctx.paths) {
                    if !names.is_empty() && !names.contains(&i.receipt.name) {
                        continue;
                    }
                    if let Some((src, c, latest)) = cask_latest(&ctx, &reg, &i, true) {
                        // apps that update themselves are left alone unless --greedy;
                        // Pour-native apps have a live feed, so their version is exact
                        if latest != i.receipt.version
                            && (*greedy || !c.auto_updates || c.livecheck_url.is_some())
                        {
                            casks.push((src, c, i.receipt.version.clone(), latest));
                        }
                    }
                }
                if casks.is_empty() {
                    if names.is_empty() || k == Some(Kind::Cask) {
                        ui.ok("all casks are up to date");
                    }
                } else {
                    for (_, c, from, to) in &casks {
                        ui.println(format!(
                            "  {} {} → {}",
                            style(&c.token).bold(),
                            style(from).dim(),
                            style(to).green()
                        ));
                    }
                    if !*dry_run {
                        let targets: Vec<(String, pour_core::model::Cask)> =
                            casks.into_iter().map(|(s, c, _, _)| (s, c)).collect();
                        ctx.install_casks(&reg, &targets, true, false)?;
                    }
                }
            }
        }
        Command::Update { force } => ctx.update(*force)?,
        Command::Outdated { kind } => {
            let reg = ctx.registry()?;
            let k = ctx.kind(kind);
            let mut any = false;
            if k != Some(Kind::Cask) {
                for (i, f) in resolve::outdated(&reg, &ctx.paths) {
                    any = true;
                    println!(
                        "{} ({}) < {}",
                        i.receipt.name,
                        i.receipt.version,
                        f.pkg_version()
                    );
                }
            }
            if k != Some(Kind::Formula) {
                for i in receipt::installed_casks(&ctx.paths) {
                    if let Some((_, _, latest)) = cask_latest(&ctx, &reg, &i, false) {
                        if latest != i.receipt.version {
                            any = true;
                            println!("{} ({}) != {}", i.receipt.name, i.receipt.version, latest);
                        }
                    }
                }
            }
            if !any && !cli.quiet {
                ui.ok("everything is up to date");
            }
        }
        Command::Search { kind, limit, query } => {
            let reg = ctx.registry()?;
            let t = std::time::Instant::now();
            let q = query.join(" ");
            let hits = reg.search(&q, ctx.kind(kind), *limit);
            let took = t.elapsed();
            if cli.json {
                let v: Vec<serde_json::Value> = hits.iter().map(|(src, h)| serde_json::json!({"name": h.name, "kind": h.kind.to_string(), "desc": h.desc, "source": src})).collect();
                println!("{}", serde_json::to_string_pretty(&v)?);
                return Ok(());
            }
            if hits.is_empty() {
                let s = reg.suggest(&q, 5);
                if s.is_empty() {
                    ui.println(format!("No formulae or casks found for \"{q}\"."));
                } else {
                    ui.println(format!(
                        "No exact match for \"{q}\". Did you mean: {}?",
                        s.iter()
                            .map(|(_, n)| n.clone())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                return Ok(());
            }
            let installed_f: std::collections::HashSet<String> =
                receipt::installed_formulae(&ctx.paths)
                    .into_iter()
                    .map(|i| i.receipt.name)
                    .collect();
            let installed_c: std::collections::HashSet<String> =
                receipt::installed_casks(&ctx.paths)
                    .into_iter()
                    .map(|i| i.receipt.name)
                    .collect();
            // formulae first, then casks — each group keeps its ranking
            let mut ordered: Vec<&(String, pour_core::index::Hit)> = Vec::with_capacity(hits.len());
            ordered.extend(hits.iter().filter(|(_, h)| h.kind == Kind::Formula));
            ordered.extend(hits.iter().filter(|(_, h)| h.kind == Kind::Cask));
            let mut last_kind = None;
            for (src, h) in ordered {
                if last_kind != Some(h.kind) {
                    last_kind = Some(h.kind);
                    ui.println(format!(
                        "{} {}",
                        arrow(),
                        style(if h.kind == Kind::Formula {
                            "Formulae"
                        } else {
                            "Casks"
                        })
                        .bold()
                    ));
                }
                let mark = if (h.kind == Kind::Formula && installed_f.contains(&h.name))
                    || (h.kind == Kind::Cask && installed_c.contains(&h.name))
                {
                    style(" ✔").green().to_string()
                } else {
                    String::new()
                };
                let tap = if src == "homebrew" {
                    String::new()
                } else {
                    style(format!(" [{src}]")).dim().to_string()
                };
                let desc = if h.desc.is_empty() {
                    String::new()
                } else {
                    format!(": {}", h.desc)
                };
                ui.println(format!(
                    "{}{mark}{tap}{}",
                    style(&h.name).bold(),
                    style(desc).dim()
                ));
            }
            ui.note(format!(
                "{} result{} in {}",
                hits.len(),
                if hits.len() == 1 { "" } else { "s" },
                human_ms(took.as_millis())
                    .replace("0 ms", &format!("{:.2} ms", took.as_secs_f64() * 1000.0))
            ));
        }
        Command::Info { kind, names } => {
            let reg = ctx.registry()?;
            let mut out = Vec::new();
            for n in names {
                let (src, pkg) = reg
                    .find(n, ctx.kind(kind))
                    .ok_or_else(|| err(format!("No formula or cask named \"{n}\"")))?;
                if cli.json {
                    out.push(pkg_json(&pkg, &src));
                } else {
                    print_info(&ctx, &pkg, &src);
                }
            }
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&out)?);
            }
        }
        Command::List { kind, versions } => {
            let k = ctx.kind(kind);
            let formulae = receipt::installed_formulae(&ctx.paths);
            let casks = receipt::installed_casks(&ctx.paths);
            if cli.json {
                let v = serde_json::json!({
                    "formulae": formulae.iter().map(|i| serde_json::json!({"name": i.receipt.name, "version": i.receipt.version, "active": i.active, "on_request": i.receipt.installed_on_request, "deps": i.receipt.runtime_deps})).collect::<Vec<_>>(),
                    "casks": casks.iter().map(|i| serde_json::json!({"token": i.receipt.name, "version": i.receipt.version, "artifacts": i.receipt.artifacts})).collect::<Vec<_>>(),
                });
                println!("{}", serde_json::to_string_pretty(&v)?);
                return Ok(());
            }
            if k != Some(Kind::Cask) {
                if k.is_none() && !formulae.is_empty() {
                    ui.println(format!("{} Formulae", arrow()));
                }
                print_columns(
                    &formulae
                        .iter()
                        .filter(|i| i.active || *versions)
                        .map(|i| {
                            if *versions {
                                format!("{} {}", i.receipt.name, i.receipt.version)
                            } else {
                                i.receipt.name.clone()
                            }
                        })
                        .collect::<Vec<_>>(),
                );
            }
            if k != Some(Kind::Formula) && !casks.is_empty() {
                if k.is_none() {
                    ui.println(format!("{} Casks", arrow()));
                }
                print_columns(
                    &casks
                        .iter()
                        .map(|i| {
                            if *versions {
                                format!("{} {}", i.receipt.name, i.receipt.version)
                            } else {
                                i.receipt.name.clone()
                            }
                        })
                        .collect::<Vec<_>>(),
                );
            }
        }
        Command::Link { overwrite, names } => {
            for n in names {
                let i = receipt::find_formula(&ctx.paths, n)
                    .ok_or_else(|| err(format!("{n} is not installed")))?;
                let rep = pour_core::link::link_keg(
                    &ctx.paths,
                    &i.receipt.name,
                    &i.path,
                    *overwrite,
                    true,
                )?;
                let mut r = i.full();
                r.links = rep.linked.clone();
                r.write(&i.path)?;
                for (f, owner) in &rep.conflicts {
                    ui.warn(format!("{f} is owned by {owner} (use --overwrite)"));
                }
                ui.ok(format!(
                    "linked {} ({} symlinks)",
                    i.receipt.name,
                    rep.linked.len()
                ));
            }
        }
        Command::Unlink { names } => {
            for n in names {
                let i = receipt::find_formula(&ctx.paths, n)
                    .ok_or_else(|| err(format!("{n} is not installed")))?;
                let mut r = i.full();
                let removed = pour_core::link::unlink_files(&ctx.paths, &i.receipt.name, &r.links);
                r.links.clear();
                r.write(&i.path)?;
                ui.ok(format!(
                    "unlinked {} ({removed} symlinks removed)",
                    i.receipt.name
                ));
            }
        }
        Command::Pin { names } | Command::Unpin { names } => {
            let pin = matches!(command, Command::Pin { .. });
            for n in names {
                let i = receipt::find_formula(&ctx.paths, n)
                    .ok_or_else(|| err(format!("{n} is not installed")))?;
                let mut r = i.full();
                r.pinned = pin;
                r.write(&i.path)?;
                ui.ok(format!(
                    "{} {}",
                    if pin { "pinned" } else { "unpinned" },
                    i.receipt.name
                ));
            }
        }
        Command::Deps {
            tree,
            installed,
            name,
        } => {
            let reg = ctx.registry()?;
            let (_, f) = resolve::lookup_formula(&reg, name)?;
            let linux = ctx.platform.os == Os::Linux;
            if *tree {
                fn walk(
                    reg: &Registry,
                    name: &str,
                    linux: bool,
                    depth: usize,
                    seen: &mut std::collections::HashSet<String>,
                    out: &mut String,
                ) {
                    let Ok((_, f)) = resolve::lookup_formula(reg, name) else {
                        return;
                    };
                    for d in f.runtime_deps(linux) {
                        let _ = writeln!(out, "{}{}", "  ".repeat(depth), d);
                        if seen.insert(d.clone()) {
                            walk(reg, &d, linux, depth + 1, seen, out);
                        }
                    }
                }
                let mut out = String::new();
                println!("{}", f.name);
                walk(&reg, &f.name, linux, 1, &mut Default::default(), &mut out);
                print!("{out}");
            } else {
                let mut all = std::collections::BTreeSet::new();
                let mut stack = f.runtime_deps(linux);
                while let Some(d) = stack.pop() {
                    if all.insert(d.clone()) {
                        if let Ok((_, df)) = resolve::lookup_formula(&reg, &d) {
                            stack.extend(df.runtime_deps(linux));
                        }
                    }
                }
                for d in all {
                    if !*installed || receipt::find_formula(&ctx.paths, &d).is_some() {
                        println!("{d}");
                    }
                }
            }
        }
        Command::Uses { name } => {
            for d in receipt::dependents(&ctx.paths, name) {
                println!("{d}");
            }
        }
        Command::Leaves => {
            let all = receipt::installed_formulae(&ctx.paths);
            for i in all.iter().filter(|i| i.active) {
                if !all
                    .iter()
                    .any(|o| o.active && o.receipt.runtime_deps.contains(&i.receipt.name))
                {
                    println!("{}", i.receipt.name);
                }
            }
        }
        Command::Apps => {
            // Live: pour-apps plus whatever the dashboard switched on since the last `pour update`.
            let url = std::env::var("POUR_APPS_URL")
                .unwrap_or_else(|_| "https://ipconfig.co.network/pour/apps/index.json".into());
            let v = pour_core::net::get_json(&pour_core::net::agent(), &url, &[])?;
            let casks = v
                .get("casks")
                .and_then(|c| c.as_array())
                .cloned()
                .unwrap_or_default();
            if cli.json {
                let out: Vec<serde_json::Value> = casks.iter().map(|c| serde_json::json!({"token": c["token"], "name": c["name"].get(0), "version": c["version"], "desc": c["desc"], "source": c.pointer("/pour/source").cloned().unwrap_or_else(|| "pour-apps".into())})).collect();
                println!("{}", serde_json::to_string_pretty(&out)?);
                return Ok(());
            }
            let installed: std::collections::HashSet<String> = receipt::installed_casks(&ctx.paths)
                .into_iter()
                .map(|i| i.receipt.name)
                .collect();
            ui.println(format!(
                "{} {} {}",
                arrow(),
                style("Apps by Jagadeesh Kumar S").bold(),
                style(format!("({})", casks.len())).dim()
            ));
            for c in &casks {
                let token = c["token"].as_str().unwrap_or("");
                let name = c["name"].get(0).and_then(|x| x.as_str()).unwrap_or(token);
                let ver = c["version"].as_str().unwrap_or("");
                let new = c.pointer("/pour/source").and_then(|x| x.as_str()) == Some("dashboard");
                let mark = if installed.contains(token) {
                    style(" ✔").green().to_string()
                } else {
                    String::new()
                };
                ui.println(format!(
                    "  {}{} {}{}  {}",
                    style(format!("{token:<10}")).cyan().bold(),
                    mark,
                    style(ver).blue(),
                    if new {
                        style(" new").yellow().to_string()
                    } else {
                        String::new()
                    },
                    style(c["desc"].as_str().unwrap_or(name)).dim()
                ));
            }
            ui.println(format!(
                "{} pour install --cask <app>",
                style("Install:").bold()
            ));
        }
        Command::Register {
            app,
            name,
            line,
            version,
            feed,
        } => {
            let ok = app.len() >= 2
                && app.len() <= 24
                && app.chars().next().is_some_and(|c| c.is_ascii_lowercase())
                && app
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
            if !ok {
                return Err(err("the app id must be 2–24 lowercase letters, digits or dashes, starting with a letter"));
            }
            let url = std::env::var("POUR_REGISTER_URL")
                .unwrap_or_else(|_| "https://ipconfig.co.network/pour/api/apps/register".into());
            let body = serde_json::json!({"app": app, "name": name, "line": line, "version": version, "download_url": feed, "platform": ctx.platform.describe()});
            let (code, text) = pour_core::net::post_json(&pour_core::net::agent(), &url, &body)
                .map_err(|e| err(format!("could not reach the dashboard: {e}")))?;
            let j: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
            if code >= 400 {
                return Err(err(j["message"]
                    .as_str()
                    .or(j["error"].as_str())
                    .unwrap_or("the dashboard refused it")
                    .to_string()));
            }
            let status = j["status"].as_str().unwrap_or("pending");
            ui.println(format!(
                "{} {} {} on the dashboard ({status})",
                style("✓").green(),
                style(app).cyan().bold(),
                if j["added"].as_bool().unwrap_or(false) {
                    "registered"
                } else {
                    "is already"
                }
            ));
            if status != "on" {
                ui.println("  The owner switches it on in the Apps tab; then `pour update && pour install --cask ".to_string() + app + "` works everywhere.");
            }
        }
        Command::Doctor => doctor(&ctx)?,
        Command::Cleanup { dry_run } => {
            if *dry_run {
                let live: std::collections::HashSet<String> =
                    receipt::installed_formulae(&ctx.paths)
                        .into_iter()
                        .filter_map(|i| i.receipt.sha256)
                        .collect();
                let mut total = 0u64;
                for (sha, meta, dir) in pour_core::store::entries(&ctx.paths.store()) {
                    if !live.contains(&sha) {
                        let size = pour_core::store::tree_size(&dir);
                        total += size;
                        println!(
                            "would remove store/{} ({}, {})",
                            &sha[..12],
                            meta.map(|m| format!("{} {}", m.name, m.version))
                                .unwrap_or_else(|| "incomplete".into()),
                            human_bytes(size)
                        );
                    }
                }
                ui.note(format!("{} reclaimable", human_bytes(total)));
            } else {
                let freed = ctx.engine().cleanup(false)?;
                ui.ok(format!("freed {}", human_bytes(freed)));
            }
        }
        Command::Tap { action } => match action {
            None | Some(TapAction::List) => {
                for t in registry::taps(&ctx.paths) {
                    println!("{}\t{}", t.name, t.url);
                }
            }
            Some(TapAction::Add { name, url }) => {
                let mut list = registry::taps(&ctx.paths);
                list.retain(|t| &t.name != name);
                list.push(registry::Tap {
                    name: name.clone(),
                    url: url.clone(),
                });
                registry::save_taps(&ctx.paths, &list)?;
                ui.ok(format!("added {name}"));
                ctx.update(false)?;
            }
            Some(TapAction::Remove { name }) => {
                let mut list = registry::taps(&ctx.paths);
                let before = list.len();
                list.retain(|t| &t.name != name);
                if list.len() == before {
                    return Err(err(format!("no tap named {name}")));
                }
                registry::save_taps(&ctx.paths, &list)?;
                registry::remove_tap_file(&ctx.paths, name);
                ui.ok(format!("removed {name}"));
            }
        },
        Command::Bundle { action } => match action {
            None | Some(BundleAction::Install { .. }) => {
                let (file, dry_run) = match action {
                    Some(BundleAction::Install { file, dry_run }) => (file.clone(), *dry_run),
                    _ => ("Brewfile".to_string(), false),
                };
                let text =
                    std::fs::read_to_string(&file).map_err(|e| err(format!("{file}: {e}")))?;
                let (brews, casks, taps) = parse_brewfile(&text);
                for (name, url) in taps {
                    if let Some(u) = url {
                        let mut list = registry::taps(&ctx.paths);
                        if !list.iter().any(|t| t.name == name) {
                            list.push(registry::Tap { name, url: u });
                            registry::save_taps(&ctx.paths, &list)?;
                        }
                    }
                }
                let reg = ctx.registry()?;
                ui.head(format!(
                    "Brewfile: {} formulae, {} casks",
                    brews.len(),
                    casks.len()
                ));
                if !brews.is_empty() {
                    let plan = resolve::plan(
                        &reg,
                        &ctx.paths,
                        &ctx.platform,
                        &brews,
                        &PlanOptions::default(),
                    )?;
                    print_plan(&ctx, &plan);
                    if !dry_run && plan.pending_count() > 0 {
                        let s = ctx.engine().install(
                            &plan,
                            &Options {
                                threads: 16,
                                ..Default::default()
                            },
                        )?;
                        print_summary(&ctx, &s);
                    }
                }
                let casks: Vec<(String, pour_core::model::Cask)> = casks
                    .iter()
                    .filter_map(|t| match reg.find(t, Some(Kind::Cask)) {
                        Some((s, Pkg::Cask(c))) => Some((s, c)),
                        _ => {
                            ui.warn(format!("cask {t} not found"));
                            None
                        }
                    })
                    .filter(|(_, c)| receipt::find_cask(&ctx.paths, &c.token).is_none())
                    .collect();
                ctx.install_casks(&reg, &casks, false, dry_run)?;
            }
            Some(BundleAction::Dump { file, force }) => {
                if std::path::Path::new(file).exists() && !*force {
                    return Err(err(format!("{file} exists; pass --force to overwrite")));
                }
                let mut out = String::from("# Brewfile written by pour bundle dump\n");
                for t in registry::taps(&ctx.paths) {
                    let _ = writeln!(out, "tap \"{}\", \"{}\"", t.name, t.url);
                }
                for i in receipt::installed_formulae(&ctx.paths)
                    .iter()
                    .filter(|i| i.active && i.receipt.installed_on_request)
                {
                    let _ = writeln!(out, "brew \"{}\"", i.receipt.name);
                }
                for i in receipt::installed_casks(&ctx.paths) {
                    let _ = writeln!(out, "cask \"{}\"", i.receipt.name);
                }
                std::fs::write(file, out)?;
                ui.ok(format!("wrote {file}"));
            }
        },
        Command::Migrate { dry_run } => {
            let brew = std::process::Command::new("brew")
                .args(["list", "--formula", "-1", "--installed-on-request"])
                .output()
                .or_else(|_| std::process::Command::new("brew").args(["leaves"]).output())
                .map_err(|_| err("brew is not on PATH"))?;
            let names: Vec<String> = String::from_utf8_lossy(&brew.stdout)
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect();
            let casks = std::process::Command::new("brew")
                .args(["list", "--cask", "-1"])
                .output()
                .map(|o| {
                    String::from_utf8_lossy(&o.stdout)
                        .lines()
                        .map(|l| l.trim().to_string())
                        .filter(|l| !l.is_empty())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            ui.head(format!(
                "Homebrew has {} formulae and {} casks installed on request",
                names.len(),
                casks.len()
            ));
            let reg = ctx.registry()?;
            if !names.is_empty() {
                let plan = resolve::plan(
                    &reg,
                    &ctx.paths,
                    &ctx.platform,
                    &names,
                    &PlanOptions::default(),
                )?;
                print_plan(&ctx, &plan);
                if !*dry_run && plan.pending_count() > 0 {
                    let s = ctx.engine().install(
                        &plan,
                        &Options {
                            threads: 16,
                            ..Default::default()
                        },
                    )?;
                    print_summary(&ctx, &s);
                }
            }
            let casks: Vec<(String, pour_core::model::Cask)> = casks
                .iter()
                .filter_map(|t| match reg.find(t, Some(Kind::Cask)) {
                    Some((s, Pkg::Cask(c))) => Some((s, c)),
                    _ => None,
                })
                .collect();
            ctx.install_casks(&reg, &casks, true, *dry_run)?;
            ui.note("Homebrew itself was left untouched; remove it whenever you are ready.");
        }
        Command::Init { dry_run } => init(&ctx, *dry_run)?,
        Command::Shellenv { shell } => {
            let sh = shell.clone().unwrap_or_else(|| {
                std::env::var("SHELL")
                    .unwrap_or_default()
                    .rsplit('/')
                    .next()
                    .unwrap_or("sh")
                    .to_string()
            });
            let p = ctx.paths.prefix_str();
            match sh.as_str() {
                "fish" => println!("set -gx POUR_PREFIX \"{p}\"; fish_add_path -gP \"{p}/bin\" \"{p}/sbin\";"),
                "pwsh" | "powershell" => println!("$env:POUR_PREFIX = \"{p}\"; $env:PATH = \"{p}\\bin;\" + $env:PATH"),
                // idempotent: a shell that reads several start-up files adds the folders once
                _ => println!("export POUR_PREFIX=\"{p}\"; case \":${{PATH}}:\" in *\":{p}/bin:\"*) ;; *) export PATH=\"{p}/bin:{p}/sbin${{PATH+:$PATH}}\" ;; esac; case \":${{MANPATH:-}}:\" in *\":{p}/share/man:\"*) ;; *) export MANPATH=\"{p}/share/man${{MANPATH+:$MANPATH}}:\" ;; esac; export INFOPATH=\"{p}/share/info:${{INFOPATH:-}}\";"),
            }
        }
        Command::Env => {
            let reg = Registry::load(&ctx.paths).ok();
            let age = reg.as_ref().map(|r| r.core().generated_at()).unwrap_or(0);
            let (nf, nc) = reg.as_ref().map(|r| r.counts()).unwrap_or((0, 0));
            if cli.json {
                println!(
                    "{}",
                    serde_json::json!({"version": env!("CARGO_PKG_VERSION"), "platform": ctx.platform.describe(), "bottle_tag": ctx.platform.native_tag(), "prefix": ctx.paths.prefix_str(), "index_generated_at": age, "formulae": nf, "casks": nc, "installed_formulae": receipt::installed_formulae(&ctx.paths).len(), "installed_casks": receipt::installed_casks(&ctx.paths).len()})
                );
            } else {
                println!("pour {}", env!("CARGO_PKG_VERSION"));
                println!(
                    "platform:   {} · bottles: {}",
                    ctx.platform.describe(),
                    ctx.platform.native_tag()
                );
                println!(
                    "prefix:     {} ({})",
                    ctx.paths.prefix_str(),
                    if ctx.paths.writable() {
                        "writable"
                    } else {
                        "not writable"
                    }
                );
                println!(
                    "index:      {} formulae, {} casks{}",
                    nf,
                    nc,
                    if age > 0 {
                        format!(", generated {}", ago(age))
                    } else {
                        " (run `pour update`)".into()
                    }
                );
                println!(
                    "installed:  {} formulae, {} casks",
                    receipt::installed_formulae(&ctx.paths).len(),
                    receipt::installed_casks(&ctx.paths).len()
                );
                let entries = pour_core::store::entries(&ctx.paths.store());
                let store_bytes: u64 = entries
                    .iter()
                    .map(|(_, _, d)| pour_core::store::tree_size(d))
                    .sum();
                let dl_bytes: u64 = std::fs::read_dir(ctx.paths.downloads())
                    .map(|rd| {
                        rd.flatten()
                            .filter_map(|e| e.metadata().ok())
                            .map(|m| m.len())
                            .sum()
                    })
                    .unwrap_or(0);
                let index_bytes = std::fs::metadata(ctx.paths.index_file())
                    .map(|m| m.len())
                    .unwrap_or(0)
                    + std::fs::metadata(pour_core::registry::details_file(&ctx.paths))
                        .map(|m| m.len())
                        .unwrap_or(0);
                println!(
                    "store:      {} entries · {} (kegs are {} of it, so nothing is stored twice)",
                    entries.len(),
                    human_bytes(store_bytes),
                    if ctx.platform.os == Os::MacOs {
                        "APFS clones"
                    } else {
                        "symlinks"
                    }
                );
                println!(
                    "pour's own: {} binary · {} index · {} download cache",
                    human_bytes(
                        std::env::current_exe()
                            .and_then(std::fs::metadata)
                            .map(|m| m.len())
                            .unwrap_or(0)
                    ),
                    human_bytes(index_bytes),
                    human_bytes(dl_bytes)
                );
                println!(
                    "taps:       {}",
                    registry::taps(&ctx.paths)
                        .iter()
                        .map(|t| t.name.clone())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
        }
        Command::SelfUpdate { check } => {
            let agent = pour_core::net::agent();
            let rel = pour_core::selfupdate::latest(&agent, &ctx.platform).map_err(err)?;
            let cur = env!("CARGO_PKG_VERSION");
            if !pour_core::selfupdate::is_newer(cur, &rel.version) {
                ui.ok(format!("pour {cur} is the newest release"));
                return Ok(());
            }
            ui.head(format!(
                "pour {} is available (you have {cur})",
                rel.version
            ));
            if !rel.notes.trim().is_empty() {
                ui.println(rel.notes.trim());
            }
            if *check {
                return Ok(());
            }
            let exe = pour_core::selfupdate::apply(&agent, &rel, ui).map_err(err)?;
            ui.ok(format!("updated {}", exe.display()));
        }
        Command::Completions { shell } => crate::print_completions(*shell),
        Command::Registry { action } => registry_cmd(&ctx, action)?,
    }
    Ok(())
}

fn ago(ts: u64) -> String {
    let now = receipt::now();
    let d = now.saturating_sub(ts);
    if d < 120 {
        format!("{d} s ago")
    } else if d < 7200 {
        format!("{} min ago", d / 60)
    } else if d < 172_800 {
        format!("{} h ago", d / 3600)
    } else {
        format!("{} days ago", d / 86400)
    }
}

fn print_columns(items: &[String]) {
    if items.is_empty() {
        return;
    }
    let width = console::Term::stdout().size().1 as usize;
    let w = items.iter().map(|s| s.len()).max().unwrap_or(10) + 2;
    let cols = (width / w).max(1);
    for row in items.chunks(cols) {
        let line: String = row.iter().map(|s| format!("{s:<w$}")).collect();
        println!("{}", line.trim_end());
    }
}

fn print_plan(ctx: &Ctx, plan: &resolve::Plan) {
    for w in &plan.warnings {
        ctx.ui.warn(w);
    }
    let pending: Vec<&resolve::Step> = plan.pending().collect();
    if pending.is_empty() {
        return;
    }
    let deps: Vec<String> = pending
        .iter()
        .filter(|s| !s.on_request)
        .map(|s| {
            format!(
                "{} {}",
                s.formula.name,
                style(s.formula.pkg_version()).dim()
            )
        })
        .collect();
    let main: Vec<String> = pending
        .iter()
        .filter(|s| s.on_request)
        .map(|s| {
            format!(
                "{} {}",
                style(&s.formula.name).bold(),
                style(s.formula.pkg_version()).dim()
            )
        })
        .collect();
    if !deps.is_empty() {
        ctx.ui.head(format!("Dependencies: {}", deps.join(", ")));
    }
    if !main.is_empty() {
        ctx.ui.head(format!("Installing: {}", main.join(", ")));
    }
}

/// Newest version known for an installed cask. `live` asks the app's own
/// feed (Pour-native apps) — right before an upgrade, which is about to go
/// to the network anyway; `outdated` stays offline and trusts the index,
/// which the apps registry rebuilds from the same feeds every few hours.
/// `None` when the cask is no longer in the index.
fn cask_latest(
    ctx: &Ctx,
    reg: &Registry,
    i: &receipt::Installed,
    live: bool,
) -> Option<(String, pour_core::model::Cask, String)> {
    let (src, Pkg::Cask(c)) = reg.find(&i.receipt.name, Some(Kind::Cask))? else {
        return None;
    };
    let (_, _, mut latest, _) = c.download_for(&ctx.platform.cask_variations());
    if live {
        if let Some(lc) = &c.livecheck_url {
            if let Some((v, _, _)) =
                pour_core::cask::livecheck(&pour_core::net::agent(), lc, &ctx.platform)
            {
                if !v.is_empty() {
                    latest = v;
                }
            }
        }
    }
    Some((src, c, latest))
}

fn describe_depends_on(json: &str) -> String {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return json.to_string();
    };
    let list = |x: &serde_json::Value| -> Vec<String> {
        match x {
            serde_json::Value::Array(a) => a
                .iter()
                .filter_map(|e| e.as_str().map(str::to_string))
                .collect(),
            serde_json::Value::String(s) => vec![s.clone()],
            _ => Vec::new(),
        }
    };
    let mut parts = Vec::new();
    if let Some(m) = v.get("macos") {
        match m {
            serde_json::Value::Object(o) => {
                for (op, vers) in o {
                    let vers = list(vers).join(", ");
                    parts.push(match op.as_str() {
                        ">=" => format!("macOS {vers} or newer"),
                        "<=" => format!("macOS {vers} or older"),
                        "==" => format!("macOS {vers}"),
                        _ => format!("macOS {op} {vers}"),
                    });
                }
            }
            other => {
                let vers = list(other).join(", ");
                if !vers.is_empty() {
                    parts.push(format!("macOS {vers}"));
                }
            }
        }
    }
    let arch = list(v.get("arch").unwrap_or(&serde_json::Value::Null));
    if !arch.is_empty() {
        parts.push(format!(
            "{} Macs",
            arch.join("/")
                .replace("arm64", "Apple Silicon")
                .replace("x86_64", "Intel")
        ));
    }
    for (key, label) in [("formula", "formula"), ("cask", "cask")] {
        let names = list(v.get(key).unwrap_or(&serde_json::Value::Null));
        if !names.is_empty() {
            parts.push(format!(
                "{label}{} {}",
                if names.len() == 1 { "" } else { "e" },
                names.join(", ")
            ));
        }
    }
    if parts.is_empty() {
        json.to_string()
    } else {
        parts.join(" · ")
    }
}

fn print_summary(ctx: &Ctx, s: &pour_core::install::Summary) {
    for w in &s.warnings {
        ctx.ui.warn(w);
    }
    if !s.installed.is_empty() {
        let line = format!(
            "{} package{} in {} · {} downloaded{}",
            s.installed.len(),
            if s.installed.len() == 1 { "" } else { "s" },
            human_ms(s.ms),
            human_bytes(s.bytes_downloaded),
            if s.store_hits > 0 {
                format!(" · {} from the store", s.store_hits)
            } else {
                String::new()
            }
        );
        ctx.ui.ok(line);
    }
    for (name, text) in &s.caveats {
        ctx.ui.println(format!(
            "\n{} Caveats for {}\n{}",
            arrow(),
            style(name).bold(),
            text.trim()
        ));
    }
}

fn pkg_json(pkg: &Pkg, src: &str) -> serde_json::Value {
    match pkg {
        Pkg::Formula(f) => {
            serde_json::json!({"kind": "formula", "source": src, "name": f.name, "full_name": f.full_name, "tap": f.tap, "desc": f.desc, "homepage": f.homepage, "license": f.license, "version": f.version, "revision": f.revision, "pkg_version": f.pkg_version(), "aliases": f.aliases, "dependencies": f.deps, "build_dependencies": f.build_deps, "linux_dependencies": f.linux_deps, "uses_from_macos": f.uses_from_macos.iter().map(|u| u.name.clone()).collect::<Vec<_>>(), "bottles": f.bottles.iter().map(|b| serde_json::json!({"tag": b.tag, "cellar": b.cellar.as_str(), "sha256": b.sha256})).collect::<Vec<_>>(), "keg_only": f.keg_only, "caveats": f.caveats, "deprecated": f.deprecated, "disabled": f.disabled, "executables": f.executables})
        }
        Pkg::Cask(c) => {
            serde_json::json!({"kind": "cask", "source": src, "token": c.token, "full_token": c.full_token, "tap": c.tap, "names": c.names, "desc": c.desc, "homepage": c.homepage, "version": c.version, "url": c.url, "sha256": c.sha256, "auto_updates": c.auto_updates, "depends_on": c.depends_on.as_ref().and_then(|d| serde_json::from_str::<serde_json::Value>(d).ok()), "artifacts": serde_json::from_str::<serde_json::Value>(&c.artifacts).unwrap_or(serde_json::Value::Null), "native_installers": c.native.iter().map(|n| serde_json::json!({"platform": n.platform, "kind": n.kind, "url": n.url})).collect::<Vec<_>>(), "caveats": c.caveats, "deprecated": c.deprecated, "disabled": c.disabled})
        }
    }
}

fn print_info(ctx: &Ctx, pkg: &Pkg, src: &str) {
    let ui = ctx.ui;
    match pkg {
        Pkg::Formula(f) => {
            let inst = receipt::find_formula(&ctx.paths, &f.name);
            ui.println(format!(
                "{} {}{}",
                arrow(),
                style(&f.full_name).bold(),
                style(format!(
                    ": stable {}{}",
                    f.pkg_version(),
                    if f.keg_only { ", keg-only" } else { "" }
                ))
                .dim()
            ));
            if !f.desc.is_empty() {
                ui.println(&f.desc);
            }
            if !f.homepage.is_empty() {
                ui.println(style(&f.homepage).underlined().to_string());
            }
            if f.deprecated {
                ui.warn(format!(
                    "deprecated{}",
                    f.deprecation_reason
                        .clone()
                        .map(|r| format!(": {r}"))
                        .unwrap_or_default()
                ));
            }
            if f.disabled {
                ui.warn(format!(
                    "disabled{}",
                    f.disable_reason
                        .clone()
                        .map(|r| format!(": {r}"))
                        .unwrap_or_default()
                ));
            }
            match &inst {
                Some(i) => ui.println(format!(
                    "{} {} ({} files){}",
                    style("Installed:").bold(),
                    i.path.display(),
                    i.receipt.links.len().max(i.receipt.links_len),
                    if i.receipt.pinned { " [pinned]" } else { "" }
                )),
                None => ui.println(format!(
                    "{} {}",
                    style("Not installed").dim(),
                    style(format!("— pour install {}", f.name)).dim()
                )),
            }
            let tags = ctx.platform.bottle_tags();
            match f.bottle_for(&tags) {
                Some(b) => ui.println(format!(
                    "{} {} ({})",
                    style("Bottle:").bold(),
                    b.tag,
                    b.cellar.as_str()
                )),
                None => ui.println(format!(
                    "{} none for {} (available: {})",
                    style("Bottle:").bold(),
                    ctx.platform.native_tag(),
                    f.bottles
                        .iter()
                        .map(|b| b.tag.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
            }
            if !f.license.is_empty() {
                ui.println(format!("{} {}", style("License:").bold(), f.license));
            }
            let deps = f.runtime_deps(ctx.platform.os == Os::Linux);
            if !deps.is_empty() {
                let shown: Vec<String> = deps
                    .iter()
                    .map(|d| {
                        if receipt::find_formula(&ctx.paths, d).is_some() {
                            format!("{d} {}", style("✔").green())
                        } else {
                            d.clone()
                        }
                    })
                    .collect();
                ui.println(format!(
                    "{} {}",
                    style("Dependencies:").bold(),
                    shown.join(", ")
                ));
            }
            if !f.build_deps.is_empty() {
                ui.println(format!(
                    "{} {}",
                    style("Build dependencies:").dim(),
                    style(f.build_deps.join(", ")).dim()
                ));
            }
            if !f.aliases.is_empty() {
                ui.println(format!(
                    "{} {}",
                    style("Aliases:").bold(),
                    f.aliases.join(", ")
                ));
            }
            if !f.conflicts_with.is_empty() {
                ui.println(format!(
                    "{} {}",
                    style("Conflicts with:").bold(),
                    f.conflicts_with.join(", ")
                ));
            }
            if let Some(c) = &f.caveats {
                ui.println(format!("{} Caveats\n{}", arrow(), c.trim()));
            }
            if src != "homebrew" {
                ui.println(format!("{} {}", style("Registry:").bold(), src));
            }
        }
        Pkg::Cask(c) => {
            let inst = receipt::find_cask(&ctx.paths, &c.token);
            ui.println(format!(
                "{} {}{}",
                arrow(),
                style(&c.full_token).bold(),
                style(format!(
                    ": {}{}",
                    c.version,
                    if c.auto_updates {
                        " (auto_updates)"
                    } else {
                        ""
                    }
                ))
                .dim()
            ));
            if !c.names.is_empty() {
                ui.println(c.names.join(", "));
            }
            if !c.desc.is_empty() {
                ui.println(&c.desc);
            }
            if !c.homepage.is_empty() {
                ui.println(style(&c.homepage).underlined().to_string());
            }
            match &inst {
                Some(i) => ui.println(format!(
                    "{} {} {}",
                    style("Installed:").bold(),
                    i.receipt.version,
                    style(i.receipt.artifacts.join(", ")).dim()
                )),
                None => ui.println(format!(
                    "{} {}",
                    style("Not installed").dim(),
                    style(format!("— pour install --cask {}", c.token)).dim()
                )),
            }
            let (url, sha, ver, _) = c.download_for(&ctx.platform.cask_variations());
            ui.println(format!(
                "{} {} {}",
                style("Download:").bold(),
                url,
                style(format!(
                    "({}{})",
                    ver,
                    if sha.is_none() {
                        ", sha256 :no_check"
                    } else {
                        ""
                    }
                ))
                .dim()
            ));
            if let Ok(arts) = serde_json::from_str::<serde_json::Value>(&c.artifacts) {
                let mut kinds: BTreeMap<String, Vec<String>> = BTreeMap::new();
                for a in arts.as_array().into_iter().flatten() {
                    for (k, v) in a.as_object().into_iter().flatten() {
                        if [
                            "app",
                            "pkg",
                            "binary",
                            "suite",
                            "font",
                            "manpage",
                            "installer",
                        ]
                        .contains(&k.as_str())
                        {
                            let items: Vec<String> = v
                                .as_array()
                                .map(|x| {
                                    x.iter()
                                        .filter_map(|y| y.as_str().map(str::to_string))
                                        .collect()
                                })
                                .unwrap_or_default();
                            kinds.entry(k.clone()).or_default().extend(items);
                        }
                    }
                }
                for (k, v) in kinds {
                    if k == "font" && v.len() > 3 {
                        ui.println(format!("{} {} fonts", style("Artifacts:").bold(), v.len()));
                    } else {
                        ui.println(format!(
                            "{} {} ({k})",
                            style("Artifacts:").bold(),
                            v.join(", ")
                        ));
                    }
                }
            }
            if !c.native.is_empty() {
                ui.println(format!(
                    "{} {}",
                    style("Also for:").bold(),
                    c.native
                        .iter()
                        .map(|n| n.platform.clone())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            if let Some(d) = &c.depends_on {
                ui.println(format!(
                    "{} {}",
                    style("Requires:").bold(),
                    describe_depends_on(d)
                ));
            }
            if c.deprecated {
                ui.warn(format!(
                    "deprecated{}",
                    c.deprecation_reason
                        .clone()
                        .map(|r| format!(": {r}"))
                        .unwrap_or_default()
                ));
            }
            if let Some(cv) = &c.caveats {
                ui.println(format!("{} Caveats\n{}", arrow(), cv.trim()));
            }
            if src != "homebrew" {
                ui.println(format!("{} {}", style("Registry:").bold(), src));
            }
        }
    }
}

/// `brew "x"`, `cask "y"`, `tap "a/b"[, "url"]`; everything else ignored.
pub fn parse_brewfile(text: &str) -> Brewfile {
    let mut brews = Vec::new();
    let mut casks = Vec::new();
    let mut taps = Vec::new();
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with('#') || l.is_empty() {
            continue;
        }
        let quoted: Vec<String> = l
            .split(['"', '\''])
            .skip(1)
            .step_by(2)
            .map(str::to_string)
            .collect();
        let Some(first) = quoted.first() else {
            continue;
        };
        if l.starts_with("brew ") {
            brews.push(first.clone());
        } else if l.starts_with("cask ") {
            casks.push(first.clone());
        } else if l.starts_with("tap ") {
            taps.push((first.clone(), quoted.get(1).cloned()));
        }
    }
    (brews, casks, taps)
}

fn doctor(ctx: &Ctx) -> Result<()> {
    let ui = ctx.ui;
    let mut problems = 0;
    ui.head(format!(
        "pour {} on {}",
        env!("CARGO_PKG_VERSION"),
        ctx.platform.describe()
    ));
    if !ctx.paths.prefix.exists() {
        problems += 1;
        ui.warn(format!(
            "prefix {} does not exist — run `pour init`",
            ctx.paths.prefix_str()
        ));
    } else if !ctx.paths.writable() {
        problems += 1;
        ui.warn(format!(
            "prefix {} is not writable by you — sudo chown -R $(whoami) {}",
            ctx.paths.prefix_str(),
            ctx.paths.prefix_str()
        ));
    } else {
        ui.ok(format!("prefix {} is writable", ctx.paths.prefix_str()));
    }
    if ctx.paths.prefix_str().len() > "/opt/homebrew".len() && ctx.platform.os == Os::MacOs {
        problems += 1;
        ui.warn("prefix is longer than /opt/homebrew: Mach-O relocation needs a prefix of 13 characters or fewer");
    }
    let on_path = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d == ctx.paths.bin()))
        .unwrap_or(false);
    if on_path {
        ui.ok(format!("{} is on PATH", ctx.paths.bin().display()));
    } else {
        problems += 1;
        ui.warn(format!(
            "{} is not on PATH — add: eval \"$(pour shellenv)\"",
            ctx.paths.bin().display()
        ));
    }
    match Registry::load(&ctx.paths) {
        Ok(r) => {
            let age = receipt::now().saturating_sub(r.core().generated_at());
            if age > 7 * 86400 {
                problems += 1;
                ui.warn(format!(
                    "index is {} days old — run `pour update`",
                    age / 86400
                ));
            } else {
                let (f, c) = r.counts();
                ui.ok(format!(
                    "index has {f} formulae and {c} casks ({})",
                    ago(r.core().generated_at())
                ));
            }
        }
        Err(_) => {
            problems += 1;
            ui.warn("no index — run `pour update`");
        }
    }
    let dangling = pour_core::link::dangling_links(&ctx.paths);
    if !dangling.is_empty() {
        problems += 1;
        ui.warn(format!(
            "{} dangling symlink(s) in the prefix, e.g. {}",
            dangling.len(),
            dangling
                .iter()
                .take(3)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let installed = receipt::installed_formulae(&ctx.paths);
    let names: std::collections::HashSet<&str> =
        installed.iter().map(|i| i.receipt.name.as_str()).collect();
    for i in installed.iter().filter(|i| i.active) {
        for d in &i.receipt.runtime_deps {
            if !names.contains(d.as_str()) {
                problems += 1;
                ui.warn(format!(
                    "{} needs {d}, which is not installed — pour install {d}",
                    i.receipt.name
                ));
            }
        }
    }
    if ctx.platform.os == Os::Linux {
        if ctx.paths.prefix.join("lib/ld.so").exists() {
            ui.ok("lib/ld.so dynamic loader link is in place");
        } else {
            problems += 1;
            ui.warn(
                "lib/ld.so is missing — pour install anything (it is created on first install)",
            );
        }
        if pour_core::linuxsetup::glibc_too_old() {
            problems += 1;
            ui.warn(format!("host glibc is older than {}.{}; Homebrew bottles need Pour's glibc: pour install glibc", pour_core::linuxsetup::MIN_GLIBC.0, pour_core::linuxsetup::MIN_GLIBC.1));
        }
    }
    if ctx.platform.os == Os::MacOs && !std::path::Path::new("/usr/bin/codesign").exists() {
        problems += 1;
        ui.warn("/usr/bin/codesign is missing — relocated binaries cannot be re-signed");
    }
    if let Ok(brew) = std::process::Command::new("brew").arg("--prefix").output() {
        let bp = String::from_utf8_lossy(&brew.stdout).trim().to_string();
        if !bp.is_empty() {
            ui.note(format!("Homebrew is also installed at {bp}; both can coexist (Pour never writes there). `pour migrate` copies its packages."));
        }
    }
    if problems == 0 {
        ui.ok("Your system is ready to pour.");
    } else {
        ui.println(format!(
            "{problems} thing{} to look at.",
            if problems == 1 { "" } else { "s" }
        ));
    }
    Ok(())
}

fn init(ctx: &Ctx, dry_run: bool) -> Result<()> {
    let ui = ctx.ui;
    let p = ctx.paths.prefix_str();
    let steps = vec![
        format!("create {p} and its directories (sudo only if needed)"),
        format!("make {p} owned by you"),
        "fetch the index".to_string(),
        "print the shell line to add".to_string(),
    ];
    ui.head("pour init");
    for s in &steps {
        ui.note(s);
    }
    if dry_run {
        return Ok(());
    }
    if (!ctx.paths.prefix.exists() || !ctx.paths.writable())
        && (std::fs::create_dir_all(&ctx.paths.prefix).is_err() || !ctx.paths.writable())
    {
        let user = std::env::var("USER").unwrap_or_else(|_| "$(whoami)".into());
        #[cfg(unix)]
        {
            let status = std::process::Command::new("sudo")
                .args([
                    "-p",
                    "Password (to create the prefix once): ",
                    "sh",
                    "-c",
                    &format!("mkdir -p '{p}' && chown -R '{user}' '{p}'"),
                ])
                .status()?;
            if !status.success() {
                return Err(err(format!("could not create {p}; create it by hand: sudo mkdir -p {p} && sudo chown -R {user} {p}")));
            }
        }
        #[cfg(not(unix))]
        return Err(err(format!("could not create {p}")));
    }
    ctx.paths.ensure()?;
    if ctx.platform.os == Os::Linux {
        let _ = pour_core::linuxsetup::ensure_ld_so(&ctx.paths, ctx.platform.arch);
    }
    ui.ok(format!("{p} is ready"));
    ctx.update(false)?;
    // ~/.zshenv is read by every zsh — login, non-login (Keel, VS Code) and `zsh -c`
    let shell = std::env::var("SHELL").unwrap_or_default();
    let rc = match shell.rsplit('/').next().unwrap_or("zsh") {
        "bash" => ".bashrc",
        "fish" => ".config/fish/config.fish",
        "zsh" | "" => ".zshenv",
        _ => ".profile",
    };
    ui.println(format!(
        "\nAdd Pour to your shell (once), so every terminal finds it:\n  echo 'eval \"$({p}/bin/pour shellenv)\"' >> ~/{rc}\n"
    ));
    Ok(())
}

fn registry_cmd(ctx: &Ctx, action: &RegistryAction) -> Result<()> {
    match action {
        RegistryAction::Build {
            formulae,
            casks,
            out,
            meta,
            diff,
            diff_out,
            search_out,
        } => {
            let t = std::time::Instant::now();
            let read = |p: &Option<String>| -> Result<serde_json::Value> {
                match p {
                    Some(p) => Ok(serde_json::from_slice(
                        &std::fs::read(p).map_err(|e| err(format!("{p}: {e}")))?,
                    )?),
                    None => Ok(serde_json::Value::Null),
                }
            };
            let f = read(formulae)?;
            let c = read(casks)?;
            let mut m = serde_json::Map::new();
            m.insert("generated_at".into(), receipt::now().into());
            m.insert("pour_version".into(), env!("CARGO_PKG_VERSION").into());
            for kv in meta {
                if let Some((k, v)) = kv.split_once('=') {
                    m.insert(k.into(), v.into());
                }
            }
            let raw = registry::build_from_json(&f, &c, serde_json::Value::Object(m))?;
            let idx = pour_core::Index::from_vec(raw.clone())?;
            let (nf, nc) = (idx.len(Kind::Formula), idx.len(Kind::Cask));
            if let Some(prev) = diff {
                let old = match pour_core::Index::open(std::path::Path::new(prev)) {
                    Ok(i) => i,
                    Err(_) => {
                        let bytes = std::fs::read(prev)?;
                        pour_core::Index::from_vec(registry::normalize_payload(
                            &bytes, "previous",
                        )?)?
                    }
                };
                let d = diff_indexes(&old, &idx);
                ctx.ui.println(format!(
                    "formulae: +{} −{} ~{} · casks: +{} −{} ~{}",
                    d["formulae"]["added"]
                        .as_array()
                        .map(|a| a.len())
                        .unwrap_or(0),
                    d["formulae"]["removed"]
                        .as_array()
                        .map(|a| a.len())
                        .unwrap_or(0),
                    d["formulae"]["updated"]
                        .as_array()
                        .map(|a| a.len())
                        .unwrap_or(0),
                    d["casks"]["added"].as_array().map(|a| a.len()).unwrap_or(0),
                    d["casks"]["removed"]
                        .as_array()
                        .map(|a| a.len())
                        .unwrap_or(0),
                    d["casks"]["updated"]
                        .as_array()
                        .map(|a| a.len())
                        .unwrap_or(0)
                ));
                if let Some(p) = diff_out {
                    std::fs::write(p, serde_json::to_vec_pretty(&d)?)?;
                }
            }
            if let Some(p) = search_out {
                let mut rows: Vec<serde_json::Value> = Vec::new();
                for kind in [Kind::Formula, Kind::Cask] {
                    for i in 0..idx.len(kind) {
                        if let Ok(pkg) = idx.get(kind, i) {
                            rows.push(serde_json::json!([
                                pkg.name(),
                                if kind == Kind::Formula { 0 } else { 1 },
                                pkg.desc(),
                                pkg.version(),
                                pkg.homepage()
                            ]));
                        }
                    }
                }
                let body = serde_json::to_vec(
                    &serde_json::json!({"generated_at": receipt::now(), "formulae": idx.len(Kind::Formula), "casks": idx.len(Kind::Cask), "rows": rows}),
                )?;
                std::fs::write(
                    p,
                    if p.ends_with(".gz") {
                        registry::gzip(&body)?
                    } else {
                        body
                    },
                )?;
            }
            let bytes = if out.ends_with(".gz") {
                registry::gzip(&raw)?
            } else {
                raw.clone()
            };
            std::fs::write(out, &bytes)?;
            ctx.ui.ok(format!(
                "{out}: {nf} formulae, {nc} casks, {} ({} raw) in {}",
                human_bytes(bytes.len() as u64),
                human_bytes(raw.len() as u64),
                human_ms(t.elapsed().as_millis())
            ));
        }
        RegistryAction::Import { file } => {
            let bytes = std::fs::read(file)?;
            let (f, c) = registry::import_core(&ctx.paths, &bytes)?;
            ctx.ui.ok(format!("imported {f} formulae and {c} casks"));
        }
        RegistryAction::Stats { file } => {
            let idx = match file {
                Some(f) => {
                    let bytes = std::fs::read(f).map_err(|e| err(format!("{f}: {e}")))?;
                    pour_core::Index::from_vec(registry::normalize_payload(&bytes, f)?)?
                }
                None => pour_core::Index::open(&ctx.paths.index_file())?,
            };
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &serde_json::json!({"formulae": idx.len(Kind::Formula), "casks": idx.len(Kind::Cask), "generated_at": idx.generated_at(), "meta": idx.meta()})
                )?
            );
        }
    }
    Ok(())
}

/// Added / removed / updated names between two indexes.
pub fn diff_indexes(old: &pour_core::Index, new: &pour_core::Index) -> serde_json::Value {
    let mut out = serde_json::Map::new();
    for (kind, key) in [(Kind::Formula, "formulae"), (Kind::Cask, "casks")] {
        let old_names: std::collections::BTreeMap<String, String> = (0..old.len(kind))
            .filter_map(|i| {
                old.get(kind, i)
                    .ok()
                    .map(|p| (p.name().to_string(), p.version()))
            })
            .collect();
        let new_names: std::collections::BTreeMap<String, String> = (0..new.len(kind))
            .filter_map(|i| {
                new.get(kind, i)
                    .ok()
                    .map(|p| (p.name().to_string(), p.version()))
            })
            .collect();
        let added: Vec<&String> = new_names
            .keys()
            .filter(|n| !old_names.contains_key(*n))
            .collect();
        let removed: Vec<&String> = old_names
            .keys()
            .filter(|n| !new_names.contains_key(*n))
            .collect();
        let updated: Vec<serde_json::Value> = new_names
            .iter()
            .filter(|(n, v)| old_names.get(*n).map(|ov| ov != *v).unwrap_or(false))
            .map(|(n, v)| serde_json::json!({"name": n, "from": old_names[n], "to": v}))
            .collect();
        out.insert(
            key.into(),
            serde_json::json!({"added": added, "removed": removed, "updated": updated}),
        );
    }
    serde_json::Value::Object(out)
}
