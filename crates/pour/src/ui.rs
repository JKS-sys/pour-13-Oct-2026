//! Terminal output: colours, progress bars and the event reporter.

use console::{style, Term};
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use pour_core::install::{Event, Reporter};
use pour_core::net::Progress;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

pub struct Ui {
    pub quiet: bool,
    pub verbose: bool,
    pub json: bool,
    multi: MultiProgress,
    bars: Mutex<HashMap<String, ProgressBar>>,
    total: Mutex<Option<ProgressBar>>,
    phase_started: Mutex<Option<std::time::Instant>>,
    /// Something was poured (installed, upgraded) — the finishing chime plays once.
    poured: AtomicBool,
}

pub fn arrow() -> console::StyledObject<&'static str> {
    style("==>").blue().bold()
}

impl Ui {
    pub fn new(quiet: bool, verbose: bool, json: bool) -> Ui {
        let multi = MultiProgress::new();
        if quiet || json || !Term::stderr().is_term() {
            multi.set_draw_target(ProgressDrawTarget::hidden());
        }
        Ui {
            quiet,
            verbose,
            json,
            multi,
            bars: Mutex::new(HashMap::new()),
            total: Mutex::new(None),
            phase_started: Mutex::new(None),
            poured: AtomicBool::new(false),
        }
    }

    pub fn println(&self, s: impl AsRef<str>) {
        if self.quiet || self.json {
            return;
        }
        self.multi.suspend(|| println!("{}", s.as_ref()));
    }

    pub fn head(&self, s: impl AsRef<str>) {
        self.println(format!("{} {}", arrow(), style(s.as_ref()).bold()));
    }

    pub fn warn(&self, s: impl AsRef<str>) {
        if self.json {
            return;
        }
        self.multi
            .suspend(|| eprintln!("{} {}", style("Warning:").yellow().bold(), s.as_ref()));
    }

    pub fn error(&self, s: impl AsRef<str>) {
        self.multi
            .suspend(|| eprintln!("{} {}", style("Error:").red().bold(), s.as_ref()));
    }

    pub fn note(&self, s: impl AsRef<str>) {
        self.println(format!("  {} {}", style("•").dim(), s.as_ref()));
    }

    pub fn ok(&self, s: impl AsRef<str>) {
        self.println(format!("{} {}", style("✓").green().bold(), s.as_ref()));
    }

    pub fn finish(&self) {
        if let Some(t) = self.total.lock().unwrap().take() {
            t.finish_and_clear();
        }
    }

    /// The pour sound when a command poured something, the low one when it failed.
    /// Interactive terminals only (never in pipes, CI, --quiet or --json); POUR_SOUND=0 turns it off.
    pub fn chime_end(&self, ok: bool) {
        if ok && !self.poured.load(Ordering::Relaxed) {
            return;
        }
        if self.quiet || self.json || !Term::stderr().is_term() || !sound_on() {
            return;
        }
        chime(ok);
    }
}

fn sound_on() -> bool {
    if std::env::var_os("CI").is_some() {
        return false;
    }
    !matches!(
        std::env::var("POUR_SOUND")
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str(),
        "0" | "off" | "false" | "no"
    )
}

/// Play the system's own sound without waiting for it (the player outlives pour).
fn chime(ok: bool) {
    use std::process::{Command, Stdio};
    let spawn = |prog: &str, args: &[&str]| {
        Command::new(prog)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .is_ok()
    };
    if cfg!(target_os = "macos") {
        let f = if ok {
            "/System/Library/Sounds/Glass.aiff"
        } else {
            "/System/Library/Sounds/Basso.aiff"
        };
        if std::path::Path::new(f).exists() && spawn("afplay", &["-v", "0.6", f]) {
            return;
        }
    } else if cfg!(target_os = "windows") {
        let f = if ok {
            r"C:\Windows\Media\Windows Notify System Generic.wav"
        } else {
            r"C:\Windows\Media\Windows Critical Stop.wav"
        };
        if std::path::Path::new(f).exists() {
            let ps = format!("(New-Object Media.SoundPlayer '{f}').PlaySync()");
            if spawn(
                "powershell",
                &["-NoProfile", "-NonInteractive", "-Command", &ps],
            ) {
                return;
            }
        }
    } else {
        let f = if ok {
            "/usr/share/sounds/freedesktop/stereo/complete.oga"
        } else {
            "/usr/share/sounds/freedesktop/stereo/dialog-error.oga"
        };
        if std::path::Path::new(f).exists() && (spawn("paplay", &[f]) || spawn("pw-play", &[f])) {
            return;
        }
        if spawn(
            "canberra-gtk-play",
            &["-i", if ok { "complete" } else { "dialog-error" }],
        ) {
            return;
        }
    }
    eprint!("\x07");
}

impl Progress for Ui {
    fn start(&self, label: &str, total: Option<u64>) {
        if self.quiet || self.json {
            return;
        }
        let bar = match total {
            Some(t) => {
                let b = self.multi.add(ProgressBar::new(t));
                b.set_style(
                    ProgressStyle::with_template("  {prefix:<24.cyan} {bar:28.yellow/dim} {bytes:>9.green} / {total_bytes:<9} {bytes_per_sec:>11.blue}")
                        .unwrap()
                        .progress_chars("█▓░"),
                );
                b
            }
            None => {
                let b = self.multi.add(ProgressBar::new_spinner());
                b.set_style(
                    ProgressStyle::with_template(
                        "  {prefix:<24.cyan} {spinner:.yellow} {bytes:>9.green}",
                    )
                    .unwrap()
                    // a glass filling up, then poured out again
                    .tick_strings(&[
                        "▁", "▂", "▃", "▄", "▅", "▆", "▇", "█", "▇", "▆", "▅", "▄", "▃", "▂", "▁",
                    ]),
                );
                b.enable_steady_tick(Duration::from_millis(80));
                b
            }
        };
        bar.set_prefix(label.to_string());
        self.bars.lock().unwrap().insert(label.to_string(), bar);
    }

    fn advance(&self, label: &str, delta: u64) {
        if let Some(b) = self.bars.lock().unwrap().get(label) {
            b.inc(delta);
        }
    }

    fn finish(&self, label: &str) {
        if let Some(b) = self.bars.lock().unwrap().remove(label) {
            b.finish_and_clear();
            if !self.quiet && !self.json {
                self.multi
                    .suspend(|| println!("  {} {}", style("↓").green(), style(label).dim()));
            }
        }
    }
}

impl Reporter for Ui {
    fn event(&self, e: Event) {
        if self.json {
            return;
        }
        match e {
            Event::Phase(p) => {
                let mut started = self.phase_started.lock().unwrap();
                if let (true, Some(t)) = (self.verbose, *started) {
                    self.note(format!(
                        "previous phase took {}",
                        human_ms(t.elapsed().as_millis())
                    ));
                }
                *started = Some(std::time::Instant::now());
                self.head(p)
            }
            Event::Info(s) => self.println(s),
            Event::Warn(w) => self.warn(w),
            Event::Note(n) => self.note(n),
            Event::Done {
                name,
                version,
                action,
                ms,
            } => {
                if matches!(
                    action,
                    pour_core::resolve::Action::Install
                        | pour_core::resolve::Action::Reinstall
                        | pour_core::resolve::Action::Upgrade
                ) {
                    self.poured.store(true, Ordering::Relaxed);
                }
                let verb = match action {
                    pour_core::resolve::Action::Install => "poured",
                    pour_core::resolve::Action::Reinstall => "reinstalled",
                    pour_core::resolve::Action::Upgrade => "upgraded",
                    pour_core::resolve::Action::Keep => "kept",
                };
                self.println(format!(
                    "  {} {} {} {} {}",
                    style("🍶").yellow(),
                    style(&name).bold(),
                    style(&version).dim(),
                    style(verb).green(),
                    style(format!("{ms} ms")).dim()
                ));
            }
        }
    }
}

pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1000.0 && i < UNITS.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

pub fn human_ms(ms: u128) -> String {
    if ms < 1000 {
        format!("{ms} ms")
    } else {
        format!("{:.2} s", ms as f64 / 1000.0)
    }
}
