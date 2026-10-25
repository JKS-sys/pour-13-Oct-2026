//! `pour services`: run an installed formula in the background — launchd on macOS,
//! systemd (user units) on Linux. Bottles ship their own service files
//! (`sh.brew.<name>.plist` / `.service`, older ones `homebrew.mxcl.<name>.plist`),
//! already relocated to Pour's prefix when the bottle was poured; Pour gives them its
//! own label (`pour.<name>`), so a Homebrew service of the same formula never clashes.
//! A keg without them gets a unit generated from the formula's `service` block
//! (Homebrew's API), the same fields Homebrew uses.
//!
//! Copyright (c) 2026 Jagadeesh Kumar S. All rights reserved. See LICENSE.

use crate::paths::Paths;
use crate::platform::{Os, Platform};
use crate::receipt;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error("{0} is not installed — pour install {0}")]
    NotInstalled(String),
    #[error("{0} has no service: it is a tool you run yourself, not something that runs in the background")]
    NoService(String),
    #[error("services on {0} are not supported yet; start the program yourself: {1}")]
    Unsupported(String, String),
    #[error("{0}")]
    Failed(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
pub type Result<T> = std::result::Result<T, ServiceError>;

/// What `pour services list` shows for one formula.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub name: String,
    /// started · stopped · error (exit code) · none (not loaded)
    pub state: String,
    /// Starts at login (the unit file is installed).
    pub enabled: bool,
    pub pid: Option<u32>,
    pub file: Option<PathBuf>,
    pub log: Option<String>,
}

pub struct Services<'a> {
    pub paths: &'a Paths,
    pub platform: &'a Platform,
}

pub fn label(name: &str) -> String {
    format!("pour.{name}")
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn uid() -> String {
    #[cfg(unix)]
    {
        extern "C" {
            fn getuid() -> u32;
        }
        unsafe { getuid() }.to_string()
    }
    #[cfg(not(unix))]
    {
        "0".into()
    }
}

fn run(prog: &str, args: &[&str]) -> (bool, String) {
    match Command::new(prog).args(args).output() {
        Ok(o) => (
            o.status.success(),
            format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            ),
        ),
        Err(e) => (false, e.to_string()),
    }
}

/// The service files a keg ships, (launchd plist, systemd unit).
pub fn keg_files(keg: &Path, name: &str) -> (Option<PathBuf>, Option<PathBuf>) {
    let pick = |cands: &[String]| cands.iter().map(|c| keg.join(c)).find(|p| p.is_file());
    (
        pick(&[
            format!("sh.brew.{name}.plist"),
            format!("homebrew.mxcl.{name}.plist"),
        ]),
        pick(&[
            format!("sh.brew.{name}.service"),
            format!("homebrew.{name}.service"),
        ]),
    )
}

/// Replace `$HOMEBREW_PREFIX`-style placeholders in a Homebrew `service` block value.
fn expand(s: &str, prefix: &str) -> String {
    s.replace("$HOMEBREW_PREFIX", prefix)
        .replace("${HOMEBREW_PREFIX}", prefix)
        .replace("$HOMEBREW_CELLAR", &format!("{prefix}/Cellar"))
        .replace("~/", &format!("{}/", home().display()))
}

fn run_args(svc: &Value, prefix: &str, os: Os) -> Vec<String> {
    let pick = |v: &Value| -> Vec<String> {
        match v {
            Value::String(s) => vec![expand(s, prefix)],
            Value::Array(a) => a
                .iter()
                .filter_map(|x| x.as_str())
                .map(|x| expand(x, prefix))
                .collect(),
            _ => Vec::new(),
        }
    };
    match &svc["run"] {
        Value::Object(o) => pick(
            o.get(if os == Os::MacOs { "macos" } else { "linux" })
                .unwrap_or(&Value::Null),
        ),
        v => pick(v),
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// A launchd plist from a Homebrew `service` block.
pub fn plist_from(name: &str, svc: &Value, prefix: &str) -> Option<String> {
    let args = run_args(svc, prefix, Os::MacOs);
    if args.is_empty() {
        return None;
    }
    let mut x = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n");
    x += &format!("\t<key>Label</key>\n\t<string>{}</string>\n", label(name));
    x += "\t<key>ProgramArguments</key>\n\t<array>\n";
    for a in &args {
        x += &format!("\t\t<string>{}</string>\n", xml_escape(a));
    }
    x += "\t</array>\n\t<key>RunAtLoad</key>\n\t<true/>\n";
    let keep = svc["keep_alive"]["always"].as_bool().unwrap_or(false)
        || svc["keep_alive"].as_bool().unwrap_or(false);
    if keep {
        x += "\t<key>KeepAlive</key>\n\t<true/>\n";
    }
    if let Some(i) = svc["interval"].as_u64() {
        x += &format!("\t<key>StartInterval</key>\n\t<integer>{i}</integer>\n");
    }
    for (k, key) in [
        ("working_dir", "WorkingDirectory"),
        ("log_path", "StandardOutPath"),
        ("error_log_path", "StandardErrorPath"),
    ] {
        if let Some(v) = svc[k].as_str() {
            x += &format!(
                "\t<key>{key}</key>\n\t<string>{}</string>\n",
                xml_escape(&expand(v, prefix))
            );
        }
    }
    if let Some(env) = svc["environment_variables"].as_object() {
        x += "\t<key>EnvironmentVariables</key>\n\t<dict>\n";
        for (k, v) in env {
            x += &format!(
                "\t\t<key>{}</key>\n\t\t<string>{}</string>\n",
                xml_escape(k),
                xml_escape(&expand(v.as_str().unwrap_or(""), prefix))
            );
        }
        x += "\t</dict>\n";
    }
    x += "</dict>\n</plist>\n";
    Some(x)
}

/// A systemd user unit from a Homebrew `service` block.
pub fn unit_from(name: &str, svc: &Value, prefix: &str) -> Option<String> {
    let args = run_args(svc, prefix, Os::Linux);
    if args.is_empty() {
        return None;
    }
    let quote = |a: &str| format!("\"{}\"", a.replace('"', "\\\""));
    let keep = svc["keep_alive"]["always"].as_bool().unwrap_or(false)
        || svc["keep_alive"].as_bool().unwrap_or(false);
    let mut u = format!("[Unit]\nDescription=Pour service for {name}\n\n[Install]\nWantedBy=default.target\n\n[Service]\nType=simple\nExecStart={}\n", args.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" "));
    u += if keep {
        "Restart=always\n"
    } else {
        "Restart=on-failure\n"
    };
    if let Some(v) = svc["working_dir"].as_str() {
        u += &format!("WorkingDirectory={}\n", expand(v, prefix));
    }
    if let Some(v) = svc["log_path"].as_str() {
        u += &format!("StandardOutput=append:{}\n", expand(v, prefix));
    }
    if let Some(v) = svc["error_log_path"].as_str() {
        u += &format!("StandardError=append:{}\n", expand(v, prefix));
    }
    if let Some(env) = svc["environment_variables"].as_object() {
        for (k, v) in env {
            u += &format!(
                "Environment=\"{k}={}\"\n",
                expand(v.as_str().unwrap_or(""), prefix)
            );
        }
    }
    Some(u)
}

/// Give a bottle's plist Pour's label.
fn relabel_plist(text: &str, name: &str) -> String {
    if let Some(i) = text.find("<key>Label</key>") {
        let rest = &text[i..];
        if let (Some(a), Some(b)) = (rest.find("<string>"), rest.find("</string>")) {
            if a < b {
                return format!(
                    "{}{}<string>{}{}",
                    &text[..i],
                    &rest[..a],
                    label(name),
                    &rest[b..]
                );
            }
        }
    }
    text.to_string()
}

/// Folders a unit writes to (logs, working directory) must exist before it starts.
fn ensure_dirs(text: &str) {
    for line in text.lines() {
        let l = line.trim();
        let val = l
            .strip_prefix("<string>")
            .and_then(|v| v.strip_suffix("</string>"))
            .map(str::to_string)
            .or_else(|| {
                [
                    "WorkingDirectory=",
                    "StandardOutput=append:",
                    "StandardError=append:",
                ]
                .iter()
                .find_map(|k| l.strip_prefix(k).map(str::to_string))
            });
        if let Some(v) = val {
            let p = PathBuf::from(&v);
            if !p.is_absolute() || !v.contains("/var") && !v.contains("/log") {
                continue;
            }
            let dir = if v.ends_with(".log") || v.ends_with(".txt") {
                p.parent().map(Path::to_path_buf)
            } else {
                Some(p)
            };
            if let Some(d) = dir {
                let _ = std::fs::create_dir_all(d);
            }
        }
    }
}

fn log_of(text: &str) -> Option<String> {
    for line in text.lines() {
        let l = line.trim();
        if let Some(v) = l.strip_prefix("StandardOutput=append:") {
            return Some(v.to_string());
        }
    }
    let i = text.find("<key>StandardOutPath</key>")?;
    let rest = &text[i..];
    let a = rest.find("<string>")? + 8;
    let b = rest[a..].find("</string>")? + a;
    Some(rest[a..b].to_string())
}

impl Services<'_> {
    fn agent_file(&self, name: &str) -> PathBuf {
        match self.platform.os {
            Os::MacOs => {
                if uid() == "0" {
                    PathBuf::from("/Library/LaunchDaemons").join(format!("{}.plist", label(name)))
                } else {
                    home()
                        .join("Library/LaunchAgents")
                        .join(format!("{}.plist", label(name)))
                }
            }
            _ => home()
                .join(".config/systemd/user")
                .join(format!("{}.service", label(name))),
        }
    }

    fn domain(&self) -> String {
        if uid() == "0" {
            "system".into()
        } else {
            format!("gui/{}", uid())
        }
    }

    /// The unit text for `name`, from the keg's own file or generated from `service` JSON.
    pub fn unit_text(&self, name: &str, service_json: Option<&Value>) -> Result<String> {
        let inst = receipt::find_formula(self.paths, name)
            .ok_or_else(|| ServiceError::NotInstalled(name.to_string()))?;
        let (plist, unit) = keg_files(&inst.path, name);
        let prefix = self.paths.prefix_str();
        match self.platform.os {
            Os::MacOs => {
                if let Some(p) = plist {
                    return Ok(relabel_plist(&std::fs::read_to_string(p)?, name));
                }
                service_json
                    .and_then(|s| plist_from(name, s, &prefix))
                    .ok_or_else(|| ServiceError::NoService(name.into()))
            }
            Os::Linux => {
                if let Some(u) = unit {
                    return Ok(std::fs::read_to_string(u)?
                        .replace("Homebrew generated unit", "Pour service"));
                }
                service_json
                    .and_then(|s| unit_from(name, s, &prefix))
                    .ok_or_else(|| ServiceError::NoService(name.into()))
            }
            other => {
                let line = service_json
                    .map(|s| run_args(s, &prefix, Os::Linux).join(" "))
                    .unwrap_or_else(|| format!("{prefix}/opt/{name}/bin/{name}"));
                Err(ServiceError::Unsupported(format!("{other:?}"), line))
            }
        }
    }

    /// Start now and at every login.
    pub fn start(&self, name: &str, service_json: Option<&Value>) -> Result<Status> {
        self.load(name, service_json, true)
    }

    /// Start now only (nothing left behind for the next login).
    pub fn run(&self, name: &str, service_json: Option<&Value>) -> Result<Status> {
        self.load(name, service_json, false)
    }

    fn load(&self, name: &str, service_json: Option<&Value>, at_login: bool) -> Result<Status> {
        let r = self.load_inner(name, service_json, at_login);
        if r.is_err() {
            // nothing half-installed: a unit that did not start does not stay for the next login
            let _ = std::fs::remove_file(self.agent_file(name));
            let _ = std::fs::remove_file(
                self.paths
                    .var()
                    .join("pour/services")
                    .join(self.agent_file(name).file_name().unwrap_or_default()),
            );
        }
        r
    }

    fn load_inner(
        &self,
        name: &str,
        service_json: Option<&Value>,
        at_login: bool,
    ) -> Result<Status> {
        let text = self.unit_text(name, service_json)?;
        ensure_dirs(&text);
        let file = if at_login {
            self.agent_file(name)
        } else {
            self.paths
                .var()
                .join("pour/services")
                .join(self.agent_file(name).file_name().unwrap_or_default())
        };
        if let Some(d) = file.parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::write(&file, &text)?;
        let f = file.to_string_lossy().into_owned();
        match self.platform.os {
            Os::MacOs => {
                let target = format!("{}/{}", self.domain(), label(name));
                let _ = run("launchctl", &["bootout", &target]);
                let (ok, out) = run("launchctl", &["bootstrap", &self.domain(), &f]);
                if !ok {
                    // older macOS: load -w
                    let (ok2, out2) = run("launchctl", &["load", "-w", &f]);
                    if !ok2 {
                        return Err(ServiceError::Failed(format!(
                            "launchctl could not start {name}: {} {}",
                            out.trim(),
                            out2.trim()
                        )));
                    }
                }
                if at_login {
                    let _ = run("launchctl", &["enable", &target]);
                }
            }
            _ => {
                let (ok, out) = run("systemctl", &["--user", "daemon-reload"]);
                if !ok {
                    return Err(ServiceError::Failed(format!(
                        "systemd (systemctl --user) is not available here: {}",
                        out.trim()
                    )));
                }
                let unit = format!("{}.service", label(name));
                if !at_login {
                    // a transient start: the unit must be where systemd looks, but not enabled
                    let user = self.agent_file(name);
                    if let Some(d) = user.parent() {
                        std::fs::create_dir_all(d)?;
                    }
                    std::fs::write(&user, &text)?;
                    let _ = run("systemctl", &["--user", "daemon-reload"]);
                }
                let args: Vec<&str> = if at_login {
                    vec!["--user", "enable", "--now", &unit]
                } else {
                    vec!["--user", "start", &unit]
                };
                let (ok, out) = run("systemctl", &args);
                if !ok {
                    return Err(ServiceError::Failed(format!(
                        "systemctl could not start {name}: {}",
                        out.trim()
                    )));
                }
            }
        }
        Ok(self.status(name))
    }

    /// Stop now and at login; removes the unit file.
    pub fn stop(&self, name: &str) -> Result<Status> {
        match self.platform.os {
            Os::MacOs => {
                let target = format!("{}/{}", self.domain(), label(name));
                let _ = run("launchctl", &["bootout", &target]);
                let _ = run("launchctl", &["disable", &target]);
            }
            Os::Linux => {
                let unit = format!("{}.service", label(name));
                let _ = run("systemctl", &["--user", "disable", "--now", &unit]);
            }
            _ => {}
        }
        let _ = std::fs::remove_file(self.agent_file(name));
        let _ = std::fs::remove_file(
            self.paths
                .var()
                .join("pour/services")
                .join(self.agent_file(name).file_name().unwrap_or_default()),
        );
        if self.platform.os == Os::Linux {
            let _ = run("systemctl", &["--user", "daemon-reload"]);
        }
        Ok(self.status(name))
    }

    pub fn restart(&self, name: &str, service_json: Option<&Value>) -> Result<Status> {
        let transient = self
            .paths
            .var()
            .join("pour/services")
            .join(self.agent_file(name).file_name().unwrap_or_default());
        let at_login = !transient.exists();
        let _ = self.stop(name);
        self.load(name, service_json, at_login)
    }

    pub fn status(&self, name: &str) -> Status {
        let file = self.agent_file(name);
        let enabled = file.exists();
        let text = std::fs::read_to_string(&file)
            .or_else(|_| {
                std::fs::read_to_string(
                    self.paths
                        .var()
                        .join("pour/services")
                        .join(file.file_name().unwrap_or_default()),
                )
            })
            .unwrap_or_default();
        let mut st = Status {
            name: name.into(),
            state: "none".into(),
            enabled,
            pid: None,
            file: enabled.then_some(file),
            log: log_of(&text),
        };
        match self.platform.os {
            Os::MacOs => {
                let (ok, out) = run(
                    "launchctl",
                    &["print", &format!("{}/{}", self.domain(), label(name))],
                );
                if ok {
                    let field = |k: &str| {
                        out.lines()
                            .find_map(|l| l.trim().strip_prefix(k).map(|v| v.trim().to_string()))
                    };
                    st.pid = field("pid = ").and_then(|p| p.parse().ok());
                    let exit = field("last exit code = ").unwrap_or_default();
                    st.state = if st.pid.is_some() {
                        "started".into()
                    } else if !exit.is_empty() && exit != "0" && exit != "(never exited)" {
                        format!("error ({exit})")
                    } else {
                        "stopped".into()
                    };
                }
            }
            Os::Linux => {
                let unit = format!("{}.service", label(name));
                let (_, out) = run("systemctl", &["--user", "is-active", &unit]);
                st.state = match out.trim() {
                    "active" | "activating" | "reloading" => "started".into(),
                    "failed" => "error".into(),
                    "inactive" if enabled => "stopped".into(),
                    _ => "none".into(),
                };
                let (_, pid) = run(
                    "systemctl",
                    &["--user", "show", "-p", "MainPID", "--value", &unit],
                );
                st.pid = pid.trim().parse().ok().filter(|p: &u32| *p > 0);
            }
            _ => {}
        }
        st
    }

    /// Every installed formula that can run as a service, with its status.
    pub fn list(&self) -> Vec<Status> {
        let mut out: Vec<Status> = receipt::installed_formulae(self.paths)
            .into_iter()
            .filter(|i| i.active)
            .filter(|i| {
                let (p, u) = keg_files(&i.path, &i.receipt.name);
                p.is_some() || u.is_some() || self.agent_file(&i.receipt.name).exists()
            })
            .map(|i| self.status(&i.receipt.name))
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Unit files whose formula is gone: stop and remove them.
    pub fn cleanup(&self) -> Vec<String> {
        let dir = match self.agent_file("x").parent() {
            Some(d) => d.to_path_buf(),
            None => return Vec::new(),
        };
        let mut removed = Vec::new();
        for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let f = e.file_name().to_string_lossy().into_owned();
            let Some(name) = f.strip_prefix("pour.").and_then(|n| {
                n.strip_suffix(".plist")
                    .or_else(|| n.strip_suffix(".service"))
            }) else {
                continue;
            };
            if receipt::find_formula(self.paths, name).is_none() {
                let _ = self.stop(name);
                removed.push(name.to_string());
            }
        }
        removed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn generated_units_carry_the_service_block() {
        let svc = json!({"run": "$HOMEBREW_PREFIX/opt/jackett/bin/jackett", "run_type": "immediate", "keep_alive": {"always": true},
            "working_dir": "$HOMEBREW_PREFIX/opt/jackett/libexec", "log_path": "$HOMEBREW_PREFIX/var/log/jackett.log", "error_log_path": "$HOMEBREW_PREFIX/var/log/jackett.log"});
        let p = plist_from("jackett", &svc, "/opt/pour").unwrap();
        assert!(p.contains("<string>pour.jackett</string>"));
        assert!(p.contains("<string>/opt/pour/opt/jackett/bin/jackett</string>"));
        assert!(p.contains("<key>KeepAlive</key>"));
        assert!(p.contains("/opt/pour/var/log/jackett.log"));
        let u = unit_from("jackett", &svc, "/opt/pour").unwrap();
        assert!(u.contains("ExecStart=\"/opt/pour/opt/jackett/bin/jackett\""));
        assert!(u.contains("Restart=always"));
        assert!(u.contains("WorkingDirectory=/opt/pour/opt/jackett/libexec"));
        let os_specific = json!({"run": {"macos": ["$HOMEBREW_PREFIX/bin/a", "-x"], "linux": ["$HOMEBREW_PREFIX/bin/b"]}});
        assert!(plist_from("a", &os_specific, "/p")
            .unwrap()
            .contains("<string>-x</string>"));
        assert!(unit_from("a", &os_specific, "/p")
            .unwrap()
            .contains("/p/bin/b"));
        assert!(plist_from("a", &json!({}), "/p").is_none());
    }

    #[test]
    fn bottle_plists_get_pours_label() {
        let t = "<dict>\n\t<key>KeepAlive</key>\n\t<true/>\n\t<key>Label</key>\n\t<string>sh.brew.redis</string>\n</dict>";
        let r = relabel_plist(t, "redis");
        assert!(r.contains("<string>pour.redis</string>") && !r.contains("sh.brew.redis"));
        assert_eq!(
            log_of("StandardOutput=append:/opt/pour/var/log/redis.log\n").as_deref(),
            Some("/opt/pour/var/log/redis.log")
        );
    }
}
