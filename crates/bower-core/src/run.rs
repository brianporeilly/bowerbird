//! The orchestrator: one profile, end to end.
//!
//! This is the function a cron-driven `bower run` calls, and the one a future
//! `bower watch` would call on a debounced filesystem event. It owns the
//! sequencing and all of the I/O that the policy engine deliberately does not,
//! including the collision check the engine hands back.

use bower_config::Profile;
use std::collections::HashMap;
use std::path::Path;
use std::time::SystemTime;

use crate::exec::{self, ExecError, Executed, Mode};
use crate::hash;
use crate::llm::{BatchRequest, BatchResponse, LlmBackend, LlmError};
use crate::model::{FileFacts, FileRecord, ResolvedAction};
use crate::policy::{self, Decision, Occupancy, PlanInput};
use crate::scan::{self, ScanError, ScanOptions, Skipped};

/// Guards against a pathological `on_conflict = "suffix"` loop. The policy
/// engine bounds this too; this is the belt to its braces.
const MAX_COLLISION_ROUNDS: u32 = 128;

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error(transparent)]
    Scan(#[from] ScanError),
    #[error(transparent)]
    Llm(#[from] LlmError),
}

/// What happened to one file.
#[derive(Debug, Clone)]
pub struct FileOutcome {
    pub file: FileRecord,
    pub action: ResolvedAction,
    /// `None` when execution failed; see [`FileOutcome::error`].
    pub executed: Option<Executed>,
    pub error: Option<String>,
}

#[derive(Debug, Default)]
pub struct RunReport {
    pub profile: String,
    pub outcomes: Vec<FileOutcome>,
    pub skipped: Vec<Skipped>,
    /// Set when the run completed but something needs a human.
    pub scanned: usize,
}

impl RunReport {
    /// Files the executor actually moved.
    #[must_use]
    pub fn moved(&self) -> usize {
        self.outcomes.iter().filter(|o| matches!(o.executed, Some(Executed::Moved { .. }))).count()
    }

    /// Files a dry run would have moved.
    #[must_use]
    pub fn would_move(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|o| matches!(o.executed, Some(Executed::WouldMove { .. })))
            .count()
    }

    /// Whether anything is waiting on a human, which drives exit code 2.
    #[must_use]
    pub fn needs_attention(&self) -> bool {
        self.outcomes.iter().any(|o| o.action.needs_attention())
    }

    #[must_use]
    pub fn attention_count(&self) -> usize {
        self.outcomes.iter().filter(|o| o.action.needs_attention()).count()
    }

    #[must_use]
    pub fn errors(&self) -> usize {
        self.outcomes.iter().filter(|o| o.error.is_some()).count()
    }
}

#[derive(Debug, Clone)]
pub struct RunOptions {
    pub mode: Mode,
    pub scan: ScanOptions,
}

/// Scans, classifies, resolves, and executes one profile.
pub fn run_profile(
    profile: &Profile,
    backend: &dyn LlmBackend,
    options: &RunOptions,
) -> Result<RunReport, RunError> {
    let scanned = scan::scan(profile, &options.scan)?;
    let mut report =
        RunReport { profile: profile.name.clone(), skipped: scanned.skipped, ..Default::default() };
    report.scanned = scanned.files.len();

    for batch in scanned.files.chunks(profile.batch_size) {
        let response = backend.classify(BatchRequest { profile, files: batch })?;
        for file in batch {
            report.outcomes.push(decide_and_execute(file, &response, profile, options));
        }
    }

    Ok(report)
}

fn decide_and_execute(
    file: &FileRecord,
    response: &BatchResponse,
    profile: &Profile,
    options: &RunOptions,
) -> FileOutcome {
    let outcome = response.outcome_for(&file.id);
    let observed = observe(&file.path);

    let mut decision = policy::plan(&PlanInput { file, outcome: &outcome, profile, observed });

    // The policy engine cannot look at the disk, so it hands the collision
    // check back here. `on_conflict = "suffix"` walks candidate names, so this
    // is a loop rather than a single round trip.
    let mut source_hash: Option<String> = None;
    let mut rounds = 0u32;
    let action = loop {
        match decision {
            Decision::Final(action) => break action,
            Decision::CheckCollision(pending) => {
                rounds += 1;
                if rounds > MAX_COLLISION_ROUNDS {
                    break ResolvedAction::Quarantine {
                        reason: "collision resolution did not converge".to_owned(),
                    };
                }
                let found =
                    match occupancy(&pending.source, pending.dest.as_path(), &mut source_hash) {
                        Ok(o) => o,
                        Err(e) => {
                            return FileOutcome {
                                file: file.clone(),
                                action: ResolvedAction::Quarantine {
                                    reason: format!("could not inspect the destination: {e}"),
                                },
                                executed: None,
                                error: Some(e.to_string()),
                            };
                        }
                    };
                decision = policy::resolve_collision(&pending, found);
            }
        }
    };

    match exec::apply(&action, &file.path, options.mode) {
        Ok(executed) => {
            FileOutcome { file: file.clone(), action, executed: Some(executed), error: None }
        }
        Err(e) => {
            let error = describe(&e);
            FileOutcome { file: file.clone(), action, executed: None, error: Some(error) }
        }
    }
}

/// Re-reads the file's facts immediately before deciding, so the staleness
/// check compares against now rather than against scan time.
fn observe(path: &Path) -> Option<FileFacts> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    Some(FileFacts { size: meta.len(), mtime: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH) })
}

/// Decides what, if anything, occupies `dest`. Hashes only when there is
/// actually something there, and caches the source hash across the suffix loop.
fn occupancy(
    source: &Path,
    dest: &Path,
    source_hash: &mut Option<String>,
) -> std::io::Result<Occupancy> {
    let meta = match std::fs::symlink_metadata(dest) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Occupancy::Vacant),
        Err(e) => return Err(e),
    };
    // A directory or symlink at the destination is never "the same file",
    // whatever its contents.
    if !meta.is_file() {
        return Ok(Occupancy::Different);
    }

    let existing = hash::file_sha256(dest)?;
    let mine = if let Some(h) = source_hash {
        h.clone()
    } else {
        let h = hash::file_sha256(source)?;
        *source_hash = Some(h.clone());
        h
    };
    Ok(if existing == mine { Occupancy::Identical } else { Occupancy::Different })
}

fn describe(e: &ExecError) -> String {
    let mut out = e.to_string();
    let mut source = std::error::Error::source(e);
    while let Some(s) = source {
        use std::fmt::Write as _;
        let _ = write!(out, ": {s}");
        source = s.source();
    }
    out
}

/// Files still awaiting a human, grouped for reporting.
#[must_use]
pub fn attention_summary(report: &RunReport) -> HashMap<&'static str, usize> {
    let mut counts = HashMap::new();
    for o in &report.outcomes {
        let key = match &o.action {
            ResolvedAction::Quarantine { .. } => "quarantine",
            ResolvedAction::RecycleSuggested { .. } => "recycle",
            ResolvedAction::NeedsManualReview { .. } => "review",
            _ => continue,
        };
        *counts.entry(key).or_insert(0) += 1;
    }
    counts
}
