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
mod journal;
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

use cli::{Cli, Command, ConfigCommand, InitArgs, RecycleCommand, ReviewCommand, RunArgs};

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
        Command::Journal(args) => {
            let config = load(&resolve_config_path(cli.config.as_deref())?)?;
            let store = open_store(&config)?;
            journal::show(&store, args)
        }
        Command::Config { command } => match command {
            ConfigCommand::Check => cmd_config_check(cli.config.as_deref()),
            ConfigCommand::Init(args) => cmd_config_init(args),
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

/// Writes a starter config prefilled for this machine.
///
/// The example file in the repository is a reference: it documents every key
/// and points at paths like `/data/documents/inbox` that exist on nobody's
/// machine. This writes something that runs as-is, which is a different job.
///
/// Three deliberate differences from the example:
///
/// * State lives under the user's home, not `/var/lib`, so a first run needs
///   no root.
/// * `stability_wait_minutes = 0`. The example's 15 is right for a real
///   downloads folder and wrong for the first thing anyone does, which is to
///   drop a few files in a directory and immediately run the tool -- and be
///   told it found nothing.
/// * `dry_run = true`, as everywhere else.
fn cmd_config_init(args: &InitArgs) -> Result<u8> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set, so there is no sensible default path. Pass --path.")?;

    let path = args.path.clone().unwrap_or_else(|| home.join(".config/bowerbird/config.toml"));

    if path.exists() && !args.force {
        bail!("{} already exists. Pass --force to overwrite it.", path.display());
    }

    let state = home.join(".local/share/bowerbird");
    let contents = starter_config(&home, &state);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("could not create {}", parent.display()))?;
    }
    std::fs::write(&path, contents)
        .with_context(|| format!("could not write {}", path.display()))?;

    println!("wrote {}", path.display());
    println!();
    println!("next:");
    println!("  1. point `endpoint` at your model server (or set api_key_env for a cloud one)");
    println!("  2. check the `path` on the [[profiles]] entry is the directory you mean");
    println!("  3. bower config check");
    println!("  4. bower run --dry-run");
    println!();
    println!("dry_run = true is set, so nothing moves until you pass --execute.");
    Ok(exit::OK)
}

fn starter_config(home: &Path, state: &Path) -> String {
    let downloads = home.join("Downloads");
    format!(
        r#"# Bowerbird configuration, generated by `bower config init`.
#
# Every key is documented in bowerbird.example.toml. Unknown keys are a hard
# error, not a warning: a typo in the file that governs where your files end up
# should not be something you discover later.

config_version = 1

[general]
# Nothing moves while this is true. `bower run --execute` overrides it.
dry_run = true

state_path = "{state}/state.db"
lock_file_dir = "{state}/locks"

log_level = "info"
default_batch_size = 25
default_confidence_threshold = 0.75

# Items awaiting a decision stay where they are; `bower review` lists them.
review_placement = "in_place"

# Where a file goes when its destination is occupied. Required because the
# profile below sets on_conflict = "quarantine". Nothing here is deleted.
quarantine_dir = "{state}/quarantine"

[[llm_backends]]
name = "local"
provider = "openai_compatible"
endpoint = "http://localhost:8080/v1"
# Name of an environment variable, never the key itself. Leave empty for a
# local server that wants no authentication.
api_key_env = ""
model = "llama-3.1-8b-instruct"
timeout_secs = 30
max_retries = 2

# "prompt" is the weakest and the most compatible: an endpoint that does not
# recognise `response_format` usually rejects the whole request rather than
# ignoring the field. Raise it to "json_object" or "json_schema" once you know
# your server supports it.
structured_output = "prompt"

[[profiles]]
name = "downloads"
path = "{downloads}"
description = "General downloads folder. Mixed file types, no fixed structure expected."
enabled = true
llm_backend = "local"
categories = ["Documents", "Images", "Installers", "Archives", "Media"]
allow_dynamic_categories = true
allow_delete_suggestions = false
confidence_threshold = 0.75
on_conflict = "quarantine"

# The example config uses 15, which is right for a real downloads folder and
# wrong for trying this out: a file you just created would be reported as still
# settling. Raise it once you are past the first run.
stability_wait_minutes = 0

exclude_patterns = ["*.part", "*.crdownload", ".DS_Store"]
include_subdirs = false

[profiles.rename]
enabled = false

[profiles.metadata]
detect_mime = true
extract_exif = false
extract_audio_tags = false
extract_pdf_metadata = false
# Bytes of file content sent to the model. 0 discloses none. Raise it and the
# file's own bytes reach the prompt, which is what makes classification good
# and is also the prompt-injection surface -- the policy engine is what makes
# acting on the result safe either way.
content_sniff_bytes = 0
"#,
        state = state.display(),
        downloads = downloads.display(),
    )
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
