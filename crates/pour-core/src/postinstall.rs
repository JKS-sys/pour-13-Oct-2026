//! Declarative post-install steps, as published in Homebrew's API
//! (`post_install_steps`). Pour runs them natively — no Ruby.

use crate::paths::Paths;
use crate::platform::{Os, Platform};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct Ctx<'a> {
    pub paths: &'a Paths,
    pub platform: &'a Platform,
    pub name: String,
    pub version: String,
    pub keg: PathBuf,
}

#[derive(Debug, Default)]
pub struct StepReport {
    pub ran: usize,
    pub skipped: Vec<String>,
    pub warnings: Vec<String>,
    pub notes: Vec<String>,
}

impl Ctx<'_> {
    fn major(&self) -> String {
        self.version
            .split(['.', '_'])
            .next()
            .unwrap_or("")
            .to_string()
    }
    fn major_minor(&self) -> String {
        let parts: Vec<&str> = self
            .version
            .split('_')
            .next()
            .unwrap_or("")
            .split('.')
            .collect();
        if parts.len() >= 2 {
            format!("{}.{}", parts[0], parts[1])
        } else {
            self.version.clone()
        }
    }

    /// Expand `{{var}}` templates.
    pub fn expand(&self, s: &str) -> String {
        let prefix = self.paths.prefix_str();
        let keg = self.keg.to_string_lossy().into_owned();
        let user = std::env::var("USER")
            .or_else(|_| std::env::var("USERNAME"))
            .unwrap_or_else(|_| "user".into());
        let pairs = [
            ("{{HOMEBREW_PREFIX}}", prefix.clone()),
            (
                "{{HOMEBREW_CELLAR}}",
                self.paths.cellar().to_string_lossy().into_owned(),
            ),
            ("{{prefix}}", keg.clone()),
            (
                "{{opt_prefix}}",
                self.paths
                    .opt_link(&self.name)
                    .to_string_lossy()
                    .into_owned(),
            ),
            ("{{bin}}", format!("{keg}/bin")),
            ("{{sbin}}", format!("{keg}/sbin")),
            ("{{lib}}", format!("{keg}/lib")),
            ("{{libexec}}", format!("{keg}/libexec")),
            ("{{share}}", format!("{keg}/share")),
            ("{{pkgshare}}", format!("{keg}/share/{}", self.name)),
            ("{{include}}", format!("{keg}/include")),
            ("{{frameworks}}", format!("{keg}/Frameworks")),
            ("{{etc}}", format!("{prefix}/etc")),
            ("{{pkgetc}}", format!("{prefix}/etc/{}", self.name)),
            ("{{var}}", format!("{prefix}/var")),
            ("{{formula_name}}", self.name.clone()),
            ("{{version}}", self.version.clone()),
            ("{{version.major}}", self.major()),
            ("{{version.major_minor}}", self.major_minor()),
            ("{{user}}", user),
            (
                "{{bash_completion}}",
                format!("{keg}/etc/bash_completion.d"),
            ),
            (
                "{{zsh_completion}}",
                format!("{keg}/share/zsh/site-functions"),
            ),
            (
                "{{fish_completion}}",
                format!("{keg}/share/fish/vendor_completions.d"),
            ),
            (
                "{{pwsh_completion}}",
                format!("{keg}/share/pwsh/completions"),
            ),
        ];
        let mut out = s.to_string();
        for (k, v) in pairs {
            if out.contains(k) {
                out = out.replace(k, &v);
            }
        }
        out
    }

    fn base(&self, base: &str) -> PathBuf {
        let keg = &self.keg;
        match base {
            "homebrew_prefix" => self.paths.prefix.clone(),
            "var" => self.paths.var(),
            "etc" => self.paths.etc(),
            "prefix" => keg.clone(),
            "libexec" => keg.join("libexec"),
            "bin" => keg.join("bin"),
            "sbin" => keg.join("sbin"),
            "lib" => keg.join("lib"),
            "share" => keg.join("share"),
            "include" => keg.join("include"),
            "frameworks" => keg.join("Frameworks"),
            "opt_prefix" => self.paths.opt_link(&self.name),
            other => PathBuf::from(self.expand(other)),
        }
    }

    /// A `{"base": ..., "path": ...}` object or a bare string.
    pub fn path(&self, v: &Value) -> Option<PathBuf> {
        match v {
            Value::String(s) => Some(PathBuf::from(self.expand(s))),
            Value::Object(o) => {
                let p = o
                    .get("path")
                    .and_then(|x| x.as_str())
                    .map(|s| self.expand(s))
                    .unwrap_or_default();
                match o.get("base").and_then(|b| b.as_str()) {
                    Some(b) => Some(if p.is_empty() {
                        self.base(b)
                    } else {
                        self.base(b).join(p)
                    }),
                    None if !p.is_empty() => Some(PathBuf::from(p)),
                    None => None,
                }
            }
            _ => None,
        }
    }

    fn glob_paths(&self, v: &Value) -> Vec<PathBuf> {
        let Some(p) = self.path(v) else {
            return Vec::new();
        };
        let s = p.to_string_lossy().into_owned();
        if s.contains(['*', '?', '{', '[']) {
            expand_glob(&s)
        } else {
            vec![p]
        }
    }

    fn guards_pass(&self, step: &Value) -> bool {
        let Some(guards) = step.get("guards").and_then(|g| g.as_array()) else {
            return true;
        };
        guards.iter().all(|g| {
            let cond = g.get("condition").and_then(|c| c.as_str()).unwrap_or("");
            match cond {
                "on" => {
                    let want = g.get("value").and_then(|v| v.as_str()).unwrap_or("");
                    match want {
                        "macos" => self.platform.os == Os::MacOs,
                        "linux" => self.platform.os == Os::Linux,
                        "arm" => self.platform.arch == crate::platform::Arch::Arm64,
                        "intel" => self.platform.arch == crate::platform::Arch::X86_64,
                        _ => true,
                    }
                }
                "if_exists" => !self
                    .glob_paths(g)
                    .iter()
                    .filter(|p| p.exists() || p.is_symlink())
                    .count()
                    .eq(&0),
                "unless_exists" => {
                    self.glob_paths(g)
                        .iter()
                        .filter(|p| p.exists() || p.is_symlink())
                        .count()
                        == 0
                }
                _ => true,
            }
        })
    }
}

/// Expand a glob with `{a,b}` alternatives (not supported by the glob crate).
fn expand_glob(pattern: &str) -> Vec<PathBuf> {
    if let (Some(s), Some(e)) = (pattern.find('{'), pattern.find('}')) {
        if s < e {
            let mut out = Vec::new();
            for alt in pattern[s + 1..e].split(',') {
                out.extend(expand_glob(&format!(
                    "{}{}{}",
                    &pattern[..s],
                    alt,
                    &pattern[e + 1..]
                )));
            }
            return out;
        }
    }
    glob::glob(pattern)
        .map(|it| it.flatten().collect())
        .unwrap_or_default()
}

fn run_cmd(cmd: &mut Command) -> Result<String, String> {
    match cmd.output() {
        Ok(o) if o.status.success() => Ok(String::from_utf8_lossy(&o.stdout).into_owned()),
        Ok(o) => Err(format!(
            "exit {}: {}",
            o.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&o.stderr).trim()
        )),
        Err(e) => Err(e.to_string()),
    }
}

fn parse_mode(spec: &str, current: u32) -> u32 {
    if let Ok(octal) = u32::from_str_radix(spec.trim_start_matches('0').trim(), 8) {
        if spec.chars().all(|c| c.is_ascii_digit()) {
            return octal;
        }
    }
    // symbolic: u+w, go-w, a+x …
    let mut mode = current;
    for clause in spec.split(',') {
        let Some(op_pos) = clause.find(['+', '-', '=']) else {
            continue;
        };
        let (who, rest) = clause.split_at(op_pos);
        let op = rest.chars().next().unwrap();
        let perms = &rest[1..];
        let mut mask = 0u32;
        let who = if who.is_empty() { "a" } else { who };
        for w in who.chars() {
            let bits = perms.chars().fold(0u32, |acc, p| {
                acc | match p {
                    'r' => 4,
                    'w' => 2,
                    'x' => 1,
                    _ => 0,
                }
            });
            mask |= match w {
                'u' => bits << 6,
                'g' => bits << 3,
                'o' => bits,
                'a' => (bits << 6) | (bits << 3) | bits,
                _ => 0,
            };
        }
        mode = match op {
            '+' => mode | mask,
            '-' => mode & !mask,
            _ => mask,
        };
    }
    mode
}

fn set_mode(p: &Path, mode_spec: &str) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let cur = std::fs::metadata(p)?.permissions().mode() & 0o7777;
        std::fs::set_permissions(
            p,
            std::fs::Permissions::from_mode(parse_mode(mode_spec, cur)),
        )
    }
    #[cfg(not(unix))]
    {
        let _ = (p, mode_spec);
        Ok(())
    }
}

/// Run every step; failures become warnings, never aborts.
pub fn run(ctx: &Ctx, steps_json: &str) -> StepReport {
    let mut rep = StepReport::default();
    let steps: Vec<Value> = match serde_json::from_str(steps_json) {
        Ok(Value::Array(a)) => a,
        _ => return rep,
    };
    for step in steps {
        let ty = step.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if !ctx.guards_pass(&step) {
            rep.skipped.push(format!("{ty} (guard)"));
            continue;
        }
        match run_step(ctx, ty, &step) {
            Ok(Some(note)) => {
                rep.ran += 1;
                rep.notes.push(note);
            }
            Ok(None) => rep.ran += 1,
            Err(e) => rep.warnings.push(format!("post-install {ty}: {e}")),
        }
    }
    rep
}

fn run_step(ctx: &Ctx, ty: &str, step: &Value) -> Result<Option<String>, String> {
    let bin = |name: &str| -> PathBuf {
        let k = ctx.keg.join("bin").join(name);
        if k.exists() {
            k
        } else {
            ctx.paths.bin().join(name)
        }
    };
    match ty {
        "mkdir_p" => {
            for p in step
                .get("paths")
                .and_then(|p| p.as_array())
                .cloned()
                .unwrap_or_else(|| vec![step.get("path").cloned().unwrap_or(Value::Null)])
            {
                if let Some(p) = ctx.path(&p) {
                    std::fs::create_dir_all(&p).map_err(|e| format!("{}: {e}", p.display()))?;
                }
            }
            Ok(None)
        }
        "touch" => {
            let p = ctx
                .path(step.get("path").unwrap_or(&Value::Null))
                .ok_or("no path")?;
            if let Some(d) = p.parent() {
                std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
            }
            if !p.exists() {
                std::fs::write(&p, b"").map_err(|e| e.to_string())?;
            }
            Ok(None)
        }
        "write" => {
            let p = ctx
                .path(step.get("path").unwrap_or(&Value::Null))
                .ok_or("no path")?;
            let overwrite = step
                .get("overwrite")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if p.exists() && !overwrite {
                return Ok(None);
            }
            if let Some(d) = p.parent() {
                std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
            }
            let content = ctx.expand(step.get("content").and_then(|c| c.as_str()).unwrap_or(""));
            std::fs::write(&p, content).map_err(|e| e.to_string())?;
            Ok(None)
        }
        "symlink" => {
            let src = ctx
                .path(step.get("source").unwrap_or(&Value::Null))
                .ok_or("no source")?;
            let dst = ctx
                .path(step.get("target").unwrap_or(&Value::Null))
                .ok_or("no target")?;
            let force = step.get("force").and_then(|v| v.as_bool()).unwrap_or(false);
            if let Some(d) = dst.parent() {
                std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
            }
            if dst.is_symlink() || dst.exists() {
                if force {
                    let _ = if dst.is_dir() && !dst.is_symlink() {
                        std::fs::remove_dir_all(&dst)
                    } else {
                        std::fs::remove_file(&dst)
                    };
                } else {
                    return Ok(None);
                }
            }
            #[cfg(unix)]
            std::os::unix::fs::symlink(&src, &dst).map_err(|e| e.to_string())?;
            #[cfg(windows)]
            std::os::windows::fs::symlink_file(&src, &dst).map_err(|e| e.to_string())?;
            Ok(None)
        }
        "copy" | "move" => {
            let src = ctx
                .path(step.get("source").unwrap_or(&Value::Null))
                .ok_or("no source")?;
            let dst = ctx
                .path(step.get("target").unwrap_or(&Value::Null))
                .ok_or("no target")?;
            let overwrite = step
                .get("overwrite")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if dst.exists() && !overwrite {
                return Ok(None);
            }
            if let Some(d) = dst.parent() {
                std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
            }
            if ty == "move" {
                if dst.exists() {
                    let _ = crate::store::remove_tree(&dst);
                }
                if std::fs::rename(&src, &dst).is_err() {
                    copy_recursive(&src, &dst).map_err(|e| e.to_string())?;
                    let _ = crate::store::remove_tree(&src);
                }
            } else {
                copy_recursive(&src, &dst).map_err(|e| e.to_string())?;
            }
            Ok(None)
        }
        "remove" => {
            let recursive = step
                .get("recursive")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            for p in step
                .get("paths")
                .and_then(|p| p.as_array())
                .cloned()
                .unwrap_or_default()
            {
                for p in ctx.glob_paths(&p) {
                    if p.is_dir() && !p.is_symlink() {
                        if recursive {
                            crate::store::remove_tree(&p).map_err(|e| e.to_string())?;
                        } else {
                            let _ = std::fs::remove_dir(&p);
                        }
                    } else if p.exists() || p.is_symlink() {
                        std::fs::remove_file(&p).map_err(|e| e.to_string())?;
                    }
                }
            }
            Ok(None)
        }
        "set_permissions" => {
            let mode = step
                .get("permissions")
                .and_then(|v| v.as_str())
                .unwrap_or("u+w");
            let non_recursive = step
                .get("non_recursive")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            for p in step
                .get("paths")
                .and_then(|p| p.as_array())
                .cloned()
                .unwrap_or_default()
            {
                for p in ctx.glob_paths(&p) {
                    if p.is_dir() && !non_recursive {
                        let mut stack = vec![p.clone()];
                        while let Some(d) = stack.pop() {
                            let _ = set_mode(&d, mode);
                            if let Ok(rd) = std::fs::read_dir(&d) {
                                for e in rd.flatten() {
                                    if e.path().is_dir() {
                                        stack.push(e.path());
                                    } else {
                                        let _ = set_mode(&e.path(), mode);
                                    }
                                }
                            }
                        }
                    } else {
                        let _ = set_mode(&p, mode);
                    }
                }
            }
            Ok(None)
        }
        "run" => {
            let cmd_path = ctx
                .path(step.get("command").unwrap_or(&Value::Null))
                .ok_or("no command")?;
            let mut cmd = Command::new(&cmd_path);
            let prefix = ctx.paths.prefix_str();
            for a in step
                .get("args")
                .and_then(|a| a.as_array())
                .cloned()
                .unwrap_or_default()
            {
                if let Some(s) = a.as_str() {
                    let arg = ctx.expand(s);
                    // Homebrew's Ruby post_install usually mkpath'd the target
                    // directory first; the declarative steps leave that out.
                    let candidate = arg.split_once('=').map(|(_, v)| v).unwrap_or(&arg);
                    if candidate.starts_with(&prefix) {
                        if let Some(parent) = Path::new(candidate).parent() {
                            if parent.starts_with(&ctx.paths.prefix) && !parent.exists() {
                                let _ = std::fs::create_dir_all(parent);
                            }
                        }
                    }
                    cmd.arg(arg);
                }
            }
            // ca-certificates rebuilds its bundle with one openssl call per
            // certificate (seconds). The result only depends on the bottle and
            // the system trust store, so keep it beside the store entry.
            let cache = ca_cert_cache(ctx, step);
            if let Some((cache_file, dest)) = &cache {
                if cache_file.is_file() {
                    if let Some(d) = dest.parent() {
                        std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
                    }
                    std::fs::copy(cache_file, dest).map_err(|e| e.to_string())?;
                    return Ok(None);
                }
            }
            if let Some(env) = step.get("env").and_then(|e| e.as_object()) {
                for (k, v) in env {
                    cmd.env(k, ctx.expand(v.as_str().unwrap_or("")));
                }
            }
            let path_env = format!(
                "{}:{}",
                ctx.paths.bin().display(),
                std::env::var("PATH").unwrap_or_default()
            );
            cmd.env("PATH", path_env);
            cmd.env("HOMEBREW_PREFIX", ctx.paths.prefix_str());
            let out = run_cmd(&mut cmd)?;
            if let Some((cache_file, dest)) = &cache {
                if dest.is_file() {
                    if let Some(d) = cache_file.parent() {
                        let _ = std::fs::create_dir_all(d);
                    }
                    let _ = std::fs::copy(dest, cache_file);
                }
            }
            if let Some(stdout_path) = step.get("stdout_path").and_then(|p| ctx.path(p)) {
                if let Some(d) = stdout_path.parent() {
                    std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
                }
                std::fs::write(&stdout_path, out).map_err(|e| e.to_string())?;
            }
            Ok(None)
        }
        "warn" => Ok(Some(
            ctx.expand(step.get("message").and_then(|m| m.as_str()).unwrap_or("")),
        )),
        "terminate_process" => {
            let name = step.get("name").and_then(|n| n.as_str()).unwrap_or("");
            if !name.is_empty() && ctx.platform.os != Os::Windows {
                let _ = Command::new("pkill").arg("-x").arg(name).output();
            }
            Ok(None)
        }
        "compile_gsettings_schemas" => {
            let p = ctx
                .path(step.get("path").unwrap_or(&Value::Null))
                .ok_or("no path")?;
            let tool = bin("glib-compile-schemas");
            if tool.exists() && p.is_dir() {
                run_cmd(Command::new(tool).arg(&p))?;
            }
            Ok(None)
        }
        "gtk_update_icon_cache" => {
            let p = ctx
                .path(step.get("path").unwrap_or(&Value::Null))
                .ok_or("no path")?;
            for t in [
                "gtk-update-icon-cache",
                "gtk4-update-icon-cache",
                "gtk3-update-icon-cache",
            ] {
                let tool = bin(t);
                if tool.exists() && p.is_dir() {
                    run_cmd(Command::new(tool).args(["-f", "-t"]).arg(&p))?;
                    break;
                }
            }
            Ok(None)
        }
        "update_mime_database" => {
            let p = ctx
                .path(step.get("path").unwrap_or(&Value::Null))
                .ok_or("no path")?;
            let tool = bin("update-mime-database");
            if tool.exists() && p.is_dir() {
                run_cmd(Command::new(tool).arg(&p))?;
            }
            Ok(None)
        }
        "update_desktop_database" => {
            let p = ctx
                .path(step.get("path").unwrap_or(&Value::Null))
                .ok_or("no path")?;
            let tool = bin("update-desktop-database");
            if tool.exists() && p.is_dir() {
                run_cmd(Command::new(tool).arg(&p))?;
            }
            Ok(None)
        }
        "gdk_pixbuf_query_loaders" => {
            let tool = bin("gdk-pixbuf-query-loaders");
            if tool.exists() {
                run_cmd(Command::new(tool).arg("--update-cache"))?;
            }
            Ok(None)
        }
        "install_gzipped_executable" => {
            let src = ctx
                .path(step.get("source").unwrap_or(&Value::Null))
                .ok_or("no source")?;
            let dst = ctx
                .path(step.get("target").unwrap_or(&Value::Null))
                .ok_or("no target")?;
            let data = std::fs::read(&src).map_err(|e| e.to_string())?;
            let mut out = Vec::new();
            std::io::Read::read_to_end(&mut flate2::read::MultiGzDecoder::new(&data[..]), &mut out)
                .map_err(|e| e.to_string())?;
            std::fs::write(&dst, out).map_err(|e| e.to_string())?;
            set_mode(&dst, "0755").map_err(|e| e.to_string())?;
            Ok(None)
        }
        "inreplace" => {
            let p = ctx
                .path(step.get("path").unwrap_or(&Value::Null))
                .ok_or("no path")?;
            let before = ctx.expand(step.get("before").and_then(|b| b.as_str()).unwrap_or(""));
            let after = ctx.expand(step.get("after").and_then(|a| a.as_str()).unwrap_or(""));
            let text = std::fs::read_to_string(&p).map_err(|e| e.to_string())?;
            let new = if step
                .get("regexp")
                .and_then(|r| r.as_bool())
                .unwrap_or(false)
            {
                let re = regex_lite::Regex::new(&before).map_err(|e| e.to_string())?;
                re.replace_all(&text, after.as_str()).into_owned()
            } else {
                text.replace(&before, &after)
            };
            if new != text {
                std::fs::write(&p, new).map_err(|e| e.to_string())?;
            }
            Ok(None)
        }
        "link_dir" => {
            let src = ctx
                .path(step.get("source").unwrap_or(&Value::Null))
                .ok_or("no source")?;
            let dst = ctx
                .path(step.get("target").unwrap_or(&Value::Null))
                .ok_or("no target")?;
            if src.is_dir() && !dst.exists() && !dst.is_symlink() {
                if let Some(d) = dst.parent() {
                    std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
                }
                #[cfg(unix)]
                std::os::unix::fs::symlink(&src, &dst).map_err(|e| e.to_string())?;
            }
            Ok(None)
        }
        "link_children" => {
            let src = ctx
                .path(step.get("source").unwrap_or(&Value::Null))
                .ok_or("no source")?;
            let dst = ctx
                .path(step.get("target").unwrap_or(&Value::Null))
                .ok_or("no target")?;
            let suffix = ctx.expand(step.get("suffix").and_then(|s| s.as_str()).unwrap_or(""));
            std::fs::create_dir_all(&dst).map_err(|e| e.to_string())?;
            for e in std::fs::read_dir(&src)
                .map_err(|e| e.to_string())?
                .flatten()
            {
                let name = format!("{}{}", e.file_name().to_string_lossy(), suffix);
                let link = dst.join(name);
                if !link.exists() && !link.is_symlink() {
                    #[cfg(unix)]
                    let _ = std::os::unix::fs::symlink(e.path(), &link);
                }
            }
            Ok(None)
        }
        "init_data_dir" => {
            let p = ctx
                .path(step.get("path").unwrap_or(&Value::Null))
                .ok_or("no path")?;
            if p.exists()
                && std::fs::read_dir(&p)
                    .map(|mut d| d.next().is_some())
                    .unwrap_or(false)
            {
                return Ok(None);
            }
            std::fs::create_dir_all(&p).map_err(|e| e.to_string())?;
            let using = step
                .get("using")
                .and_then(|u| u.as_str())
                .unwrap_or("initdb");
            let r = match using {
                "initdb" => run_cmd(
                    Command::new(bin("initdb"))
                        .arg("--locale=C")
                        .arg("-E")
                        .arg("UTF-8")
                        .arg(&p),
                ),
                "mariadb_install_db" => run_cmd(
                    Command::new(bin("mariadb-install-db"))
                        .arg(format!("--datadir={}", p.display()))
                        .arg("--auth-root-authentication-method=normal"),
                ),
                "mysqld" => run_cmd(
                    Command::new(bin("mysqld"))
                        .arg("--initialize-insecure")
                        .arg(format!("--datadir={}", p.display())),
                ),
                other => Err(format!("unknown initializer {other}")),
            };
            r.map(|_| None)
        }
        "configure_gcc_runtime" => {
            // lib/gcc/current -> lib/gcc/<major>, which every Linux rpath points at.
            let gcc_lib = ctx.keg.join("lib/gcc");
            let major = ctx.major();
            if gcc_lib.join(&major).is_dir() {
                let current = gcc_lib.join("current");
                if current.is_symlink() || current.exists() {
                    let _ = std::fs::remove_file(&current);
                }
                #[cfg(unix)]
                std::os::unix::fs::symlink(&major, &current).map_err(|e| e.to_string())?;
            }
            Ok(None)
        }
        "bootstrap_cpython" => {
            let site = ctx
                .paths
                .prefix
                .join("lib")
                .join(format!("python{}", ctx.major_minor()))
                .join("site-packages");
            std::fs::create_dir_all(&site).map_err(|e| e.to_string())?;
            let py = ctx
                .keg
                .join("bin")
                .join(format!("python{}", ctx.major_minor()));
            if py.exists() {
                let _ = Command::new(&py)
                    .args(["-m", "ensurepip", "--upgrade", "--default-pip"])
                    .output();
            }
            Ok(Some(format!("site-packages: {}", site.display())))
        }
        "change_dylib_id" => {
            let src = ctx
                .path(step.get("source").unwrap_or(&Value::Null))
                .ok_or("no source")?;
            let id = ctx.expand(step.get("id").and_then(|i| i.as_str()).unwrap_or(""));
            let target = if step
                .get("resolve_source")
                .and_then(|r| r.as_bool())
                .unwrap_or(false)
            {
                std::fs::canonicalize(&src).unwrap_or(src)
            } else {
                src
            };
            if Path::new("/usr/bin/install_name_tool").exists() {
                run_cmd(
                    Command::new("/usr/bin/install_name_tool")
                        .arg("-id")
                        .arg(&id)
                        .arg(&target),
                )?;
                let _ = crate::relocate::codesign(&[target]);
            }
            Ok(None)
        }
        "configure_glibc_runtime"
        | "configure_clang_system"
        | "configure_php"
        | "bootstrap_pypy" => Ok(Some(format!("{ty}: step not automated by Pour yet"))),
        other => Err(format!("unknown step type {other}")),
    }
}

/// For ca-certificates' bundle build: (cache file, destination) keyed by the
/// keg (the store entry the keg resolves to) and the system trust store's
/// modification time.
fn ca_cert_cache(ctx: &Ctx, step: &Value) -> Option<(PathBuf, PathBuf)> {
    if ctx.name != "ca-certificates" {
        return None;
    }
    let args = step.get("args")?.as_array()?;
    let dest = PathBuf::from(ctx.expand(args.get(1)?.as_str()?));
    let keg = std::fs::canonicalize(&ctx.keg).unwrap_or(ctx.keg.clone());
    let sys_mtime = [
        "/etc/ssl/certs",
        "/System/Library/Keychains/SystemRootCertificates.keychain",
        "/Library/Keychains/System.keychain",
        "/etc/pki/tls/certs",
    ]
    .iter()
    .filter_map(|p| std::fs::metadata(p).ok())
    .filter_map(|m| m.modified().ok())
    .filter_map(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
    .map(|d| d.as_secs())
    .max()
    .unwrap_or(0);
    Some((
        keg.join(".pour-cache")
            .join(format!("cert-{sys_mtime}.pem")),
        dest,
    ))
}

fn copy_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    if src.is_dir() {
        std::fs::create_dir_all(dst)?;
        for e in std::fs::read_dir(src)? {
            let e = e?;
            copy_recursive(&e.path(), &dst.join(e.file_name()))?;
        }
        Ok(())
    } else {
        std::fs::copy(src, dst).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::Arch;

    fn ctx(dir: &Path) -> (Paths, Platform) {
        let paths = Paths::new(dir.join("pour"));
        paths.ensure().unwrap();
        (
            paths,
            Platform {
                os: Os::Linux,
                arch: Arch::X86_64,
                macos_major: 0,
                wsl: false,
                ish: false,
            },
        )
    }

    #[test]
    fn templates_and_bases() {
        let dir = tempfile::tempdir().unwrap();
        let (paths, platform) = ctx(dir.path());
        let c = Ctx {
            paths: &paths,
            platform: &platform,
            name: "python@3.14".into(),
            version: "3.14.0_1".into(),
            keg: paths.keg("python@3.14", "3.14.0_1"),
        };
        assert_eq!(
            c.expand("{{HOMEBREW_PREFIX}}/lib/python{{version.major_minor}}"),
            format!("{}/lib/python3.14", paths.prefix_str())
        );
        assert_eq!(c.expand("{{version.major}}"), "3");
        assert_eq!(
            c.path(&serde_json::json!({"base": "var", "path": "log/x"}))
                .unwrap(),
            paths.var().join("log/x")
        );
        assert_eq!(
            c.path(&serde_json::json!({"path": "{{pkgetc}}/cert.pem"}))
                .unwrap(),
            paths.etc().join("python@3.14/cert.pem")
        );
        assert_eq!(parse_mode("0750", 0), 0o750);
        assert_eq!(parse_mode("u+w", 0o444), 0o644);
        assert_eq!(parse_mode("go-w", 0o777), 0o755);
    }

    #[test]
    fn runs_file_steps_with_guards() {
        let dir = tempfile::tempdir().unwrap();
        let (paths, platform) = ctx(dir.path());
        let keg = paths.keg("demo", "1.0");
        std::fs::create_dir_all(keg.join("libexec")).unwrap();
        std::fs::write(keg.join("libexec/default.conf"), "k=v\n").unwrap();
        let c = Ctx {
            paths: &paths,
            platform: &platform,
            name: "demo".into(),
            version: "1.0".into(),
            keg: keg.clone(),
        };
        let steps = serde_json::json!([
            {"type": "mkdir_p", "path": {"base": "var", "path": "demo"}},
            {"type": "copy", "source": {"base": "libexec", "path": "default.conf"}, "target": {"base": "var", "path": "demo/demo.conf"},
             "guards": [{"base": "var", "path": "demo/demo.conf", "condition": "unless_exists"}]},
            {"type": "touch", "path": {"path": "{{var}}/log/demo.log"}},
            {"type": "write", "path": {"base": "var", "path": "demo/note.txt"}, "content": "prefix = {{HOMEBREW_PREFIX}}\n", "overwrite": true},
            {"type": "inreplace", "path": {"base": "var", "path": "demo/demo.conf"}, "before": "k=v", "after": "k={{formula_name}}"},
            {"type": "warn", "message": "hello from {{formula_name}}", "guards": [{"condition": "on", "value": "linux"}]},
            {"type": "warn", "message": "mac only", "guards": [{"condition": "on", "value": "macos"}]},
            {"type": "set_permissions", "paths": [{"base": "var", "path": "demo"}], "permissions": "0700", "non_recursive": true},
            {"type": "bogus_step"}
        ])
        .to_string();
        let rep = run(&c, &steps);
        assert_eq!(rep.ran, 7, "{:?}", rep);
        assert_eq!(rep.skipped, vec!["warn (guard)"]);
        assert_eq!(rep.warnings.len(), 1);
        assert_eq!(rep.notes, vec!["hello from demo"]);
        assert_eq!(
            std::fs::read_to_string(paths.var().join("demo/demo.conf")).unwrap(),
            "k=demo\n"
        );
        assert!(paths.var().join("log/demo.log").exists());
        assert!(std::fs::read_to_string(paths.var().join("demo/note.txt"))
            .unwrap()
            .contains(&paths.prefix_str()));
        // second run: copy is guarded off, nothing breaks
        let rep2 = run(&c, &steps);
        assert!(rep2.skipped.contains(&"copy (guard)".to_string()));
    }
}
