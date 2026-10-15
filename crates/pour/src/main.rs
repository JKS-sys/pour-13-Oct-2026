//! `pour` — the Homebrew-compatible package manager that installs in
//! milliseconds. Copyright (c) 2026 Jagadeesh Kumar S. All rights reserved.

mod cmd;
mod ui;

use clap::{Args, CommandFactory, Parser, Subcommand};
use std::process::ExitCode;

#[derive(Parser)]
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
    /// Introduce a new app to the dashboard (it stays pending until the owner switches it on;
    /// then it is `pour install --cask <app>` everywhere)
    Register {
        /// The app's id: 2–24 lowercase letters, digits or dashes
        app: String,
        /// Display name (defaults to the id, capitalised)
        #[arg(long)]
        name: Option<String>,
        /// One line about the app
        #[arg(long)]
        line: Option<String>,
        /// Its current version
        #[arg(long)]
        version: Option<String>,
        /// Where its release feed lives (defaults to https://ipconfig.co.network/updates/<app>/latest.json)
        #[arg(long)]
        feed: Option<String>,
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
