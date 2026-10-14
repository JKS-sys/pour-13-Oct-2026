//! Dependency resolution: turn the names a user typed into an ordered plan
//! of bottles to pour, dependencies first.

use crate::model::{Bottle, Formula, Kind, Pkg};
use crate::paths::Paths;
use crate::platform::{Os, Platform};
use crate::receipt;
use crate::registry::Registry;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Install,
    Reinstall,
    Upgrade,
    /// Already installed at this version; nothing to do.
    Keep,
}

#[derive(Debug, Clone)]
pub struct Step {
    pub formula: Formula,
    pub source: String,
    pub bottle: Bottle,
    pub on_request: bool,
    pub action: Action,
    pub installed_version: Option<String>,
    /// Run-time dependencies on this platform (names).
    pub deps: Vec<String>,
}

#[derive(Debug, Default)]
pub struct Plan {
    pub steps: Vec<Step>,
    pub warnings: Vec<String>,
}

impl Plan {
    pub fn pending(&self) -> impl Iterator<Item = &Step> {
        self.steps.iter().filter(|s| s.action != Action::Keep)
    }
    pub fn pending_count(&self) -> usize {
        self.pending().count()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    #[error("No formula or cask named \"{name}\"{hint}")]
    NotFound { name: String, hint: String },
    #[error("{name} has been disabled{reason}")]
    Disabled { name: String, reason: String },
    #[error("{name} has no bottle for {platform} (available: {available})")]
    NoBottle {
        name: String,
        platform: String,
        available: String,
    },
    #[error("dependency cycle involving {0}")]
    Cycle(String),
    #[error("{0} is a cask; use `pour install --cask {0}`")]
    IsCask(String),
    #[error("Homebrew bottles cannot run on {0}; search, info and Pour-native apps still work")]
    Unsupported(String),
}

#[derive(Default)]
pub struct PlanOptions {
    pub reinstall: bool,
    pub upgrade: bool,
    pub only_dependencies: bool,
    pub ignore_installed_deps: bool,
}

fn hint_for(reg: &Registry, name: &str) -> String {
    let s = reg.suggest(name, 3);
    if s.is_empty() {
        String::new()
    } else {
        let names: Vec<String> = s
            .iter()
            .map(|(k, n)| {
                if *k == Kind::Cask {
                    format!("--cask {n}")
                } else {
                    n.clone()
                }
            })
            .collect();
        format!(". Did you mean: {}?", names.join(", "))
    }
}

/// Resolve a formula by name, with the user-facing errors.
pub fn lookup_formula(reg: &Registry, name: &str) -> Result<(String, Formula), PlanError> {
    match reg.find(name, Some(Kind::Formula)) {
        Some((src, Pkg::Formula(f))) => Ok((src, f)),
        _ => {
            if reg.find(name, Some(Kind::Cask)).is_some() {
                return Err(PlanError::IsCask(name.to_string()));
            }
            Err(PlanError::NotFound {
                name: name.to_string(),
                hint: hint_for(reg, name),
            })
        }
    }
}

/// Build an install plan for `names`.
pub fn plan(
    reg: &Registry,
    paths: &Paths,
    platform: &Platform,
    names: &[String],
    opts: &PlanOptions,
) -> Result<Plan, PlanError> {
    if !platform.pours_bottles() {
        return Err(PlanError::Unsupported(platform.describe()));
    }
    let tags = platform.bottle_tags();
    let linux = platform.os == Os::Linux;
    let installed: HashMap<String, String> = receipt::installed_formulae(paths)
        .into_iter()
        .filter(|i| i.active || true)
        .map(|i| (i.receipt.name.clone(), i.receipt.version.clone()))
        .collect();

    let mut plan = Plan::default();
    let mut requested: Vec<(String, Formula)> = Vec::new();
    for n in names {
        let (src, f) = lookup_formula(reg, n)?;
        if f.disabled {
            return Err(PlanError::Disabled {
                name: f.name.clone(),
                reason: f
                    .disable_reason
                    .clone()
                    .map(|r| format!(": {r}"))
                    .unwrap_or_default(),
            });
        }
        if f.deprecated {
            plan.warnings.push(format!(
                "{} is deprecated{}",
                f.name,
                f.deprecation_reason
                    .clone()
                    .map(|r| format!(": {r}"))
                    .unwrap_or_default()
            ));
        }
        requested.push((src, f));
    }

    // Depth-first walk, dependencies before dependents.
    let mut order: Vec<Step> = Vec::new();
    let mut state: HashMap<String, u8> = HashMap::new(); // 1 = visiting, 2 = done
    let requested_names: HashSet<String> = requested.iter().map(|(_, f)| f.name.clone()).collect();

    #[allow(clippy::too_many_arguments)]
    fn visit(
        name: &str,
        on_request: bool,
        reg: &Registry,
        tags: &[String],
        linux: bool,
        installed: &HashMap<String, String>,
        requested_names: &HashSet<String>,
        opts: &PlanOptions,
        state: &mut HashMap<String, u8>,
        order: &mut Vec<Step>,
        warnings: &mut Vec<String>,
        platform: &Platform,
    ) -> Result<(), PlanError> {
        match state.get(name) {
            Some(2) => return Ok(()),
            Some(1) => return Err(PlanError::Cycle(name.to_string())),
            _ => {}
        }
        state.insert(name.to_string(), 1);
        let (src, f) = lookup_formula(reg, name)?;
        // Dependency names may be aliases (`python` for `python@3.15`):
        // record the formula they resolve to, so receipts, `uses` and
        // `doctor` all speak of the same name.
        let deps: Vec<String> = f
            .runtime_deps(linux)
            .into_iter()
            .map(|d| match lookup_formula(reg, &d) {
                Ok((_, df)) => df.name,
                Err(_) => d,
            })
            .collect();
        let installed_version = installed.get(&f.name).cloned();
        let latest = f.pkg_version();
        let action = if on_request && opts.reinstall {
            Action::Reinstall
        } else if let Some(v) = &installed_version {
            if v == &latest {
                Action::Keep
            } else if on_request || opts.upgrade {
                Action::Upgrade
            } else {
                Action::Keep
            }
        } else {
            Action::Install
        };
        // Dependencies first. Installed dependencies are kept unless we are
        // upgrading everything.
        if action != Action::Keep || opts.ignore_installed_deps {
            for d in &deps {
                if requested_names.contains(d) || !installed.contains_key(d) || opts.upgrade {
                    visit(
                        d,
                        requested_names.contains(d),
                        reg,
                        tags,
                        linux,
                        installed,
                        requested_names,
                        opts,
                        state,
                        order,
                        warnings,
                        platform,
                    )?;
                } else {
                    state.insert(d.clone(), 2);
                }
            }
        }
        let bottle = match f.bottle_for(tags) {
            Some(b) => b.clone(),
            None => {
                if action == Action::Keep {
                    state.insert(name.to_string(), 2);
                    return Ok(());
                }
                let available: Vec<&str> = f.bottles.iter().map(|b| b.tag.as_str()).collect();
                return Err(PlanError::NoBottle {
                    name: f.name.clone(),
                    platform: platform.native_tag(),
                    available: if available.is_empty() {
                        "none".into()
                    } else {
                        available.join(", ")
                    },
                });
            }
        };
        if f.deprecated && !on_request {
            warnings.push(format!("dependency {} is deprecated", f.name));
        }
        order.push(Step {
            deps,
            formula: f,
            source: src,
            bottle,
            on_request,
            action,
            installed_version,
        });
        state.insert(name.to_string(), 2);
        Ok(())
    }

    for (_, f) in &requested {
        visit(
            &f.name,
            !opts.only_dependencies,
            reg,
            &tags,
            linux,
            &installed,
            &requested_names,
            opts,
            &mut state,
            &mut order,
            &mut plan.warnings,
            platform,
        )?;
    }
    if opts.only_dependencies {
        order.retain(|s| !requested_names.contains(&s.formula.name));
    }
    plan.steps = order;
    Ok(plan)
}

/// Installed formulae whose index version differs from the installed one.
pub fn outdated(reg: &Registry, paths: &Paths) -> Vec<(receipt::Installed, Formula)> {
    let mut out = Vec::new();
    for i in receipt::installed_formulae(paths) {
        if !i.active || i.receipt.pinned {
            continue;
        }
        if let Some((_, Pkg::Formula(f))) = reg.find_lean(&i.receipt.name, Some(Kind::Formula)) {
            if f.pkg_version() != i.receipt.version {
                out.push((i, f));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Cellar;
    use crate::platform::Arch;
    use crate::registry;
    use serde_json::json;

    fn formula(name: &str, deps: &[&str], tags: &[&str]) -> serde_json::Value {
        let mut files = serde_json::Map::new();
        for t in tags {
            files.insert(
                t.to_string(),
                json!({"cellar": ":any", "sha256": format!("{:0>64}", name.len() + t.len())}),
            );
        }
        json!({"name": name, "full_name": name, "tap": "homebrew/core", "desc": name, "homepage": "h", "versions": {"stable": "1.0"}, "revision": 0,
               "bottle": {"stable": {"rebuild": 0, "root_url": "https://ghcr.io/v2/homebrew/core", "files": files}},
               "dependencies": deps, "build_dependencies": [], "uses_from_macos": []})
    }

    fn setup() -> (tempfile::TempDir, Paths, Registry, Platform) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path().join("pour"));
        paths.ensure().unwrap();
        let core = registry::build_from_json(
            &json!([
                formula("a", &["b", "c"], &["x86_64_linux", "arm64_tahoe"]),
                formula("b", &["c"], &["x86_64_linux", "arm64_tahoe"]),
                formula("c", &[], &["all"]),
                formula("maconly", &[], &["arm64_tahoe"]),
                formula("loop1", &["loop2"], &["all"]),
                formula("loop2", &["loop1"], &["all"]),
            ]),
            &json!([{"token": "a", "full_token": "a", "tap": "homebrew/cask", "name": ["A"], "desc": "", "homepage": "", "version": "1", "sha256": "no_check", "url": "u", "artifacts": []},
                    {"token": "onlycask", "full_token": "onlycask", "tap": "homebrew/cask", "name": ["O"], "desc": "", "homepage": "", "version": "1", "sha256": "no_check", "url": "u", "artifacts": []}]),
            json!({}),
        )
        .unwrap();
        registry::import_core(&paths, &core).unwrap();
        let reg = Registry::load(&paths).unwrap();
        let platform = Platform {
            os: Os::Linux,
            arch: Arch::X86_64,
            macos_major: 0,
            wsl: false,
            ish: false,
        };
        (dir, paths, reg, platform)
    }

    #[test]
    fn dependencies_come_first_and_installed_ones_are_kept() {
        let (_d, paths, reg, platform) = setup();
        let p = plan(
            &reg,
            &paths,
            &platform,
            &["a".into()],
            &PlanOptions::default(),
        )
        .unwrap();
        let names: Vec<&str> = p.steps.iter().map(|s| s.formula.name.as_str()).collect();
        assert_eq!(names, vec!["c", "b", "a"]);
        assert!(p.steps[2].on_request && !p.steps[0].on_request);
        // install c, then plan again: c is kept
        receipt::Receipt {
            kind: Kind::Formula,
            name: "c".into(),
            version: "1.0".into(),
            ..Default::default()
        }
        .write(&paths.keg("c", "1.0"))
        .unwrap();
        crate::link::link_keg(&paths, "c", &paths.keg("c", "1.0"), false, false).unwrap();
        let p = plan(
            &reg,
            &paths,
            &platform,
            &["a".into()],
            &PlanOptions::default(),
        )
        .unwrap();
        let names: Vec<&str> = p.pending().map(|s| s.formula.name.as_str()).collect();
        assert_eq!(names, vec!["b", "a"]);
        // outdated c → upgrade path
        receipt::Receipt {
            kind: Kind::Formula,
            name: "c".into(),
            version: "0.9".into(),
            ..Default::default()
        }
        .write(&paths.keg("c", "1.0"))
        .unwrap();
        let p = plan(
            &reg,
            &paths,
            &platform,
            &["c".into()],
            &PlanOptions::default(),
        )
        .unwrap();
        assert_eq!(p.steps[0].action, Action::Upgrade);
        assert_eq!(outdated(&reg, &paths).len(), 1);
    }

    #[test]
    fn errors_are_specific() {
        let (_d, paths, reg, platform) = setup();
        assert!(matches!(
            plan(
                &reg,
                &paths,
                &platform,
                &["maconly".into()],
                &PlanOptions::default()
            ),
            Err(PlanError::NoBottle { .. })
        ));
        assert!(matches!(
            plan(
                &reg,
                &paths,
                &platform,
                &["loop1".into()],
                &PlanOptions::default()
            ),
            Err(PlanError::Cycle(_))
        ));
        assert!(matches!(
            plan(
                &reg,
                &paths,
                &platform,
                &["onlycask".into()],
                &PlanOptions::default()
            ),
            Err(PlanError::IsCask(_))
        ));
        match plan(
            &reg,
            &paths,
            &platform,
            &["maconlyy".into()],
            &PlanOptions::default(),
        ) {
            Err(PlanError::NotFound { hint, .. }) => assert!(hint.contains("maconly")),
            other => panic!("{other:?}"),
        }
        let win = Platform {
            os: Os::Windows,
            arch: Arch::X86_64,
            macos_major: 0,
            wsl: false,
            ish: false,
        };
        assert!(matches!(
            plan(&reg, &paths, &win, &["a".into()], &PlanOptions::default()),
            Err(PlanError::Unsupported(_))
        ));
    }

    #[test]
    fn bottle_fallback_to_all() {
        let (_d, paths, reg, platform) = setup();
        let p = plan(
            &reg,
            &paths,
            &platform,
            &["c".into()],
            &PlanOptions::default(),
        )
        .unwrap();
        assert_eq!(p.steps[0].bottle.tag, "all");
        assert_eq!(p.steps[0].bottle.cellar, Cellar::Any);
    }
}
