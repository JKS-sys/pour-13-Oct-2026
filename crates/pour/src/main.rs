//! `pour` — the Homebrew-compatible package manager that installs in
//! milliseconds. Copyright (c) 2026 Jagadeesh Kumar S. All rights reserved.

mod cmd;
mod paint;
mod ui;

use clap::{Args, CommandFactory, Parser, Subcommand};
use std::process::ExitCode;

/// Help in Pour's colours (no pink): headings gold, usage teal, commands and flags sky,
/// placeholders lime, errors red, suggestions green.
const STYLES: clap::builder::styling::Styles = {
    use clap::builder::styling::{Ansi256Color, Color, Style, Styles};
    const fn c(n: u8) -> Option<Color> {
        Some(Color::Ansi256(Ansi256Color(n)))
    }
    Styles::styled()
        .header(Style::new().bold().fg_color(c(179)))
        .usage(Style::new().bold().fg_color(c(43)))
        .literal(Style::new().bold().fg_color(c(39)))
        .placeholder(Style::new().fg_color(c(149)))
        .error(Style::new().bold().fg_color(c(203)))
        .valid(Style::new().bold().fg_color(c(78)))
        .invalid(Style::new().bold().fg_color(c(214)))
};

#[derive(Parser)]
#[command(styles = STYLES)]
#[command(name = "pour", version, about = "Pour — the Homebrew-compatible package manager that installs in milliseconds", long_about = None, disable_help_subcommand = true, arg_required_else_help = true)]
#[command(
    after_help = "Examples:\n  pour install ffmpeg wget          pour two formulae (and what they need)\n  pour install --cask firefox       install an app\n  pour search json                  find packages\n  pour upgrade                      upgrade everything that is outdated\n  pour bundle                       install from a Brewfile\n\nBy Jagadeesh Kumar S · https://ipconfig.co.network/pour"
)]
pub struct Cli {
    /// Say less (no progress bars)
    #[arg(short, long, global = true)]
    quiet: bool,
    /// Say more
    #[arg(short, long, global = true)]
    verbose: bool,
    /// Machine-readable JSON output where supported
    #[arg(long, global = true)]
    json: bool,
    /// Print Pour's prefix (optionally for one installed formula)
    #[arg(long, value_name = "FORMULA", num_args = 0..=1, default_missing_value = "")]
    prefix: Option<String>,
    /// Print the Cellar path
    #[arg(long)]
    cellar: bool,
    /// Print the download cache path
    #[arg(long)]
    cache: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Args, Clone, Default)]
pub struct KindFlags {
    /// Treat names as formulae only
    #[arg(long, conflicts_with = "cask")]
    pub formula: bool,
    /// Treat names as casks (apps) only
    #[arg(long)]
    pub cask: bool,
}

#[derive(Subcommand)]
pub enum Command {
    /// Install formulae or casks (and everything they depend on)
    #[command(visible_aliases = ["i", "add"])]
    Install {
        #[command(flatten)]
        kind: KindFlags,
        /// Reinstall even if already installed
        #[arg(long)]
        reinstall: bool,
        /// Install only the dependencies of the named formulae
        #[arg(long)]
        only_dependencies: bool,
        /// Do not run post-install steps
        #[arg(long)]
        skip_post_install: bool,
        /// Replace files in the prefix that something else owns
        #[arg(long)]
        overwrite: bool,
        /// Replace an app that is already present (casks)
        #[arg(long, short)]
        force: bool,
        /// Take over an app that is already in Applications (put there by Homebrew, the
        /// App Store or by hand) instead of downloading it again (casks)
        #[arg(long)]
        adopt: bool,
        /// Parallel downloads
        #[arg(short = 'j', long, default_value_t = 16)]
        jobs: usize,
        /// Show the plan and stop
        #[arg(long, short = 'n')]
        dry_run: bool,
        /// Package names (`name`, `tap/name`, `homebrew/cask/name`)
        #[arg(required = true)]
        names: Vec<String>,
    },
    /// Uninstall formulae or casks
    #[command(visible_aliases = ["remove", "rm", "un"])]
    Uninstall {
        #[command(flatten)]
        kind: KindFlags,
        /// Remove even when other installed formulae depend on it
        #[arg(long)]
        ignore_dependencies: bool,
        /// Casks: also remove the app's preferences, caches and support files
        #[arg(long)]
        zap: bool,
        #[arg(required = true)]
        names: Vec<String>,
    },
    /// Reinstall formulae or casks
    Reinstall {
        #[command(flatten)]
        kind: KindFlags,
        #[arg(required = true)]
        names: Vec<String>,
    },
    /// Upgrade outdated formulae and casks (all, or the named ones)
    #[command(visible_alias = "up")]
    Upgrade {
        #[command(flatten)]
        kind: KindFlags,
        /// Casks that update themselves are skipped unless --greedy
        #[arg(long)]
        greedy: bool,
        #[arg(long, short = 'n')]
        dry_run: bool,
        names: Vec<String>,
    },
    /// Fetch the newest index of formulae, casks and taps
    Update {
        /// Re-download even when nothing changed
        #[arg(long, short)]
        force: bool,
    },
    /// Show installed packages with a newer version in the index
    Outdated {
        #[command(flatten)]
        kind: KindFlags,
    },
    /// Search formulae and casks by name and description
    #[command(visible_alias = "s")]
    Search {
        #[command(flatten)]
        kind: KindFlags,
        /// Maximum number of results
        #[arg(long, default_value_t = 40)]
        limit: usize,
        /// Words to look for (all must match)
        #[arg(required = true)]
        query: Vec<String>,
    },
    /// Show everything about a formula or cask
    #[command(visible_alias = "show")]
    Info {
        #[command(flatten)]
        kind: KindFlags,
        #[arg(required = true)]
        names: Vec<String>,
    },
    /// List installed packages
    #[command(visible_alias = "ls")]
    List {
        #[command(flatten)]
        kind: KindFlags,
        /// Show versions
        #[arg(long, short = 'l')]
        versions: bool,
        /// One name per line, nothing else (what scripts read; also the default in pipes)
        #[arg(short = '1', long = "one")]
        one: bool,
        /// Also list what Homebrew installed on this machine (shown on a terminal by default)
        #[arg(long)]
        homebrew: bool,
    },
    /// Symlink an installed formula into the prefix
    Link {
        #[arg(long)]
        overwrite: bool,
        #[arg(required = true)]
        names: Vec<String>,
    },
    /// Remove a formula's symlinks from the prefix (it stays installed)
    Unlink {
        #[arg(required = true)]
        names: Vec<String>,
    },
    /// Keep a formula at its current version during upgrades
    Pin {
        #[arg(required = true)]
        names: Vec<String>,
    },
    /// Allow a pinned formula to upgrade again
    Unpin {
        #[arg(required = true)]
        names: Vec<String>,
    },
    /// Show the dependencies of a formula
    Deps {
        /// Draw the dependency tree
        #[arg(long)]
        tree: bool,
        /// Only dependencies that are installed
        #[arg(long)]
        installed: bool,
        #[arg(required = true)]
        name: String,
    },
    /// Show installed formulae that depend on a formula
    Uses {
        #[arg(required = true)]
        name: String,
    },
    /// Installed formulae nothing else depends on
    Leaves,
    /// Every app made at ipconfig.co.network, including apps the dashboard has just added
    Apps,
    /// Register an app (`pour install --cask NAME`) or a command-line tool (`pour install NAME`):
    /// checked automatically, listed for everyone at once when it qualifies — no waiting for approval
    Register {
        /// The id people install it by: 2–40 lowercase letters, digits, dashes, dots, plus or @
        app: String,
        /// A command-line tool (installed with `pour install NAME`) instead of an app (`--cask`)
        #[arg(long)]
        cli: bool,
        /// Display name (defaults to the id, capitalised)
        #[arg(long)]
        name: Option<String>,
        /// One line about it (10–200 characters)
        #[arg(long, alias = "line")]
        desc: Option<String>,
        /// Its web page (https)
        #[arg(long)]
        homepage: Option<String>,
        /// Its current version
        #[arg(long)]
        version: Option<String>,
        /// Release feed (latest.json with version + downloads per system); Pour follows it for every update
        #[arg(long)]
        feed: Option<String>,
        /// A download for one system: PLATFORM=URL#SHA256 (macos-arm64, macos-x86_64, linux-x86_64,
        /// linux-arm64, windows-x86_64, windows-arm64, freebsd-x86_64); repeat for each system
        #[arg(long = "url", value_name = "PLATFORM=URL#SHA256")]
        urls: Vec<String>,
        /// Program names a command-line tool puts on PATH (defaults to the id)
        #[arg(long = "bin")]
        bins: Vec<String>,
        /// The app bundle inside the download (defaults to "<Name>.app")
        #[arg(long = "app-bundle")]
        app_bundle: Option<String>,
        /// The key printed at the first registration (or POUR_REGISTER_KEY / APPS_API_KEY) — needed to update it
        #[arg(long)]
        key: Option<String>,
    },
    /// Run installed formulae in the background (launchd on macOS, systemd on Linux)
    Services {
        #[command(subcommand)]
        action: Option<ServicesAction>,
    },
    /// Check the installation for problems
    Doctor,
    /// Remove unused store entries and downloads
    Cleanup {
        /// Only report what would be removed
        #[arg(long, short = 'n')]
        dry_run: bool,
    },
    /// Manage extra registries ("taps")
    Tap {
        #[command(subcommand)]
        action: Option<TapAction>,
    },
    /// Install from a Brewfile, or dump installed packages to one
    Bundle {
        #[command(subcommand)]
        action: Option<BundleAction>,
    },
    /// Install everything Homebrew has installed on this machine
    Migrate {
        #[arg(long, short = 'n')]
        dry_run: bool,
    },
    /// Create the prefix (asks for sudo once) and the shell setup
    Init {
        /// Only print what `pour init` would do
        #[arg(long, short = 'n')]
        dry_run: bool,
    },
    /// Print shell lines that put Pour on PATH (eval "$(pour shellenv)")
    Shellenv {
        /// Shell flavour: sh, bash, zsh, fish, pwsh (default: detect)
        shell: Option<String>,
    },
    /// Show platform, prefix, index age and counts
    #[command(visible_alias = "config")]
    Env,
    /// Update Pour itself to the newest release
    #[command(name = "self-update")]
    SelfUpdate {
        /// Only check
        #[arg(long)]
        check: bool,
    },
    /// Generate shell completions
    Completions {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
    /// Maintainer tools: build or import an index
    #[command(hide = true)]
    Registry {
        #[command(subcommand)]
        action: RegistryAction,
    },
}

#[derive(Subcommand)]
pub enum TapAction {
    /// Add a registry: `pour tap add user/repo https://…/index.json`
    Add { name: String, url: String },
    /// Remove a registry
    Remove { name: String },
    /// List registries
    List,
}

#[derive(Subcommand)]
pub enum ServicesAction {
    /// Every installed formula that can run as a service, and its status (default)
    List,
    /// Start now and at every login
    Start {
        /// Formula names
        names: Vec<String>,
        /// Every service of an installed formula
        #[arg(long)]
        all: bool,
    },
    /// Stop now and at login
    Stop {
        names: Vec<String>,
        #[arg(long)]
        all: bool,
    },
    /// Stop, then start again
    Restart {
        names: Vec<String>,
        #[arg(long)]
        all: bool,
    },
    /// Start now only (not at the next login)
    Run {
        names: Vec<String>,
        #[arg(long)]
        all: bool,
    },
    /// Status, unit file and log of one service
    Info { name: String },
    /// Remove services whose formula is no longer installed
    Cleanup,
}

#[derive(Subcommand)]
pub enum BundleAction {
    /// Install everything a Brewfile lists (default)
    Install {
        #[arg(long, short, default_value = "Brewfile")]
        file: String,
        #[arg(long, short = 'n')]
        dry_run: bool,
    },
    /// Write a Brewfile of what is installed
    Dump {
        #[arg(long, short, default_value = "Brewfile")]
        file: String,
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand)]
pub enum RegistryAction {
    /// Build an index from Homebrew API JSON files
    Build {
        /// formula.json (array)
        #[arg(long)]
        formulae: Option<String>,
        /// cask.json (array)
        #[arg(long)]
        casks: Option<String>,
        /// Output file (gzip when it ends in .gz)
        #[arg(long, short)]
        out: String,
        /// Metadata key=value pairs
        #[arg(long = "meta", value_name = "KEY=VALUE")]
        meta: Vec<String>,
        /// Previous index to diff against (prints added/removed/updated)
        #[arg(long)]
        diff: Option<String>,
        /// Write the diff as JSON here
        #[arg(long)]
        diff_out: Option<String>,
        /// Also write a compact search list (name, kind, desc, version) for websites
        #[arg(long)]
        search_out: Option<String>,
    },
    /// Install an index file (or JSON registry) as the core index
    Import { file: String },
    /// Print index statistics
    Stats { file: Option<String> },
}

fn main() -> ExitCode {
    // `pour search x | head` must not panic when the pipe closes.
    #[cfg(unix)]
    unsafe {
        libc_sigpipe_default();
    }
    paint::init();
    let cli = Cli::parse();
    let ui = ui::Ui::new(cli.quiet, cli.verbose, cli.json);
    let result = cmd::run(&cli, &ui);
    ui.finish();
    match result {
        Ok(()) => {
            ui.chime_end(true);
            ExitCode::SUCCESS
        }
        Err(e) => {
            ui.error(e.to_string());
            ui.chime_end(false);
            ExitCode::FAILURE
        }
    }
}

pub fn print_completions(shell: clap_complete::Shell) {
    let mut cmd = Cli::command();
    clap_complete::generate(shell, &mut cmd, "pour", &mut std::io::stdout());
}

#[cfg(unix)]
unsafe fn libc_sigpipe_default() {
    extern "C" {
        fn signal(sig: i32, handler: usize) -> usize;
    }
    const SIGPIPE: i32 = 13;
    const SIG_DFL: usize = 0;
    signal(SIGPIPE, SIG_DFL);
}
