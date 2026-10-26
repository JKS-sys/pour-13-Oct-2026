//! Terminal output: colours, progress bars and the event reporter.

use crate::paint::{self, Hue};
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
    /// The sound this command ends with when it succeeds (set by the command).
    cue: Mutex<Option<Cue>>,
    /// A spinner for one long step (services, update).
    spinner: Mutex<Option<ProgressBar>>,
}

/// What a command did, for its closing sound. Each has its own sound on macOS,
/// Windows and Linux desktops; nothing plays in pipes, CI, --quiet or --json.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cue {
    Poured,
    Upgraded,
    Removed,
    Updated,
    Found,
    Cleaned,
    Registered,
    ServiceUp,
    ServiceDown,
}

pub fn arrow() -> String {
    paint::b(Hue::Sky, "==>")
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
            cue: Mutex::new(None),
            spinner: Mutex::new(None),
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

    /// A heading in the i-th section colour.
    pub fn section(&self, s: impl AsRef<str>, i: usize) {
        self.println(paint::heading(s.as_ref(), i));
    }

    /// A spinner while one step runs: a glass filling and emptying in every colour but pink.
    pub fn spin_start(&self, msg: impl Into<String>) {
        if self.quiet || self.json || !Term::stderr().is_term() {
            return;
        }
        let b = self.multi.add(ProgressBar::new_spinner());
        b.set_style(
            ProgressStyle::with_template("  {spinner} {msg:.43.bold}")
                .unwrap()
                .tick_strings(&[
                    "\x1b[38;5;179m▁\x1b[0m",
                    "\x1b[38;5;214m▂\x1b[0m",
                    "\x1b[38;5;209m▃\x1b[0m",
                    "\x1b[38;5;149m▄\x1b[0m",
                    "\x1b[38;5;78m▅\x1b[0m",
                    "\x1b[38;5;43m▆\x1b[0m",
                    "\x1b[38;5;45m▇\x1b[0m",
                    "\x1b[38;5;39m█\x1b[0m",
                    "\x1b[38;5;75m▇\x1b[0m",
                    "\x1b[38;5;105m▆\x1b[0m",
                    "\x1b[38;5;141m▅\x1b[0m",
                    "\x1b[38;5;179m▄\x1b[0m",
                    "",
                ]),
        );
        b.set_message(msg.into());
        b.enable_steady_tick(Duration::from_millis(70));
        *self.spinner.lock().unwrap() = Some(b);
    }

    pub fn spin_stop(&self) {
        if let Some(b) = self.spinner.lock().unwrap().take() {
            b.finish_and_clear();
        }
    }

    /// A card with a gold frame for the end of a big step: a title and coloured lines.
    /// Plain lines in pipes, CI logs keep the frame (they render it fine).
    pub fn card(&self, title: &str, lines: &[String]) {
        if self.quiet || self.json {
            return;
        }
        let vis = |t: &str| console::measure_text_width(t);
        let w = lines
            .iter()
            .map(|l| vis(l))
            .chain(std::iter::once(vis(title) + 2))
            .max()
            .unwrap_or(20)
            .min(96)
            + 2;
        let gold = |t: &str| paint::c(Hue::Gold, t);
        self.println(format!(
            "{}{} {} {}{}",
            gold("╭"),
            gold("─"),
            paint::b(Hue::Gold, title),
            gold(&"─".repeat(w.saturating_sub(vis(title) + 3))),
            gold("╮")
        ));
        for l in lines {
            self.println(format!(
                "{} {}{} {}",
                gold("│"),
                l,
                " ".repeat(w.saturating_sub(vis(l) + 2)),
                gold("│")
            ));
        }
        self.println(format!(
            "{}{}{}",
            gold("╰"),
            gold(&"─".repeat(w)),
            gold("╯")
        ));
    }

    /// Choose the closing sound for a successful command.
    pub fn cue(&self, c: Cue) {
        *self.cue.lock().unwrap() = Some(c);
    }

    /// A short pouring animation on a terminal (≈0.4 s): a bottle tips, the glass
    /// fills in gold, then the line settles into `done`. Plain line elsewhere.
    pub fn pour_animation(&self, done: impl AsRef<str>) {
        if self.quiet || self.json {
            return;
        }
        let interactive = Term::stdout().is_term() && std::env::var_os("CI").is_none();
        if interactive && !reduced_motion() {
            use std::io::Write;
            let frames = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
            for (i, f) in frames.iter().enumerate() {
                let stream = paint::c(Hue::Amber, "╲".repeat(1 + i % 3));
                let glass = paint::b(paint::DEPTH[i % paint::DEPTH.len()], *f);
                self.multi.suspend(|| {
                    print!("\r  🍶 {stream:<3} {glass} ");
                    let _ = std::io::stdout().flush();
                });
                std::thread::sleep(Duration::from_millis(45));
            }
            // a short burst of colour sparks before the line settles
            let sparks = ["·", "✦", "✧", "⋆", "✦", "·"];
            for k in 0..6 {
                let line: String = (0..14)
                    .map(|i| {
                        paint::b(
                            paint::DEPTH[(i + k) % paint::DEPTH.len()],
                            sparks[(i + k) % sparks.len()],
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                self.multi.suspend(|| {
                    print!("\r  {line}");
                    let _ = std::io::stdout().flush();
                });
                std::thread::sleep(Duration::from_millis(40));
            }
            self.multi.suspend(|| {
                print!("\r\x1b[2K");
                let _ = std::io::stdout().flush();
            });
        }
        self.println(format!("{} {}", paint::b(Hue::Gold, "🥂"), done.as_ref()));
    }

    pub fn warn(&self, s: impl AsRef<str>) {
        if self.json {
            return;
        }
        self.multi
            .suspend(|| eprintln!("{} {}", paint::b(Hue::Amber, "Warning:"), s.as_ref()));
    }

    pub fn error(&self, s: impl AsRef<str>) {
        self.multi
            .suspend(|| eprintln!("{} {}", paint::b(Hue::Red, "Error:"), s.as_ref()));
    }

    pub fn note(&self, s: impl AsRef<str>) {
        self.println(format!("  {} {}", paint::c(Hue::Violet, "•"), s.as_ref()));
    }

    pub fn ok(&self, s: impl AsRef<str>) {
        self.println(format!("{} {}", paint::b(Hue::Green, "✓"), s.as_ref()));
    }

    pub fn finish(&self) {
        if let Some(t) = self.total.lock().unwrap().take() {
            t.finish_and_clear();
        }
    }

    /// The pour sound when a command poured something, the low one when it failed.
    /// Interactive terminals only (never in pipes, CI, --quiet or --json); POUR_SOUND=0 turns it off.
    pub fn chime_end(&self, ok: bool) {
        let cue = *self.cue.lock().unwrap();
        let cue = if self.poured.load(Ordering::Relaxed) {
            cue.or(Some(Cue::Poured))
        } else {
            cue
        };
        if ok && cue.is_none() {
            return;
        }
        if self.quiet || self.json || !Term::stderr().is_term() || !sound_on() {
            return;
        }
        chime(if ok { cue } else { None });
    }
}

/// Reduced motion: POUR_ANIMATIONS=0 (or off), or the accessibility setting
/// exported by some terminals (REDUCE_MOTION=1).
fn reduced_motion() -> bool {
    let off = |k: &str| {
        matches!(
            std::env::var(k)
                .unwrap_or_default()
                .to_ascii_lowercase()
                .as_str(),
            "0" | "off" | "false" | "no"
        )
    };
    off("POUR_ANIMATIONS") || std::env::var("REDUCE_MOTION").is_ok_and(|v| v == "1")
}

/// The system sound for each cue: (macOS, freedesktop name, Windows media file).
fn sound_for(c: Option<Cue>) -> (&'static str, &'static str, &'static str) {
    match c {
        Some(Cue::Poured) => ("Glass", "complete", "Windows Notify System Generic"),
        Some(Cue::Upgraded) => ("Hero", "service-login", "Windows Unlock"),
        Some(Cue::Removed) => ("Bottle", "trash-empty", "Windows Recycle"),
        Some(Cue::Updated) => ("Pop", "message-new-instant", "Windows Notify Messaging"),
        Some(Cue::Found) => ("Tink", "bell", "Windows Ding"),
        Some(Cue::Cleaned) => ("Purr", "trash-empty", "Windows Recycle"),
        Some(Cue::Registered) => ("Submarine", "dialog-information", "Windows Notify Calendar"),
        Some(Cue::ServiceUp) => ("Ping", "service-login", "Windows Hardware Insert"),
        Some(Cue::ServiceDown) => ("Blow", "service-logout", "Windows Hardware Remove"),
        None => ("Basso", "dialog-error", "Windows Critical Stop"),
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
fn chime(cue: Option<Cue>) {
    use std::process::{Command, Stdio};
    let (mac, fd, win) = sound_for(cue);
    let soft = matches!(cue, Some(Cue::Found));
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
        let f = format!("/System/Library/Sounds/{mac}.aiff");
        let vol = if soft { "0.3" } else { "0.6" };
        if std::path::Path::new(&f).exists() && spawn("afplay", &["-v", vol, &f]) {
            return;
        }
    } else if cfg!(target_os = "windows") {
        let f = format!(r"C:\Windows\Media\{win}.wav");
        if std::path::Path::new(&f).exists() {
            let ps = format!("(New-Object Media.SoundPlayer '{f}').PlaySync()");
            if spawn(
                "powershell",
                &["-NoProfile", "-NonInteractive", "-Command", &ps],
            ) {
                return;
            }
        }
    } else {
        let f = format!("/usr/share/sounds/freedesktop/stereo/{fd}.oga");
        if std::path::Path::new(&f).exists() && (spawn("paplay", &[&f]) || spawn("pw-play", &[&f]))
        {
            return;
        }
        if spawn("canberra-gtk-play", &["-i", fd]) {
            return;
        }
    }
    if !soft {
        eprint!("\x07");
    }
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
                    ProgressStyle::with_template("  {prefix:<24.43.bold} {bar:28.179/238} {bytes:>9.78} / {total_bytes:<9.247} {bytes_per_sec:>11.39} {eta:>4.141}")
                        .unwrap()
                        .progress_chars("█▓░"),
                );
                b
            }
            None => {
                let b = self.multi.add(ProgressBar::new_spinner());
                b.set_style(
                    ProgressStyle::with_template(
                        "  {prefix:<24.43.bold} {spinner:.179} {bytes:>9.78}",
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
                self.multi.suspend(|| {
                    println!(
                        "  {} {}",
                        paint::b(Hue::Green, "↓"),
                        paint::c(Hue::Teal, label)
                    )
                });
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
                let verb_hue = match action {
                    pour_core::resolve::Action::Install => Hue::Green,
                    pour_core::resolve::Action::Reinstall => Hue::Sky,
                    pour_core::resolve::Action::Upgrade => Hue::Lime,
                    pour_core::resolve::Action::Keep => Hue::Slate,
                };
                self.println(format!(
                    "  {} {} {} {} {}",
                    "🍶",
                    paint::formula(&name),
                    paint::version(&version),
                    paint::b(verb_hue, verb),
                    paint::c(Hue::Indigo, format!("{ms} ms"))
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
