//! The orchestrator: one profile, end to end.
//!
//! This is the function a cron-driven `bower run` calls, and the one a future
//! `bower watch` would call on a debounced filesystem event. It owns the
//! sequencing and all of the I/O that the policy engine deliberately does not:
//! the collision check the engine hands back, the content hashing behind
//! remembered rejections, and every write to the state store.

use bower_config::{Profile, ReviewPlacement};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::exec::{self, ExecContext, ExecError, Executed, Mode, Pending};
use crate::hash;
use crate::llm::{BatchRequest, BatchResponse, LlmBackend, LlmError};
use crate::model::{DestPath, FileFacts, FileRecord, ResolvedAction};
use crate::policy::{self, Decision, Occupancy, PlanInput, PriorRejections};
use crate::scan::{self, ScanError, ScanOptions, Skipped};
use crate::state::{
    JournalAction, JournalSink, NewReviewItem, NoJournal, Provenance, RejectionIndex, ReviewKind,
    StateError, Store,
};

/// Guards against a pathological `on_conflict = "suffix"` loop. The policy
/// engine bounds this too; this is the belt to its braces.
const MAX_COLLISION_ROUNDS: u32 = 128;

/// How many suffixed names to try when parking a file in a holding folder.
const MAX_PLACEMENT_ATTEMPTS: u32 = 100;

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error(transparent)]
    Scan(#[from] ScanError),
    #[error(transparent)]
    Llm(#[from] LlmError),
    #[error(transparent)]
    State(#[from] StateError),
    #[error("review_placement = \"quarantine\" needs general.quarantine_dir to be set")]
    NoQuarantineDir,
}

/// What happened to one file.
#[derive(Debug, Clone)]
pub struct FileOutcome {
    pub file: FileRecord,
    pub action: ResolvedAction,
    /// `None` when execution failed; see [`FileOutcome::error`].
    pub executed: Option<Executed>,
    /// Set when this file was added to the review queue.
    pub queued_id: Option<i64>,
    /// Where the file ended up if it was parked in a holding folder.
    pub parked_at: Option<PathBuf>,
    pub error: Option<String>,
}

#[derive(Debug, Default)]
pub struct RunReport {
    pub profile: String,
    pub outcomes: Vec<FileOutcome>,
    pub skipped: Vec<Skipped>,
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

    /// Rows newly added to the review queue. Fewer than
    /// [`RunReport::attention_count`] when a previous run already queued some.
    #[must_use]
    pub fn newly_queued(&self) -> usize {
        self.outcomes.iter().filter(|o| o.queued_id.is_some()).count()
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
    /// Whether pending items stay put or move to a holding folder.
    pub review_placement: ReviewPlacement,
    pub quarantine_dir: Option<PathBuf>,
}

/// Scans, classifies, resolves, executes, and records one profile.
pub fn run_profile(
    profile: &Profile,
    backend: &dyn LlmBackend,
    options: &RunOptions,
    store: &Store,
) -> Result<RunReport, RunError> {
    if options.review_placement == ReviewPlacement::Quarantine && options.quarantine_dir.is_none() {
        return Err(RunError::NoQuarantineDir);
    }

    let rejections = store.rejections_for(&profile.name)?;

    // Directories this profile has actually written into. Together with the
    // declared categories the scanner already excludes, this is what stops a
    // dynamically created category from being re-ingested as fresh input.
    let mut scan_options = options.scan.clone();
    scan_options.extra_excluded_roots.extend(store.managed_dirs(&profile.name)?);

    let scanned = scan::scan(profile, &scan_options)?;
    let mut report = RunReport {
        profile: profile.name.clone(),
        scanned: scanned.files.len(),
        skipped: scanned.skipped,
        ..Default::default()
    };

    for batch in scanned.files.chunks(profile.batch_size) {
        // The context builder runs here, not inside the backend: what the model
        // may see is a policy decision and stays on this side of the port.
        let ctx = crate::context::build(BatchRequest { profile, files: batch });
        let response = backend.classify(&ctx)?;
        for file in batch {
            report.outcomes.push(handle_file(
                file,
                &response,
                profile,
                options,
                store,
                &rejections,
            ));
        }
    }

    Ok(report)
}

/// Builds the executor's context, routing a dry run's journal writes into a
/// sink that discards them -- nothing was executed, so nothing is recorded.
fn context<'a>(
    profile: &'a Profile,
    options: &RunOptions,
    store: &'a Store,
    file_hash: Option<&'a str>,
    confidence: Option<f32>,
) -> ExecContext<'a> {
    const DISCARD: NoJournal = NoJournal;
    let journal: &dyn JournalSink = if options.mode == Mode::DryRun { &DISCARD } else { store };
    ExecContext {
        profile: &profile.name,
        mode: options.mode,
        file_hash,
        journal,
        // Anything reaching the executor from a run cleared the confidence gate
        // on its own; a queued item that a person approved is journalled from
        // `review`, which records `model_approved` instead.
        provenance: Provenance::model_auto(confidence),
    }
}

/// Content hash computed at most once per file, and only if something actually
/// needs it.
struct LazyHash<'a> {
    path: &'a Path,
    value: Option<String>,
}

impl<'a> LazyHash<'a> {
    fn new(path: &'a Path) -> Self {
        Self { path, value: None }
    }

    fn get(&mut self) -> std::io::Result<&str> {
        if self.value.is_none() {
            self.value = Some(hash::file_sha256(self.path)?);
        }
        Ok(self.value.as_deref().unwrap_or_default())
    }

    fn peek(&self) -> Option<&str> {
        self.value.as_deref()
    }
}

fn handle_file(
    file: &FileRecord,
    response: &BatchResponse,
    profile: &Profile,
    options: &RunOptions,
    store: &Store,
    rejections: &RejectionIndex,
) -> FileOutcome {
    let mut hasher = LazyHash::new(&file.path);

    // Hashing every file on every run just to check for rejections would be
    // wasteful. The size prefilter means a file is only read when its size
    // matches something a human has actually refused.
    let prior = if rejections.is_empty() || !rejections.might_match_size(file.facts.size) {
        PriorRejections::default()
    } else {
        match hasher.get() {
            Ok(digest) => PriorRejections {
                categories: &rejections.rejected_categories(digest),
                deletion: rejections.contains(digest, ReviewKind::Recycle, ""),
            },
            // A file we cannot read is a file we cannot match; the staleness and
            // execution stages will report it properly.
            Err(_) => PriorRejections::default(),
        }
    };

    decide_and_execute(file, response, profile, options, store, prior, &mut hasher)
}

#[allow(clippy::too_many_arguments)]
fn decide_and_execute(
    file: &FileRecord,
    response: &BatchResponse,
    profile: &Profile,
    options: &RunOptions,
    store: &Store,
    prior: PriorRejections<'_>,
    hasher: &mut LazyHash<'_>,
) -> FileOutcome {
    let outcome = response.outcome_for(&file.id);
    let observed = observe(&file.path);

    let mut decision =
        policy::plan(&PlanInput { file, outcome: &outcome, profile, observed, rejected: prior });

    // The policy engine cannot look at the disk, so it hands the collision
    // check back here. `on_conflict = "suffix"` walks candidate names, so this
    // is a loop rather than a single round trip.
    let mut rounds = 0u32;
    let mut confidence: Option<f32> = None;
    let action = loop {
        match decision {
            Decision::Final(action) => break action,
            Decision::CheckCollision(pending) => {
                // The action the loop breaks with carries no confidence, so
                // remember what the gate was given while it is still in hand.
                confidence = Some(pending.confidence);
                rounds += 1;
                if rounds > MAX_COLLISION_ROUNDS {
                    break ResolvedAction::Quarantine {
                        reason: "collision resolution did not converge".to_owned(),
                        proposed: None,
                    };
                }
                let found = match occupancy(pending.dest.as_path(), hasher) {
                    Ok(o) => o,
                    Err(e) => {
                        return failed(
                            file,
                            ResolvedAction::Quarantine {
                                reason: format!("could not inspect the destination: {e}"),
                                proposed: None,
                            },
                            e.to_string(),
                        );
                    }
                };
                decision = policy::resolve_collision(&pending, found);
            }
        }
    };

    // Owned rather than borrowed from `hasher`, which `defer` still needs
    // mutably in order to hash a file nothing has read yet.
    let known_hash = hasher.peek().map(str::to_owned);
    let ctx = context(profile, options, store, known_hash.as_deref(), confidence);

    match exec::apply(&action, &file.path, &ctx) {
        Ok(Executed::Deferred(pending)) => {
            defer(file, action, &pending, profile, options, store, hasher)
        }
        Ok(executed) => FileOutcome {
            file: file.clone(),
            action,
            executed: Some(executed),
            queued_id: None,
            parked_at: None,
            error: None,
        },
        Err(e) => failed(file, action, describe(&e)),
    }
}

/// Records a decision the executor will not make on its own, optionally moving
/// the file to a holding folder first.
fn defer(
    file: &FileRecord,
    action: ResolvedAction,
    pending: &Pending,
    profile: &Profile,
    options: &RunOptions,
    store: &Store,
    hasher: &mut LazyHash<'_>,
) -> FileOutcome {
    let (kind, reason, confidence) = match pending {
        Pending::Review { reason, confidence } => (ReviewKind::Review, reason.clone(), *confidence),
        Pending::Quarantine { reason } => (ReviewKind::Quarantine, reason.clone(), None),
        Pending::Recycle { reason, confidence } => {
            (ReviewKind::Recycle, reason.clone(), Some(*confidence))
        }
    };

    let digest = match hasher.get() {
        Ok(d) => d.to_owned(),
        Err(e) => return failed(file, action, format!("could not hash the file: {e}")),
    };

    // `review_placement = "quarantine"` physically moves pending items so they
    // can be browsed without the CLI. A dry run reports the decision but parks
    // nothing.
    // Parking is performed by the run itself, so it is an automatic operation
    // even though the decision it parks is still pending.
    let ctx = context(profile, options, store, Some(&digest), confidence);
    let mut parked_at = None;
    if options.review_placement == ReviewPlacement::Quarantine && options.mode == Mode::Execute {
        let Some(root) = options.quarantine_dir.as_deref() else {
            return failed(file, action, "quarantine_dir is not configured".to_owned());
        };
        match park(&file.path, root, &profile.name, file.file_name(), &ctx) {
            Ok(dest) => parked_at = Some(dest),
            Err(e) => return failed(file, action, e),
        }
    }

    let current = parked_at.as_deref().unwrap_or(&file.path);
    let category = action.proposed_dest().map_or("", DestPath::category);
    let proposed_dest = action.proposed_dest().map(DestPath::as_path);

    let queued = store.enqueue_review(&NewReviewItem {
        profile: &profile.name,
        kind,
        path: current,
        original_path: &file.path,
        file_hash: &digest,
        category,
        proposed_dest,
        reasoning: reasoning_of(&action),
        confidence,
        reason: &reason,
    });

    match queued {
        Ok(queued_id) => FileOutcome {
            file: file.clone(),
            action,
            executed: Some(Executed::Deferred(pending.clone())),
            queued_id,
            parked_at,
            error: None,
        },
        Err(e) => failed(file, action, e.to_string()),
    }
}

/// Moves a file into a holding folder, walking suffixes past anything already
/// there. Uses the same contained-destination machinery as a normal move, so a
/// parked file is no more able to escape its root than a filed one.
fn park(
    source: &Path,
    root: &Path,
    profile: &str,
    filename: &str,
    ctx: &ExecContext<'_>,
) -> Result<PathBuf, String> {
    let base = DestPath::under(root, profile, filename)
        .map_err(|e| format!("could not build a holding path: {e}"))?;

    for attempt in 0..=MAX_PLACEMENT_ATTEMPTS {
        let candidate = if attempt == 0 {
            base.clone()
        } else {
            match base.with_suffix(attempt) {
                Ok(c) => c,
                Err(e) => return Err(format!("could not build a holding path: {e}")),
            }
        };
        match exec::relocate(source, &candidate, JournalAction::Quarantine, ctx) {
            Ok(_) => return Ok(candidate.as_path().to_path_buf()),
            // Something is already parked under that name; try the next one.
            Err(ExecError::DestinationOccupied { .. }) => {}
            Err(e) => return Err(describe(&e)),
        }
    }
    Err(format!("holding folder {} has no free name for {filename}", root.display()))
}

fn reasoning_of(action: &ResolvedAction) -> &str {
    match action {
        ResolvedAction::NeedsManualReview { raw, .. } => match raw.as_ref() {
            crate::model::ProposalOutcome::Ok(p) => match p {
                crate::model::Proposal::Categorize(c) => &c.reasoning,
                crate::model::Proposal::SuggestDelete(d) => &d.reason,
            },
            _ => "",
        },
        ResolvedAction::RecycleSuggested { reason, .. }
        | ResolvedAction::Quarantine { reason, .. } => reason,
        _ => "",
    }
}

fn failed(file: &FileRecord, action: ResolvedAction, error: impl Into<String>) -> FileOutcome {
    FileOutcome {
        file: file.clone(),
        action,
        executed: None,
        queued_id: None,
        parked_at: None,
        error: Some(error.into()),
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
/// actually something there.
fn occupancy(dest: &Path, hasher: &mut LazyHash<'_>) -> std::io::Result<Occupancy> {
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
    let mine = hasher.get()?;
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
