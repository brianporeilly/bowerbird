//! Command-line surface.

use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

/// LLM-assisted file organization. The model proposes; a deterministic policy
/// engine decides; a separate executor is the only thing that touches disk.
#[derive(Debug, Parser)]
#[command(name = "bower", version, about, long_about = None)]
pub(crate) struct Cli {
    /// Path to the config file.
    #[arg(long, short = 'c', global = true, env = "BOWER_CONFIG")]
    pub(crate) config: Option<PathBuf>,

    /// Print more detail. Repeat for more.
    #[arg(long, short = 'v', global = true, action = clap::ArgAction::Count)]
    pub(crate) verbose: u8,

    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Scan, classify, and organize.
    Run(RunArgs),
    /// Show what has actually been done, from the journal.
    Journal(JournalArgs),
    /// Inspect the configuration without running anything.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Act on items waiting for a human decision.
    Review {
        #[command(subcommand)]
        command: ReviewCommand,
    },
    /// Inspect and restore files moved to the recycle store.
    Recycle {
        #[command(subcommand)]
        command: RecycleCommand,
    },
}

#[derive(Debug, Args)]
pub(crate) struct RunArgs {
    /// Run this profile. Repeatable.
    #[arg(long, short = 'p')]
    pub(crate) profile: Vec<String>,

    /// Run every enabled profile.
    #[arg(long, conflicts_with = "profile")]
    pub(crate) all: bool,

    /// Report what would happen without writing anything. Overrides the config.
    #[arg(long, conflicts_with = "execute")]
    pub(crate) dry_run: bool,

    /// Actually move files, overriding `general.dry_run = true`.
    #[arg(long)]
    pub(crate) execute: bool,

    /// Use the built-in offline classifier instead of a real backend. Its
    /// proposals are deterministic, so the printed plan is reproducible.
    #[arg(long)]
    pub(crate) stub_llm: bool,
}

#[derive(Debug, Subcommand)]
pub(crate) enum ConfigCommand {
    /// Validate the config and print the profiles it defines.
    Check,
    /// Write a starter config, prefilled for this machine.
    Init(InitArgs),
}

#[derive(Debug, Subcommand)]
pub(crate) enum ReviewCommand {
    /// Show everything waiting on a decision.
    List(ReviewListArgs),
    /// Show one item in full, including the model's reasoning.
    Show { id: i64 },
    /// Carry out a queued decision.
    Approve(ApproveArgs),
    /// Refuse a queued decision, and remember the refusal.
    Reject(RejectArgs),
}

#[derive(Debug, Args)]
pub(crate) struct ReviewListArgs {
    #[arg(long, short = 'p')]
    pub(crate) profile: Option<String>,
    /// Restrict to one kind of pending decision.
    #[arg(long = "type", value_enum)]
    pub(crate) kind: Option<ItemType>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum ItemType {
    /// The engine would not decide on its own.
    Review,
    /// A deletion suggestion.
    Delete,
    /// Parked because the destination was occupied.
    Quarantine,
}

#[derive(Debug, Args)]
pub(crate) struct ApproveArgs {
    /// The item to approve. Omit with --all.
    pub(crate) id: Option<i64>,
    /// Approve every pending item, optionally narrowed to one profile.
    #[arg(long, conflicts_with = "id")]
    pub(crate) all: bool,
    #[arg(long, short = 'p')]
    pub(crate) profile: Option<String>,
    /// Skip the confirmation prompt. Required for --all when not on a terminal.
    #[arg(long, short = 'y')]
    pub(crate) yes: bool,
    /// Report what would happen without writing anything.
    #[arg(long)]
    pub(crate) dry_run: bool,
}

#[derive(Debug, Args)]
pub(crate) struct RejectArgs {
    pub(crate) id: i64,
    /// Recorded alongside the refusal, for your own future reference.
    #[arg(long)]
    pub(crate) reason: Option<String>,
}

#[derive(Debug, Subcommand)]
pub(crate) enum RecycleCommand {
    /// Show everything in the recycle store.
    List,
    /// Move a recycled file back where it came from.
    Restore { id: i64 },
    /// Permanently delete recycled files. The only command that destroys
    /// anything.
    Purge(PurgeArgs),
}

#[derive(Debug, Args)]
pub(crate) struct PurgeArgs {
    /// Only purge items recycled longer ago than this, e.g. 30d, 2w, 12h.
    #[arg(long = "older-than", value_name = "DURATION")]
    pub(crate) older_than: String,
    /// List what would be destroyed without destroying it.
    #[arg(long)]
    pub(crate) dry_run: bool,
    /// Skip the confirmation prompt.
    #[arg(long, short = 'y')]
    pub(crate) yes: bool,
}

#[derive(Debug, clap::Args)]
pub(crate) struct JournalArgs {
    /// Restrict to one profile.
    #[arg(long)]
    pub(crate) profile: Option<String>,
    /// How many rows to show, newest first.
    #[arg(long, default_value_t = 20)]
    pub(crate) limit: usize,
    /// Show only operations that failed, and intents with no result -- the
    /// signature of a crash partway through an operation.
    #[arg(long)]
    pub(crate) failed: bool,
    /// Only rows newer than this, e.g. 30d, 2w, 12h.
    #[arg(long)]
    pub(crate) since: Option<String>,
}

#[derive(Debug, clap::Args)]
pub(crate) struct InitArgs {
    /// Where to write it. Defaults to ~/.config/bowerbird/config.toml.
    #[arg(long)]
    pub(crate) path: Option<std::path::PathBuf>,
    /// Overwrite an existing file.
    #[arg(long)]
    pub(crate) force: bool,
}
