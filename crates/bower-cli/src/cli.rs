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
}

#[derive(Debug, Subcommand)]
pub(crate) enum ReviewCommand {
    List,
    Show { id: String },
    Approve { id: String },
    Reject { id: String },
}

#[derive(Debug, Subcommand)]
pub(crate) enum RecycleCommand {
    List,
    Restore { id: String },
    Purge,
}
