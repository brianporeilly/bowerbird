#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::duration_suboptimal_units
    )
)]
//! `bower` -- the Bowerbird command-line interface.

mod cli;
mod report;
mod triage;

use anyhow::{Context, Result, bail};
use bower_config::{Config, Profile, Rename};
use bower_core::exec::Mode;
use bower_core::llm::LlmBackend;
use bower_core::lock::{LockError, ProfileLock};
use bower_core::policy;
use bower_core::run::{RunOptions, run_profile};
use bower_core::scan::ScanOptions;
use bower_core::state::Store;
use clap::Parser;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use cli::{Cli, Command, ConfigCommand, RecycleCommand, ReviewCommand, RunArgs};

/// Exit codes, chosen so an unattended cron job can act on them.
mod exit {
    /// Clean run, nothing pending.
    pub(crate) const OK: u8 = 0;
    /// Hard error: bad config, unreachable backend, filesystem failure.
    pub(crate) const ERROR: u8 = 1;
    /// Succeeded, but items are waiting on a human.
    pub(crate) const ATTENTION: u8 = 2;
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    match dispatch(&cli) {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(exit::ERROR)
        }
    }
}

fn dispatch(cli: &Cli) -> Result<u8> {
    match &cli.command {
        Command::Run(args) => cmd_run(cli.config.as_deref(), args),
        Command::Config { command } => match command {
            ConfigCommand::Check => cmd_config_check(cli.config.as_deref()),
        },
        Command::Review { command } => {
            let config = load(&resolve_config_path(cli.config.as_deref())?)?;
            let store = open_store(&config)?;
            match command {
                ReviewCommand::List(args) => triage::list(&store, args),
                ReviewCommand::Show { id } => triage::show(&store, *id),
                ReviewCommand::Approve(args) => triage::approve(&store, &config, args),
                ReviewCommand::Reject(args) => triage::reject(&store, &config, args),
            }
        }
        Command::Recycle { command } => {
            let config = load(&resolve_config_path(cli.config.as_deref())?)?;
            let store = open_store(&config)?;
            match command {
                RecycleCommand::List => triage::recycle_list(&store),
                RecycleCommand::Restore { id } => triage::recycle_restore(&store, &config, *id),
                RecycleCommand::Purge(args) => triage::recycle_purge(&store, &config, args),
            }
        }
    }
}

fn cmd_config_check(explicit: Option<&Path>) -> Result<u8> {
    let path = resolve_config_path(explicit)?;
    let config = load(&path)?;
    check_templates(&config)?;

    println!("{} is valid", path.display());
    println!(
        "  dry_run = {}, review_placement = {:?}",
        config.general.dry_run, config.general.review_placement
    );
    println!("  {} backend(s), {} profile(s)", config.backends.len(), config.profiles.len());
    for p in &config.profiles {
        let state = if p.enabled { "enabled" } else { "disabled" };
        let placement = if p.is_in_place() {
            "in place".to_owned()
        } else {
            format!("-> {}", p.destination_root.display())
        };
        println!(
            "  - {} ({state}, backend {}, {}, threshold {:.2}, {})",
            p.name,
            p.llm_backend,
            p.path.display(),
            p.confidence_threshold,
            placement,
        );
    }
    Ok(exit::OK)
}

fn cmd_run(explicit: Option<&Path>, args: &RunArgs) -> Result<u8> {
    let path = resolve_config_path(explicit)?;
    let config = load(&path)?;
    check_templates(&config)?;

    let selected = select_profiles(&config, args)?;
    let dry_run = resolve_dry_run(&config, args);
    let store = open_store(&config)?;
    let options = RunOptions {
        mode: Mode::from_dry_run(dry_run),
        review_placement: config.general.review_placement,
        quarantine_dir: config.general.quarantine_dir.clone(),
        scan: ScanOptions {
            // The quarantine and recycle stores are this tool's own output. If
            // either happens to sit inside a scanned directory it must never be
            // read back in as fresh input.
            extra_excluded_roots: [&config.general.quarantine_dir, &config.general.recycle_dir]
                .into_iter()
                .flatten()
                .cloned()
                .collect(),
        },
    };

    let mut needs_attention = false;
    let mut ran = 0usize;

    for profile in selected {
        let backend = backend_for(&config, profile, args)?;

        // A dry run writes nothing, so it does not contend for the lock -- and
        // taking one would mean a scheduled run could not be previewed while it
        // was in progress.
        let _lock = if dry_run {
            None
        } else {
            match ProfileLock::acquire(&config.general.lock_file_dir, &profile.name) {
                Ok(lock) => Some(lock),
                // `--all` skips a busy profile so one long run cannot stall the
                // batch; an explicitly named profile fails loudly, because the
                // user asked for that one specifically.
                Err(e @ LockError::Held { .. }) if args.all => {
                    eprintln!("skipping: {e}");
                    continue;
                }
                Err(e) => return Err(e).context("could not take the profile lock"),
            }
        };

        let report = run_profile(profile, backend.as_ref(), &options, &store)
            .with_context(|| format!("profile `{}` failed", profile.name))?;
        report::print_run(&report, dry_run);
        needs_attention |= report.needs_attention();
        ran += 1;
    }

    if ran == 0 {
        println!("no profiles ran");
    }
    Ok(if needs_attention { exit::ATTENTION } else { exit::OK })
}

fn backend_for(config: &Config, profile: &Profile, args: &RunArgs) -> Result<Box<dyn LlmBackend>> {
    if args.stub_llm {
        return Ok(Box::new(bower_llm::StubBackend::new()));
    }
    let backend = config
        .backend_for(profile)
        .with_context(|| format!("profile `{}` names an unknown backend", profile.name))?;
    bower_llm::build(backend).with_context(|| format!("could not build backend `{}`", backend.name))
}

/// Implements the no-flag rule from ADR-0001: with exactly one profile defined,
/// run it; with more than one, refuse and say what the options are, rather than
/// picking one or silently running them all.
fn select_profiles<'a>(config: &'a Config, args: &RunArgs) -> Result<Vec<&'a Profile>> {
    if args.all {
        let enabled: Vec<_> = config.enabled_profiles().collect();
        if enabled.is_empty() {
            bail!("--all was given but no profile is enabled");
        }
        return Ok(enabled);
    }

    if !args.profile.is_empty() {
        let mut out = Vec::with_capacity(args.profile.len());
        for name in &args.profile {
            let profile = config
                .profile(name)
                .with_context(|| format!("no profile named `{name}`; {}", known(config)))?;
            if !profile.enabled {
                bail!("profile `{name}` is disabled; set enabled = true to run it");
            }
            out.push(profile);
        }
        return Ok(out);
    }

    let enabled: Vec<_> = config.enabled_profiles().collect();
    match enabled.len() {
        0 => bail!("no profile is enabled"),
        1 => Ok(enabled),
        _ => bail!(
            "more than one profile is enabled, so there is no obvious default.\n\
             Pass --profile NAME (repeatable) or --all. {}",
            known(config)
        ),
    }
}

fn known(config: &Config) -> String {
    let names: Vec<_> = config.profiles.iter().map(|p| p.name.as_str()).collect();
    format!("Known profiles: {}", names.join(", "))
}

/// `--dry-run` and `--execute` both override the config; `--execute` is the only
/// way to write when the config says `dry_run = true`.
fn resolve_dry_run(config: &Config, args: &RunArgs) -> bool {
    if args.dry_run {
        return true;
    }
    if args.execute {
        return false;
    }
    config.general.dry_run
}

/// Reports a broken filename template once, at startup, rather than once per
/// file.
fn check_templates(config: &Config) -> Result<()> {
    for profile in &config.profiles {
        if let Rename::Enabled { template } = &profile.rename {
            policy::validate_template(template).with_context(|| {
                format!("profiles[{}].rename.template is invalid", profile.name)
            })?;
        }
    }
    Ok(())
}

/// Opens the state store, which holds the journal, the review queue,
/// remembered rejections, and the recycle index.
fn open_store(config: &Config) -> Result<Store> {
    Store::open(&config.general.state_path).with_context(|| {
        format!("could not open the state store at {}", config.general.state_path.display())
    })
}

fn load(path: &Path) -> Result<Config> {
    match Config::load(path) {
        Ok(c) => Ok(c),
        Err(e) => {
            if let Some(report) = e.problem_report() {
                bail!("{} is invalid:\n{report}", path.display());
            }
            Err(e).with_context(|| format!("could not load {}", path.display()))
        }
    }
}

/// Search order: `--config`, then the working directory, then the user's config
/// directory, then the system one.
fn resolve_config_path(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        if p.is_file() {
            return Ok(p.to_path_buf());
        }
        bail!("no config file at {}", p.display());
    }

    let mut candidates = vec![PathBuf::from("bowerbird.toml")];
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(Path::new(&home).join(".config/bowerbird/config.toml"));
    }
    candidates.push(PathBuf::from("/etc/bowerbird/config.toml"));

    candidates.iter().find(|p| p.is_file()).cloned().with_context(|| {
        let list =
            candidates.iter().map(|p| format!("  {}", p.display())).collect::<Vec<_>>().join("\n");
        format!("no config file found. Looked for:\n{list}\nPass --config PATH.")
    })
}

fn init_tracing(verbose: u8) {
    let default = match verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    let filter = tracing_subscriber::EnvFilter::try_from_env("BOWER_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default));
    tracing_subscriber::fmt().with_env_filter(filter).with_target(false).init();
}
