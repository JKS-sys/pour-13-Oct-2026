//! Command implementations.

use crate::paint::{self, Hue};
use crate::ui::{arrow, human_bytes, human_ms, Ui};
use crate::{BundleAction, Cli, Command, KindFlags, RegistryAction, ServicesAction, TapAction};
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
    /// `--adopt`: take over apps already in Applications instead of downloading them.
    adopt: std::cell::Cell<bool>,
}

/// Registries of command-line programs (installed with `pour install NAME`, no --cask).
fn is_tool_tap(t: &str) -> bool {
    matches!(t, "jks-sys/pour-tools" | "jks-sys/pour-windows")
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
        self.ui
            .spin_start("fetching the index of every formula, cask, tool and app");
        let rep = registry::update(&self.paths, force);
        self.ui.spin_stop();
        let show = |name: &str, st: &registry::Status| match st {
            registry::Status::Updated { formulae, casks } => self.ui.ok(format!(
                "{}: {} formulae, {} casks",
                paint::tap(name),
                paint::number(formulae),
                paint::number(casks)
            )),
            registry::Status::Unchanged => self.ui.note(format!(
                "{}: {}",
                paint::tap(name),
                paint::c(Hue::Green, "already up to date")
            )),
            registry::Status::Failed(m) => self.ui.warn(format!("{}: {m}", paint::tap(name))),
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
        self.ui.note(format!(
            "done in {}",
            paint::b(Hue::Gold, human_ms(t.elapsed().as_millis()))
        ));
        self.ui.cue(crate::ui::Cue::Updated);
        Ok(())
    }

    /// Split names into formulae and casks according to the index.
    fn classify(&self, reg: &Registry, names: &[String], kind: Option<Kind>) -> Result<Split> {
        let mut formulae = Vec::new();
        let mut casks = Vec::new();
        for n in names {
            match reg.find(n, kind) {
                Some((src, Pkg::Cask(c))) => {
                    // A Homebrew cask is a macOS app; on Windows/Linux/FreeBSD the app's own
                    // installer from pour-desktop stands in under the same name.
                    let here = self.platform.os == Os::MacOs
                        || !c.native.is_empty()
                        || c.livecheck_url.is_some();
                    match (here, reg.native_alternative(&c.token, &self.platform)) {
                        (false, Some(alt)) => casks.push(alt),
                        _ => casks.push((src, c)),
                    }
                }
                Some((_, Pkg::Formula(f))) => {
                    // No bottle for this system (Windows, FreeBSD, an old Mac)? A tool's own
                    // build from pour-tools stands in, under the same name.
                    // A disabled formula (node@18) is the same story: its own build stands in.
                    let tags = self.platform.bottle_tags();
                    let bottled = f.bottles.iter().any(|b| tags.contains(&b.tag)) && !f.disabled;
                    match (bottled, reg.native_alternative(&f.name, &self.platform)) {
                        (false, Some((src, c))) => casks.push((src, c)),
                        _ => formulae.push(f.name.clone()),
                    }
                }
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
                adopt: self.adopt.get(),
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
                    "  would install {} {}",
                    paint::cask(&c.token),
                    paint::version(&probe.version)
                ));
                continue;
            }
            let opts = CaskOptions {
                force,
                dry_run: false,
                adopt: self.adopt.get(),
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
                .map(|a| a.rsplit(['/', '\\']).next().unwrap_or(a).to_string())
                .fold(Vec::<String>::new(), |mut v, a| {
                    if !v.contains(&a) && a != "files" {
                        v.push(a);
                    }
                    v
                })
                .join(", ");
            self.ui.cue(crate::ui::Cue::Poured);
            if s.adopted {
                self.ui.ok(format!(
                    "{} {} {} {}",
                    paint::cask(&c.token),
                    paint::version(&s.version),
                    paint::b(Hue::Teal, "adopted"),
                    paint::dim("(already in Applications — nothing downloaded)")
                ));
                continue;
            }
            let cli_tool = is_tool_tap(&c.tap);
            self.ui.pour_animation(format!(
                "{} {} {} {}",
                if cli_tool {
                    paint::tool(&c.token)
                } else {
                    paint::cask(&c.token)
                },
                paint::version(&s.version),
                paint::b(Hue::Green, "installed"),
                paint::c(
                    Hue::Indigo,
                    format!(
                        "{} · {}",
                        paint::number(human_bytes(s.bytes)),
                        human_ms(s.ms)
                    )
                )
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
        adopt: std::cell::Cell::new(false),
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
            adopt,
            reinstall,
            only_dependencies,
            skip_post_install,
            overwrite,
            force,
            jobs,
            dry_run,
            names,
        } => {
            ctx.adopt.set(*adopt);
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
                // a formula running as a service is stopped first (and not started at login again)
                let svc = pour_core::services::Services {
                    paths: &ctx.paths,
                    platform: &ctx.platform,
                };
                for f in &formulae {
                    let st = svc.status(f);
                    if st.enabled || st.state == "started" {
                        let _ = svc.stop(f);
                        ui.note(format!("stopped the {} service", paint::formula(f)));
                    }
                }
                let removed = ctx.engine().uninstall(&formulae, *ignore_dependencies)?;
                for r in removed {
                    ui.ok(format!(
                        "{} {}",
                        paint::formula(&r),
                        paint::c(Hue::Orange, "uninstalled")
                    ));
                }
                ui.cue(crate::ui::Cue::Removed);
            }
            for c in casks {
                let warnings = ctx.casks().uninstall(&c, *zap)?;
                for w in warnings {
                    ui.warn(w);
                }
                ui.ok(format!(
                    "{} {}{}",
                    paint::cask(&c),
                    paint::c(Hue::Orange, "uninstalled"),
                    if *zap {
                        paint::c(Hue::Red, " (zapped)")
                    } else {
                        String::new()
                    }
                ));
                ui.cue(crate::ui::Cue::Removed);
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
                                "  {} {} {} {}",
                                paint::formula(&i.receipt.name),
                                paint::old_version(&i.receipt.version),
                                paint::c(Hue::Amber, "→"),
                                paint::new_version(&f.pkg_version())
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
                        ui.cue(crate::ui::Cue::Upgraded);
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
                            "  {} {} {} {}",
                            paint::cask(&c.token),
                            paint::old_version(from),
                            paint::c(Hue::Amber, "→"),
                            paint::new_version(to)
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
                        "{} {} {} {}",
                        paint::formula(&i.receipt.name),
                        paint::old_version(&i.receipt.version),
                        paint::dim("→"),
                        paint::new_version(&f.pkg_version())
                    );
                }
            }
            if k != Some(Kind::Formula) {
                for i in receipt::installed_casks(&ctx.paths) {
                    if let Some((_, _, latest)) = cask_latest(&ctx, &reg, &i, false) {
                        if latest != i.receipt.version {
                            any = true;
                            println!(
                                "{} {} {} {}",
                                paint::cask(&i.receipt.name),
                                paint::old_version(&i.receipt.version),
                                paint::dim("→"),
                                paint::new_version(&latest)
                            );
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
                let v: Vec<serde_json::Value> = hits.iter().map(|(src, h)| serde_json::json!({"name": h.name, "kind": h.kind.to_string(), "desc": h.desc, "source": src, "install": if h.kind == Kind::Cask { format!("pour install --cask {}", h.name) } else { format!("pour install {}", h.name) }})).collect();
                paint::print_json(&serde_json::Value::Array(v));
                return Ok(());
            }
            if hits.is_empty() {
                let s = reg.suggest(&q, 5);
                if s.is_empty() {
                    ui.println(format!(
                        "No formulae or casks found for \"{}\".",
                        paint::b(Hue::Amber, &q)
                    ));
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
            let is_tool = |src: &str| is_tool_tap(src);
            ordered.extend(hits.iter().filter(|(_, h)| h.kind == Kind::Formula));
            ordered.extend(
                hits.iter()
                    .filter(|(s, h)| h.kind == Kind::Cask && is_tool(s)),
            );
            ordered.extend(
                hits.iter()
                    .filter(|(s, h)| h.kind == Kind::Cask && !is_tool(s)),
            );
            let mut last_group = None;
            for (src, h) in ordered {
                let group = if h.kind == Kind::Formula {
                    0
                } else if is_tool(src) {
                    1
                } else {
                    2
                };
                if last_group != Some(group) {
                    last_group = Some(group);
                    match group {
                        0 => ui.section("Formulae (command-line tools · pour install NAME)", 1),
                        1 => ui.section(
                            "Tools with their own builds for every system (pour install NAME)",
                            3,
                        ),
                        _ => ui.section("Casks (apps · pour install --cask NAME)", 0),
                    }
                }
                let mark = if (h.kind == Kind::Formula && installed_f.contains(&h.name))
                    || (h.kind == Kind::Cask && installed_c.contains(&h.name))
                {
                    paint::b(Hue::Green, " ✔")
                } else {
                    String::new()
                };
                let tap = if src == "homebrew" {
                    String::new()
                } else {
                    format!(
                        " {}{}{}",
                        paint::b(paint::DEPTH[0], "["),
                        paint::tap(src),
                        paint::b(paint::DEPTH[0], "]")
                    )
                };
                let desc = if h.desc.is_empty() {
                    String::new()
                } else {
                    format!(": {}", h.desc)
                };
                let name = if is_tool_tap(src) {
                    paint::tool(&h.name)
                } else if h.kind == Kind::Formula {
                    paint::formula(&h.name)
                } else {
                    paint::cask(&h.name)
                };
                ui.println(format!("{name}{mark}{tap}{}", paint::dim(&desc)));
            }
            ui.cue(crate::ui::Cue::Found);
            ui.note(format!(
                "{} result{} in {}",
                paint::number(hits.len()),
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
                paint::print_json(&serde_json::Value::Array(out));
            }
        }
        Command::List {
            kind,
            versions,
            one,
            homebrew,
        } => {
            let k = ctx.kind(kind);
            let formulae = receipt::installed_formulae(&ctx.paths);
            let casks = receipt::installed_casks(&ctx.paths);
            // Scripts (and `pour list | grep`) get Homebrew's shape: one name per line,
            // Pour's own packages only, no headings — `-1`, or whenever stdout is a pipe.
            let piped = !console::Term::stdout().is_term();
            if !cli.json && (*one || (piped && !*homebrew)) {
                let mut names: Vec<String> = Vec::new();
                if k != Some(Kind::Cask) {
                    names.extend(formulae.iter().filter(|i| i.active || *versions).map(|i| {
                        if *versions {
                            format!("{} {}", i.receipt.name, i.receipt.version)
                        } else {
                            i.receipt.name.clone()
                        }
                    }));
                }
                if k != Some(Kind::Formula) {
                    names.extend(casks.iter().map(|i| {
                        if *versions {
                            format!("{} {}", i.receipt.name, i.receipt.version)
                        } else {
                            i.receipt.name.clone()
                        }
                    }));
                }
                for n in names {
                    println!("{n}");
                }
                return Ok(());
            }
            // What Homebrew installed on this machine, read from its Cellar and Caskroom,
            // so `pour list` shows everything — and which of it Pour can install too.
            let (bf, bc) = pour_core::homebrew::installed(&ctx.paths.prefix);
            let reg = Registry::load(&ctx.paths).ok();
            let known = |n: &str, kind: Kind| -> bool {
                reg.as_ref()
                    .is_some_and(|r| r.find(n, Some(kind)).is_some())
            };
            if cli.json {
                let v = serde_json::json!({
                    "formulae": formulae.iter().map(|i| serde_json::json!({"name": i.receipt.name, "version": i.receipt.version, "active": i.active, "on_request": i.receipt.installed_on_request, "deps": i.receipt.runtime_deps})).collect::<Vec<_>>(),
                    "casks": casks.iter().map(|i| serde_json::json!({"token": i.receipt.name, "version": i.receipt.version, "artifacts": i.receipt.artifacts})).collect::<Vec<_>>(),
                    "homebrew": {
                        "formulae": bf.iter().map(|i| serde_json::json!({"name": i.name, "version": i.version, "on_request": i.on_request, "in_pour": known(&i.name, Kind::Formula), "path": i.path})).collect::<Vec<_>>(),
                        "casks": bc.iter().map(|i| serde_json::json!({"token": i.name, "version": i.version, "in_pour": known(&i.name, Kind::Cask), "path": i.path})).collect::<Vec<_>>(),
                    },
                });
                paint::print_json(&v);
                return Ok(());
            }
            let show =
                |name: &str, ver: &str, paint_name: &dyn Fn(&str) -> String| -> (String, usize) {
                    if *versions {
                        (
                            format!("{} {}", paint_name(name), paint::version(ver)),
                            name.len() + 1 + ver.len(),
                        )
                    } else {
                        (paint_name(name), name.len())
                    }
                };
            let mut section = 0;
            if k != Some(Kind::Cask) && !formulae.is_empty() {
                ui.section(
                    format!(
                        "Formulae ({})",
                        formulae.iter().filter(|i| i.active).count()
                    ),
                    1,
                );
                section += 1;
                print_columns_colored(
                    &formulae
                        .iter()
                        .filter(|i| i.active || *versions)
                        .map(|i| show(&i.receipt.name, &i.receipt.version, &|n| paint::formula(n)))
                        .collect::<Vec<_>>(),
                );
            }
            let (tools, apps): (Vec<_>, Vec<_>) =
                casks.iter().partition(|i| is_tool_tap(&i.receipt.tap));
            if k != Some(Kind::Formula) && !tools.is_empty() {
                ui.section(format!("Tools ({})", tools.len()), 3);
                section += 1;
                print_columns_colored(
                    &tools
                        .iter()
                        .map(|i| show(&i.receipt.name, &i.receipt.version, &|n| paint::tool(n)))
                        .collect::<Vec<_>>(),
                );
            }
            if k != Some(Kind::Formula) && !apps.is_empty() {
                ui.section(format!("Casks ({})", apps.len()), 0);
                section += 1;
                print_columns_colored(
                    &apps
                        .iter()
                        .map(|i| show(&i.receipt.name, &i.receipt.version, &|n| paint::cask(n)))
                        .collect::<Vec<_>>(),
                );
            }
            let bf: Vec<_> = bf
                .iter()
                .filter(|i| {
                    k != Some(Kind::Cask) && formulae.iter().all(|p| p.receipt.name != i.name)
                })
                .collect();
            let bc: Vec<_> = bc
                .iter()
                .filter(|i| {
                    k != Some(Kind::Formula) && casks.iter().all(|p| p.receipt.name != i.name)
                })
                .collect();
            if !bf.is_empty() || !bc.is_empty() {
                let mut missing: Vec<String> = Vec::new();
                for (items, kind, label, idx) in [
                    (&bf, Kind::Formula, "Installed by Homebrew — formulae", 9),
                    (&bc, Kind::Cask, "Installed by Homebrew — casks", 5),
                ] {
                    if items.is_empty() {
                        continue;
                    }
                    let in_pour = items.iter().filter(|i| known(&i.name, kind)).count();
                    ui.section(format!("{label} ({}, {in_pour} in Pour)", items.len()), idx);
                    let frac = in_pour as f64 / items.len().max(1) as f64;
                    ui.println(format!(
                        "  {} {} known to Pour",
                        paint::bar(frac, 24, Hue::Green),
                        paint::number(format!("{:.0}%", frac * 100.0))
                    ));
                    print_columns_colored(
                        &items
                            .iter()
                            .map(|i| {
                                let ok = known(&i.name, kind);
                                if !ok {
                                    missing.push(i.name.clone());
                                }
                                let f = |n: &str| {
                                    if ok {
                                        paint::brew(n)
                                    } else {
                                        paint::c(Hue::Red, n)
                                    }
                                };
                                show(&i.name, &i.version, &f)
                            })
                            .collect::<Vec<_>>(),
                    );
                }
                if missing.is_empty() {
                    ui.note(format!(
                        "every one of them is in Pour: {} installs them with Pour (Homebrew stays as it is)",
                        paint::cmd("pour migrate")
                    ));
                } else {
                    ui.note(format!(
                        "{} not in Pour's index yet ({}): run {} — or they came from a tap Pour does not follow (pour tap add)",
                        paint::number(missing.len()),
                        missing.join(", "),
                        paint::cmd("pour update")
                    ));
                }
            } else if section == 0 {
                ui.note(format!(
                    "nothing installed yet — try {}",
                    paint::cmd("pour install jq")
                ));
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
                paint::print_json(&serde_json::Value::Array(out));
                return Ok(());
            }
            let installed: std::collections::HashSet<String> = receipt::installed_casks(&ctx.paths)
                .into_iter()
                .map(|i| i.receipt.name)
                .collect();
            ui.section(format!("Apps by Jagadeesh Kumar S ({})", casks.len()), 0);
            for c in &casks {
                let token = c["token"].as_str().unwrap_or("");
                let name = c["name"].get(0).and_then(|x| x.as_str()).unwrap_or(token);
                let ver = c["version"].as_str().unwrap_or("");
                let new = c.pointer("/pour/source").and_then(|x| x.as_str()) == Some("dashboard");
                let mark = if installed.contains(token) {
                    paint::b(Hue::Green, " ✔")
                } else {
                    String::new()
                };
                let hue = paint::DEPTH[casks
                    .iter()
                    .position(|x| x["token"] == c["token"])
                    .unwrap_or(0)
                    % paint::DEPTH.len()];
                ui.println(format!(
                    "  {}{} {}{}  {}",
                    paint::b(hue, format!("{token:<10}")),
                    mark,
                    paint::version(ver),
                    if new {
                        paint::b(Hue::Lime, " new")
                    } else {
                        String::new()
                    },
                    paint::dim(c["desc"].as_str().unwrap_or(name))
                ));
            }
            ui.println(format!(
                "{} {}",
                paint::b(Hue::Gold, "Install:"),
                paint::cmd("pour install --cask <app>")
            ));
        }
        Command::Register {
            app,
            cli: is_cli,
            name,
            desc,
            homepage,
            version,
            feed,
            urls,
            bins,
            app_bundle,
            key,
        } => {
            let ok = (2..=40).contains(&app.len())
                && app
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
                && app.chars().all(|c| {
                    c.is_ascii_lowercase()
                        || c.is_ascii_digit()
                        || matches!(c, '-' | '.' | '+' | '@')
                });
            if !ok {
                return Err(err("the id must be 2–40 lowercase letters, digits, dashes, dots, + or @, starting with a letter or digit"));
            }
            let mut platforms = serde_json::Map::new();
            for u in urls {
                let (plat, rest) = u
                    .split_once('=')
                    .ok_or_else(|| err(format!("--url {u}: write it as PLATFORM=URL#SHA256")))?;
                let (url, sha) = match rest.rsplit_once('#') {
                    Some((a, b)) if b.len() == 64 && b.chars().all(|c| c.is_ascii_hexdigit()) => {
                        (a, Some(b.to_ascii_lowercase()))
                    }
                    _ => (rest, None),
                };
                platforms.insert(
                    plat.to_string(),
                    serde_json::json!({"url": url, "sha256": sha}),
                );
            }
            let key = key
                .clone()
                .or_else(|| std::env::var("POUR_REGISTER_KEY").ok())
                .or_else(|| std::env::var("APPS_API_KEY").ok())
                .filter(|k| !k.is_empty());
            let body = serde_json::json!({
                "kind": if *is_cli { "cli" } else { "cask" },
                "token": app, "name": name, "desc": desc, "homepage": homepage,
                "version": version, "feed": feed, "platforms": platforms,
                "bin": bins, "app": app_bundle, "key": key,
                "from": ctx.platform.describe(),
            });
            let url = std::env::var("POUR_REGISTER_URL")
                .unwrap_or_else(|_| "https://ipconfig.co.network/pour/api/register".into());
            let (code, text) = pour_core::net::post_json(&pour_core::net::agent(), &url, &body)
                .map_err(|e| err(format!("could not reach {url}: {e}")))?;
            let j: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
            if cli.json {
                paint::print_json(&j);
            }
            let problems: Vec<String> = j["problems"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            if code >= 400 || j["ok"].as_bool() == Some(false) {
                for p in &problems {
                    ui.println(format!("  {} {p}", paint::b(Hue::Red, "✗")));
                }
                return Err(err(j["message"]
                    .as_str()
                    .or(j["error"].as_str())
                    .unwrap_or("it did not qualify")
                    .to_string()));
            }
            ui.cue(crate::ui::Cue::Registered);
            let install = j["install"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| {
                    format!("pour install {}{app}", if *is_cli { "" } else { "--cask " })
                });
            ui.println(format!(
                "{} {} {} — {}",
                paint::b(Hue::Green, "✓"),
                if *is_cli {
                    paint::tool(app)
                } else {
                    paint::cask(app)
                },
                paint::b(
                    Hue::Green,
                    if j["added"].as_bool().unwrap_or(false) {
                        "registered and listed"
                    } else {
                        "updated"
                    }
                ),
                paint::cmd(&install)
            ));
            for c in j["checks"].as_array().into_iter().flatten() {
                if let Some(c) = c.as_str() {
                    ui.println(format!("  {} {}", paint::b(Hue::Green, "✓"), paint::dim(c)));
                }
            }
            if let Some(k) = j["key"].as_str() {
                ui.println(format!(
                    "  {} keep this key to update it later: {}",
                    paint::b(Hue::Amber, "key"),
                    paint::b(Hue::Gold, k)
                ));
            }
            ui.note(format!(
                "everyone gets it after {}",
                paint::cmd("pour update")
            ));
        }
        Command::Services { action } => services_cmd(&ctx, cli, action.as_ref())?,
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
                ui.ok(format!("freed {}", paint::b(Hue::Gold, human_bytes(freed))));
                ui.cue(crate::ui::Cue::Cleaned);
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
                if !registry::remove_tap(&ctx.paths, name)? {
                    return Err(err(format!("no tap named {name}")));
                }
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
            // Read Homebrew's Cellar and Caskroom directly: works without `brew` on PATH
            // (Keel, cron, a shell that never loaded brew shellenv).
            let (bf, bc) = pour_core::homebrew::installed(&ctx.paths.prefix);
            let names: Vec<String> = bf
                .iter()
                .filter(|i| i.on_request)
                .map(|i| i.name.clone())
                .collect();
            let casks: Vec<String> = bc.iter().map(|i| i.name.clone()).collect();
            ui.section(
                format!(
                    "Homebrew has {} formulae on request ({} with dependencies) and {} casks",
                    names.len(),
                    bf.len(),
                    casks.len()
                ),
                9,
            );
            if names.is_empty() && casks.is_empty() {
                ui.note("nothing to migrate: no Homebrew Cellar or Caskroom on this machine");
                return Ok(());
            }
            let reg = ctx.registry()?;
            let (names, unknown): (Vec<String>, Vec<String>) = names
                .into_iter()
                .partition(|n| reg.find(n, Some(Kind::Formula)).is_some());
            if !unknown.is_empty() {
                ui.warn(format!(
                    "not in Pour's index (left with Homebrew): {}",
                    unknown.join(", ")
                ));
            }
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
            // the apps are in Applications already: take them over, download nothing
            ctx.adopt.set(true);
            ctx.install_casks(&reg, &casks, false, *dry_run)?;
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
                println!(
                    "{} {}",
                    paint::b(Hue::Gold, "🍶 pour"),
                    paint::version(env!("CARGO_PKG_VERSION"))
                );
                println!(
                    "{}   {} · bottles: {}",
                    paint::b(Hue::Sky, "platform:"),
                    ctx.platform.describe(),
                    ctx.platform.native_tag()
                );
                println!(
                    "{}     {} ({})",
                    paint::b(Hue::Teal, "prefix:"),
                    ctx.paths.prefix_str(),
                    if ctx.paths.writable() {
                        "writable"
                    } else {
                        "not writable"
                    }
                );
                println!(
                    "{}      {} formulae, {} casks{}",
                    paint::b(Hue::Lime, "index:"),
                    nf,
                    nc,
                    if age > 0 {
                        format!(", generated {}", ago(age))
                    } else {
                        " (run `pour update`)".into()
                    }
                );
                println!(
                    "{}  {} formulae, {} casks",
                    paint::b(Hue::Green, "installed:"),
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
                    "{}      {} entries · {} (kegs are {} of it, so nothing is stored twice)",
                    paint::b(Hue::Violet, "store:"),
                    entries.len(),
                    human_bytes(store_bytes),
                    if ctx.platform.os == Os::MacOs {
                        "APFS clones"
                    } else {
                        "symlinks"
                    }
                );
                println!(
                    "{} {} binary · {} index · {} download cache",
                    paint::b(Hue::Gold, "pour's own:"),
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
                    "{}       {}",
                    paint::b(Hue::Orange, "taps:"),
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

/// Columns of coloured names: (text with colour codes, visible width).
fn print_columns_colored(items: &[(String, usize)]) {
    if items.is_empty() {
        return;
    }
    let width = console::Term::stdout().size().1 as usize;
    let w = items.iter().map(|s| s.1).max().unwrap_or(10) + 2;
    let cols = (width / w).max(1);
    for row in items.chunks(cols) {
        let mut line = String::new();
        for (text, vis) in row {
            line.push_str(text);
            line.push_str(&" ".repeat(w.saturating_sub(*vis)));
        }
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
                paint::c(Hue::Teal, &s.formula.name),
                paint::version(&s.formula.pkg_version())
            )
        })
        .collect();
    let main: Vec<String> = pending
        .iter()
        .filter(|s| s.on_request)
        .map(|s| {
            format!(
                "{} {}",
                paint::formula(&s.formula.name),
                paint::version(&s.formula.pkg_version())
            )
        })
        .collect();
    if !deps.is_empty() {
        ctx.ui.println(format!(
            "{} {}",
            paint::heading("Dependencies:", 3),
            deps.join(&paint::dim(", "))
        ));
    }
    if !main.is_empty() {
        ctx.ui.println(format!(
            "{} {}",
            paint::heading("Installing:", 1),
            main.join(&paint::dim(", "))
        ));
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
            "{} package{} poured in {} {} {} downloaded{}",
            paint::number(s.installed.len()),
            if s.installed.len() == 1 { "" } else { "s" },
            paint::b(Hue::Gold, human_ms(s.ms)),
            paint::dim("·"),
            paint::c(Hue::Sky, human_bytes(s.bytes_downloaded)),
            if s.store_hits > 0 {
                format!(
                    " {} {} from the store",
                    paint::dim("·"),
                    paint::number(s.store_hits)
                )
            } else {
                String::new()
            }
        );
        ctx.ui.pour_animation(line);
    }
    // a framed card: what was poured, with versions
    if s.installed.len() > 1 && console::Term::stdout().is_term() {
        let lines: Vec<String> = s
            .installed
            .iter()
            .enumerate()
            .map(|(i, (n, v, a))| {
                format!(
                    "{} {} {} {}",
                    paint::b(paint::DEPTH[i % paint::DEPTH.len()], "◆"),
                    paint::formula(n),
                    paint::version(v),
                    paint::dim(a)
                )
            })
            .collect();
        ctx.ui.card("poured", &lines);
    }
    // One caveats block per package, with Homebrew's placeholders made real and
    // `brew` commands spoken as Pour's.
    let prefix = ctx.paths.prefix_str();
    let mut grouped: Vec<(String, Vec<String>)> = Vec::new();
    for (name, text) in &s.caveats {
        let t = caveat_text(text, &prefix);
        match grouped.iter_mut().find(|(n, _)| n == name) {
            Some((_, v)) => v.push(t),
            None => grouped.push((name.clone(), vec![t])),
        }
    }
    for (name, texts) in grouped {
        ctx.ui.println(format!(
            "\n{} {}\n{}",
            paint::heading("Caveats for", 5),
            paint::formula(&name),
            texts.join("\n\n")
        ));
    }
    // formulae that can run in the background: say how
    let svc_names: Vec<&String> = s
        .installed
        .iter()
        .map(|(n, _, _)| n)
        .filter(|n| {
            receipt::find_formula(&ctx.paths, n)
                .map(|i| {
                    let (p, u) = pour_core::services::keg_files(&i.path, n);
                    p.is_some() || u.is_some()
                })
                .unwrap_or(false)
        })
        .collect();
    for n in svc_names {
        ctx.ui.println(format!(
            "\n{} {}\n  {}   {}\n  {}    {}\n  {} {}",
            paint::heading("Run in the background:", 2),
            paint::formula(n),
            paint::cmd(&format!("pour services start {n}")),
            paint::dim("# start now and at every login"),
            paint::cmd(&format!("pour services stop {n}")),
            paint::dim("# stop, and not at login"),
            paint::cmd(&format!("pour services restart {n}")),
            paint::dim("# restart · pour services list shows them all")
        ));
    }
}

/// Caveats as Homebrew writes them, for Pour: `$HOMEBREW_PREFIX` → the prefix, `brew` → `pour`.
pub fn caveat_text(text: &str, prefix: &str) -> String {
    text.trim()
        .replace("$(brew --prefix)", prefix)
        .replace("${HOMEBREW_PREFIX}", prefix)
        .replace("$HOMEBREW_PREFIX", prefix)
        .replace("${HOMEBREW_CELLAR}", &format!("{prefix}/Cellar"))
        .replace("$HOMEBREW_CELLAR", &format!("{prefix}/Cellar"))
        .replace("brew services", "pour services")
        .replace("brew install", "pour install")
        .replace("brew link", "pour link")
        .replace("brew unlink", "pour unlink")
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

/// `pour info` on a package Homebrew installed here says so.
fn brew_line(ctx: &Ctx, name: &str, cask: bool) {
    let (f, c) = pour_core::homebrew::installed(&ctx.paths.prefix);
    let hit = if cask { c } else { f };
    if let Some(i) = hit.iter().find(|i| i.name == name) {
        ctx.ui.println(format!(
            "{} {} {} {}",
            paint::b(Hue::Amber, "Homebrew:"),
            paint::version(&i.version),
            paint::path(&i.path.display().to_string()),
            paint::dim("(pour migrate installs it with Pour; Homebrew stays as it is)")
        ));
    }
}

fn print_info(ctx: &Ctx, pkg: &Pkg, src: &str) {
    let ui = ctx.ui;
    match pkg {
        Pkg::Formula(f) => {
            let inst = receipt::find_formula(&ctx.paths, &f.name);
            ui.println(format!(
                "{} {}{} {}{}",
                arrow(),
                paint::formula(&f.full_name),
                paint::dim(": stable"),
                paint::version(&f.pkg_version()),
                if f.keg_only {
                    paint::c(Hue::Orange, ", keg-only")
                } else {
                    String::new()
                }
            ));
            if !f.desc.is_empty() {
                ui.println(&f.desc);
            }
            if !f.homepage.is_empty() {
                ui.println(paint::url(&f.homepage));
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
                    paint::b(Hue::Green, "Installed:"),
                    paint::path(&i.path.display().to_string()),
                    i.receipt.links.len().max(i.receipt.links_len),
                    if i.receipt.pinned { " [pinned]" } else { "" }
                )),
                None => ui.println(format!(
                    "{} {} {}",
                    paint::c(Hue::Slate, "Not installed"),
                    paint::dim("—"),
                    paint::cmd(&format!("pour install {}", f.name))
                )),
            }
            brew_line(ctx, &f.name, false);
            let tags = ctx.platform.bottle_tags();
            match f.bottle_for(&tags) {
                Some(b) => ui.println(format!(
                    "{} {} ({})",
                    paint::b(Hue::Sky, "Bottle:"),
                    paint::c(Hue::Cyan, &b.tag),
                    paint::dim(&b.cellar.as_str())
                )),
                None => ui.println(format!(
                    "{} none for {} (available: {})",
                    paint::b(Hue::Sky, "Bottle:"),
                    ctx.platform.native_tag(),
                    f.bottles
                        .iter()
                        .map(|b| b.tag.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
            }
            if !f.license.is_empty() {
                ui.println(format!(
                    "{} {}",
                    paint::b(Hue::Violet, "License:"),
                    paint::c(Hue::Indigo, &f.license)
                ));
            }
            let deps = f.runtime_deps(ctx.platform.os == Os::Linux);
            if !deps.is_empty() {
                let shown: Vec<String> = deps
                    .iter()
                    .map(|d| {
                        if receipt::find_formula(&ctx.paths, d).is_some() {
                            format!("{} {}", paint::formula(d), paint::b(Hue::Green, "✔"))
                        } else {
                            paint::c(Hue::Teal, d)
                        }
                    })
                    .collect();
                ui.println(format!(
                    "{} {}",
                    paint::b(Hue::Lime, "Dependencies:"),
                    shown.join(", ")
                ));
            }
            if !f.build_deps.is_empty() {
                ui.println(format!(
                    "{} {}",
                    paint::c(Hue::Slate, "Build dependencies:"),
                    paint::dim(&f.build_deps.join(", "))
                ));
            }
            if !f.aliases.is_empty() {
                ui.println(format!(
                    "{} {}",
                    paint::b(Hue::Orange, "Aliases:"),
                    f.aliases.join(", ")
                ));
            }
            if !f.conflicts_with.is_empty() {
                ui.println(format!(
                    "{} {}",
                    paint::b(Hue::Red, "Conflicts with:"),
                    f.conflicts_with.join(", ")
                ));
            }
            if let Some(c) = &f.caveats {
                ui.println(format!("{}\n{}", paint::heading("Caveats", 5), c.trim()));
            }
            if src != "homebrew" {
                ui.println(format!(
                    "{} {}",
                    paint::b(Hue::Violet, "Registry:"),
                    paint::tap(src)
                ));
            }
        }
        Pkg::Cask(c) => {
            let inst = receipt::find_cask(&ctx.paths, &c.token);
            ui.println(format!(
                "{} {}{} {}{}",
                arrow(),
                paint::cask(&c.full_token),
                paint::dim(":"),
                paint::version(&c.version),
                if c.auto_updates {
                    paint::c(Hue::Indigo, " (auto_updates)")
                } else {
                    String::new()
                }
            ));
            if !c.names.is_empty() {
                ui.println(c.names.join(", "));
            }
            if !c.desc.is_empty() {
                ui.println(&c.desc);
            }
            if !c.homepage.is_empty() {
                ui.println(paint::url(&c.homepage));
            }
            match &inst {
                Some(i) => ui.println(format!(
                    "{} {} {}",
                    paint::b(Hue::Green, "Installed:"),
                    paint::version(&i.receipt.version),
                    paint::path(&i.receipt.artifacts.join(", "))
                )),
                None => ui.println(format!(
                    "{} {} {}",
                    paint::c(Hue::Slate, "Not installed"),
                    paint::dim("—"),
                    paint::cmd(&format!(
                        "pour install {}{}",
                        if is_tool_tap(&c.tap) { "" } else { "--cask " },
                        c.token
                    ))
                )),
            }
            brew_line(ctx, &c.token, true);
            let (mut url, mut sha, ver, _) = c.download_for(&ctx.platform.cask_variations());
            if url.is_empty() || ctx.platform.os != Os::MacOs {
                let key = pour_core::cask::native_key(&ctx.platform);
                if let Some(n) = c.native.iter().find(|n| n.platform == key) {
                    url = n.url.clone();
                    sha = n.sha256.clone();
                }
            }
            if url.is_empty() {
                url = format!("none for {}", ctx.platform.describe());
            }
            ui.println(format!(
                "{} {} {}",
                paint::b(Hue::Sky, "Download:"),
                paint::url(&url),
                paint::dim(&format!(
                    "({}{})",
                    ver,
                    if sha.is_none() {
                        ", sha256 :no_check"
                    } else {
                        ""
                    }
                ))
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
                        ui.println(format!(
                            "{} {} fonts",
                            paint::b(Hue::Lime, "Artifacts:"),
                            v.len()
                        ));
                    } else {
                        ui.println(format!(
                            "{} {} ({k})",
                            paint::b(Hue::Lime, "Artifacts:"),
                            v.join(", ")
                        ));
                    }
                }
            }
            if !c.native.is_empty() {
                ui.println(format!(
                    "{} {}",
                    paint::b(Hue::Teal, "Also for:"),
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
                    paint::b(Hue::Orange, "Requires:"),
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
                ui.println(format!("{}\n{}", paint::heading("Caveats", 5), cv.trim()));
            }
            if src != "homebrew" {
                ui.println(format!(
                    "{} {}",
                    paint::b(Hue::Violet, "Registry:"),
                    paint::tap(src)
                ));
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

/// A formula's `service` block from Homebrew's API, for kegs without their own unit file.
fn service_json(name: &str) -> Option<serde_json::Value> {
    let url = format!("https://formulae.brew.sh/api/formula/{name}.json");
    pour_core::net::get_json(&pour_core::net::agent(), &url, &[])
        .ok()
        .and_then(|v| v.get("service").cloned())
        .filter(|s| !s.is_null())
}

fn state_colour(state: &str) -> String {
    let h = if state == "started" {
        Hue::Green
    } else if state.starts_with("error") {
        Hue::Red
    } else if state == "stopped" {
        Hue::Amber
    } else {
        Hue::Slate
    };
    let dot = match h {
        Hue::Green => "●",
        Hue::Red => "✗",
        Hue::Amber => "◐",
        _ => "○",
    };
    format!("{} {}", paint::b(h, dot), paint::b(h, state))
}

fn services_cmd(ctx: &Ctx, cli: &Cli, action: Option<&ServicesAction>) -> Result<()> {
    let ui = ctx.ui;
    let svc = pour_core::services::Services {
        paths: &ctx.paths,
        platform: &ctx.platform,
    };
    let names_of = |names: &[String], all: bool| -> Result<Vec<String>> {
        if all {
            return Ok(svc.list().into_iter().map(|s| s.name).collect());
        }
        if names.is_empty() {
            return Err(err(
                "name a formula (or --all): pour services start jackett",
            ));
        }
        Ok(names.to_vec())
    };
    let show = |st: &pour_core::services::Status, verb: &str, hue: Hue| {
        ui.println(format!(
            "{} {} {}{}",
            paint::b(hue, verb),
            paint::formula(&st.name),
            state_colour(&st.state),
            st.pid
                .map(|p| format!(" {}", paint::dim(&format!("pid {}", paint::number(p)))))
                .unwrap_or_default()
        ));
        if let Some(l) = &st.log {
            ui.note(format!("log: {}", paint::path(l)));
        }
    };
    match action.unwrap_or(&ServicesAction::List) {
        ServicesAction::List => {
            let list = svc.list();
            if cli.json {
                let v: Vec<serde_json::Value> = list.iter().map(|s| serde_json::json!({"name": s.name, "status": s.state, "enabled": s.enabled, "pid": s.pid, "file": s.file, "log": s.log, "label": pour_core::services::label(&s.name)})).collect();
                paint::print_json(&serde_json::Value::Array(v));
                return Ok(());
            }
            if list.is_empty() {
                ui.note(format!(
                    "no installed formula runs as a service yet — e.g. {}",
                    paint::cmd("pour install redis")
                ));
                return Ok(());
            }
            ui.section(format!("Services ({})", list.len()), 2);
            ui.println(format!(
                "  {}{} {}{} {}{} {}",
                paint::b(Hue::Slate, "Name"),
                " ".repeat(18),
                paint::b(Hue::Slate, "Status"),
                " ".repeat(10),
                paint::b(Hue::Slate, "Login"),
                " ".repeat(3),
                paint::b(Hue::Slate, "File")
            ));
            for s in &list {
                let pad = 22usize.saturating_sub(s.name.len());
                let st = state_colour(&s.state);
                let st_pad = 16usize.saturating_sub(s.state.len() + 2);
                ui.println(format!(
                    "  {}{} {}{} {}{} {}",
                    paint::formula(&s.name),
                    " ".repeat(pad),
                    st,
                    " ".repeat(st_pad),
                    if s.enabled {
                        paint::b(Hue::Green, "yes")
                    } else {
                        paint::dim("no")
                    },
                    " ".repeat(if s.enabled { 5 } else { 6 }),
                    s.file
                        .as_ref()
                        .map(|f| paint::path(&f.display().to_string()))
                        .unwrap_or_default()
                ));
            }
        }
        ServicesAction::Start { names, all }
        | ServicesAction::Run { names, all }
        | ServicesAction::Restart { names, all } => {
            let which = match action.unwrap() {
                ServicesAction::Start { .. } => "start",
                ServicesAction::Run { .. } => "run",
                _ => "restart",
            };
            for n in names_of(names, *all)? {
                let local = receipt::find_formula(&ctx.paths, &n)
                    .map(|i| pour_core::services::keg_files(&i.path, &n));
                let need_json = matches!(local, Some((None, None)));
                let json = if need_json { service_json(&n) } else { None };
                ui.spin_start(format!("{which} {n}"));
                let r = match which {
                    "start" => svc.start(&n, json.as_ref()),
                    "run" => svc.run(&n, json.as_ref()),
                    _ => svc.restart(&n, json.as_ref()),
                };
                ui.spin_stop();
                let st = r?;
                show(
                    &st,
                    match which {
                        "start" => "▶ started",
                        "run" => "▶ running",
                        _ => "↻ restarted",
                    },
                    Hue::Green,
                );
                if which == "start" {
                    ui.note(format!(
                        "starts again at every login · stop with {}",
                        paint::cmd(&format!("pour services stop {n}"))
                    ));
                }
            }
            ui.cue(crate::ui::Cue::ServiceUp);
        }
        ServicesAction::Stop { names, all } => {
            for n in names_of(names, *all)? {
                let st = svc.stop(&n)?;
                show(&st, "■ stopped", Hue::Orange);
            }
            ui.cue(crate::ui::Cue::ServiceDown);
        }
        ServicesAction::Info { name } => {
            let st = svc.status(name);
            if cli.json {
                paint::print_json(
                    &serde_json::json!({"name": st.name, "status": st.state, "enabled": st.enabled, "pid": st.pid, "file": st.file, "log": st.log, "label": pour_core::services::label(name)}),
                );
                return Ok(());
            }
            ui.println(format!("{} {}", arrow(), paint::formula(name)));
            ui.println(format!(
                "{} {}",
                paint::b(Hue::Sky, "Status:"),
                state_colour(&st.state)
            ));
            ui.println(format!(
                "{} {}",
                paint::b(Hue::Teal, "Label:"),
                paint::tap(&pour_core::services::label(name))
            ));
            ui.println(format!(
                "{} {}",
                paint::b(Hue::Lime, "At login:"),
                if st.enabled {
                    paint::b(Hue::Green, "yes")
                } else {
                    paint::dim("no")
                }
            ));
            if let Some(p) = st.pid {
                ui.println(format!(
                    "{} {}",
                    paint::b(Hue::Cyan, "PID:"),
                    paint::number(p)
                ));
            }
            if let Some(f) = &st.file {
                ui.println(format!(
                    "{} {}",
                    paint::b(Hue::Violet, "File:"),
                    paint::path(&f.display().to_string())
                ));
            }
            if let Some(l) = &st.log {
                ui.println(format!(
                    "{} {}",
                    paint::b(Hue::Orange, "Log:"),
                    paint::path(l)
                ));
            }
        }
        ServicesAction::Cleanup => {
            let gone = svc.cleanup();
            if gone.is_empty() {
                ui.ok("nothing to clean up");
            } else {
                ui.ok(format!(
                    "removed services of formulae no longer installed: {}",
                    gone.join(", ")
                ));
                ui.cue(crate::ui::Cue::Cleaned);
            }
        }
    }
    Ok(())
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
            {
                let (bf, bc) = pour_core::homebrew::installed(&ctx.paths.prefix);
                ui.note(format!(
                    "Homebrew is also installed at {} with {} formulae and {} casks; both can coexist (Pour never writes there). {} lists them, {} installs them with Pour.",
                    paint::path(&bp.to_string()),
                    paint::number(bf.len()),
                    paint::number(bc.len()),
                    paint::cmd("pour list"),
                    paint::cmd("pour migrate")
                ));
            }
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
