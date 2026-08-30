//! Resolving pending decisions: approve, reject, restore, purge.
//!
//! # Re-validation
//!
//! A queued row may have been sitting for days. The file it describes can have
//! changed, been moved, or vanished in the meantime, so nothing here acts on a
//! row without first confirming the bytes still hash to what was proposed
//! (ADR-0001 §7). A row whose file no longer matches is discarded rather than
//! executed: the proposal was about a file that no longer exists in that form,
//! and the next run will make a fresh one.
//!
//! # Config remains authoritative
//!
//! Approval re-resolves the category against the profile *as it stands now*,
//! not as it stood when the row was written. A category since removed from a
//! profile is not filed into just because a row remembers it.

use bower_config::Profile;
use std::path::{Path, PathBuf};

use crate::exec::{self, ExecContext, ExecError, Mode};
use crate::hash;
use crate::model::DestPath;
use crate::policy;
use crate::state::{
    JournalAction, JournalSink, NoJournal, RecycleItem, ReviewItem, ReviewKind, StateError, Store,
};

/// How many suffixed names to try when the destination is occupied.
const MAX_ATTEMPTS: u32 = 100;

#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("{path} no longer exists; the queued decision has been discarded")]
    Vanished { path: PathBuf },
    #[error("{path} has changed since it was proposed; the queued decision has been discarded")]
    Changed { path: PathBuf },
    #[error(
        "this item never got far enough to have a destination, so there is nothing to approve; \
         reject it instead"
    )]
    NothingProposed,
    #[error("profile `{0}` is no longer defined in the config")]
    NoSuchProfile(String),
    #[error("category `{category}` is no longer permitted by profile `{profile}`")]
    CategoryNoLongerAllowed { profile: String, category: String },
    #[error("approving a deletion needs general.recycle_dir to be set")]
    NoRecycleDir,
    #[error("no free name for {filename} in {dir}")]
    NoFreeName { filename: String, dir: PathBuf },
    #[error(transparent)]
    Exec(#[from] ExecError),
    #[error(transparent)]
    State(#[from] StateError),
    #[error("could not read {path}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// What approving an item did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Approved {
    Filed { to: PathBuf },
    Recycled { to: PathBuf },
    WouldFile { to: PathBuf },
    WouldRecycle { to: PathBuf },
}

/// What rejecting an item did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejected {
    /// Set when the file was moved back out of the holding folder.
    pub restored_to: Option<PathBuf>,
    /// Set when a restore was attempted and could not be completed.
    pub restore_failed: Option<String>,
}

/// Everything resolution needs beyond the item itself.
#[derive(Debug, Clone, Copy)]
pub struct ResolveOptions<'a> {
    pub mode: Mode,
    pub recycle_dir: Option<&'a Path>,
}

/// Confirms the file is still the one the proposal was about.
///
/// On any mismatch the queued row is dropped, so a stale decision cannot linger
/// and cannot be approved later by someone who did not notice.
fn revalidate(store: &Store, item: &ReviewItem) -> Result<(), ResolveError> {
    if !item.path.is_file() {
        store.review_discard_stale(item.id)?;
        return Err(ResolveError::Vanished { path: item.path.clone() });
    }
    let digest = hash::file_sha256(&item.path)
        .map_err(|source| ResolveError::Io { path: item.path.clone(), source })?;
    if digest != item.file_hash {
        store.review_discard_stale(item.id)?;
        return Err(ResolveError::Changed { path: item.path.clone() });
    }
    Ok(())
}

fn context<'a>(
    profile: &'a str,
    mode: Mode,
    store: &'a Store,
    file_hash: Option<&'a str>,
) -> ExecContext<'a> {
    const DISCARD: NoJournal = NoJournal;
    let journal: &dyn JournalSink = if mode == Mode::DryRun { &DISCARD } else { store };
    ExecContext { profile, mode, file_hash, journal }
}

/// Carries out a queued decision.
pub fn approve(
    store: &Store,
    item: &ReviewItem,
    profile: &Profile,
    options: &ResolveOptions<'_>,
) -> Result<Approved, ResolveError> {
    if profile.name != item.profile {
        return Err(ResolveError::NoSuchProfile(item.profile.clone()));
    }
    revalidate(store, item)?;

    match item.kind {
        ReviewKind::Recycle => approve_recycle(store, item, options),
        ReviewKind::Review | ReviewKind::Quarantine => approve_move(store, item, profile, options),
    }
}

fn approve_move(
    store: &Store,
    item: &ReviewItem,
    profile: &Profile,
    options: &ResolveOptions<'_>,
) -> Result<Approved, ResolveError> {
    let Some(proposed) = item.proposed_dest.as_ref() else {
        return Err(ResolveError::NothingProposed);
    };
    let filename =
        proposed.file_name().and_then(|n| n.to_str()).ok_or(ResolveError::NothingProposed)?;

    // The config is authoritative, not the row: re-resolve against the profile
    // as it stands now.
    let category = policy::resolve_category(profile, &item.category).ok_or_else(|| {
        ResolveError::CategoryNoLongerAllowed {
            profile: profile.name.clone(),
            category: item.category.clone(),
        }
    })?;

    let base = DestPath::under(&profile.destination_root, &category, filename).map_err(|e| {
        ResolveError::Exec(ExecError::Io {
            op: "build a destination for",
            path: proposed.clone(),
            source: std::io::Error::other(e.to_string()),
        })
    })?;

    let ctx = context(&profile.name, options.mode, store, Some(&item.file_hash));
    let placed = place(&item.path, &base, JournalAction::Move, &ctx)?;

    if options.mode == Mode::DryRun {
        return Ok(Approved::WouldFile { to: placed });
    }
    store.review_remove(item.id)?;
    Ok(Approved::Filed { to: placed })
}

fn approve_recycle(
    store: &Store,
    item: &ReviewItem,
    options: &ResolveOptions<'_>,
) -> Result<Approved, ResolveError> {
    let root = options.recycle_dir.ok_or(ResolveError::NoRecycleDir)?;
    let filename =
        item.path.file_name().and_then(|n| n.to_str()).ok_or(ResolveError::NothingProposed)?;

    // The recycle store is laid out as <root>/<profile>/<name>, not as a mirror
    // of the original path. Restore does not need a mirror -- the row records
    // original_path -- and a flat layout keeps every recycled file inside the
    // same two-component containment guarantee that protects a filed one.
    let base = DestPath::under(root, &item.profile, filename).map_err(|e| {
        ResolveError::Exec(ExecError::Io {
            op: "build a recycle path for",
            path: item.path.clone(),
            source: std::io::Error::other(e.to_string()),
        })
    })?;

    let ctx = context(&item.profile, options.mode, store, Some(&item.file_hash));
    let stored = place(&item.path, &base, JournalAction::Recycle, &ctx)?;

    if options.mode == Mode::DryRun {
        return Ok(Approved::WouldRecycle { to: stored });
    }
    store.record_recycled(
        &item.profile,
        &item.original_path,
        &stored,
        &item.file_hash,
        &item.reason,
    )?;
    store.review_remove(item.id)?;
    Ok(Approved::Recycled { to: stored })
}

/// Refuses a queued decision, remembering it so the next run does not ask
/// again, and putting a parked file back where it came from.
pub fn reject(
    store: &Store,
    item: &ReviewItem,
    reason: Option<&str>,
    options: &ResolveOptions<'_>,
) -> Result<Rejected, ResolveError> {
    let size = std::fs::metadata(&item.path).map_or(0, |m| m.len());
    store.remember_rejection(
        &item.profile,
        item.kind,
        &item.file_hash,
        size,
        &item.category,
        reason,
    )?;

    // A rejected item is no longer pending, so leaving it in the pending folder
    // would be wrong. Best effort: if the original location is now occupied the
    // file stays put and the caller is told.
    let mut restored_to = None;
    let mut restore_failed = None;
    if item.path != item.original_path && options.mode == Mode::Execute && item.path.is_file() {
        match restore_to_original(store, &item.profile, &item.path, &item.original_path) {
            Ok(()) => restored_to = Some(item.original_path.clone()),
            Err(e) => restore_failed = Some(e.to_string()),
        }
    }

    store.review_remove(item.id)?;
    Ok(Rejected { restored_to, restore_failed })
}

/// Moves a file back to a path outside any destination root.
///
/// [`DestPath`] deliberately cannot express this -- it only ever names
/// `<root>/<category>/<file>` -- so a restore is not a policy decision being
/// executed but the undoing of one, back to a path the store recorded before
/// anything was moved.
fn restore_to_original(
    store: &Store,
    profile: &str,
    from: &Path,
    to: &Path,
) -> Result<(), ResolveError> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|source| ResolveError::Io { path: parent.to_path_buf(), source })?;
    }
    let intent = crate::state::Intent {
        profile,
        action: JournalAction::Restore,
        source: from,
        dest: Some(to),
        dest_dir: None,
        file_hash: None,
    };
    let op = store.record_intent(&intent)?;
    match exec::move_no_clobber(from, to) {
        Ok(()) => {
            store.record_result(&op, &intent, &crate::state::Outcome::Committed)?;
            Ok(())
        }
        Err(e) => {
            let _ = store.record_result(
                &op,
                &intent,
                &crate::state::Outcome::Failed { detail: e.to_string() },
            );
            Err(ResolveError::Exec(e))
        }
    }
}

/// Moves to `base`, walking suffixes past anything already there. A human
/// saying yes is not a licence to overwrite.
fn place(
    source: &Path,
    base: &DestPath,
    what: JournalAction,
    ctx: &ExecContext<'_>,
) -> Result<PathBuf, ResolveError> {
    for attempt in 0..=MAX_ATTEMPTS {
        let candidate = if attempt == 0 {
            base.clone()
        } else {
            base.with_suffix(attempt).map_err(|e| {
                ResolveError::Exec(ExecError::Io {
                    op: "build a suffixed destination for",
                    path: base.as_path().to_path_buf(),
                    source: std::io::Error::other(e.to_string()),
                })
            })?
        };
        match exec::relocate(source, &candidate, what, ctx) {
            Ok(_) => return Ok(candidate.as_path().to_path_buf()),
            // Occupied; try the next name.
            Err(ExecError::DestinationOccupied { .. }) => {}
            Err(e) => return Err(ResolveError::Exec(e)),
        }
    }
    Err(ResolveError::NoFreeName { filename: base.filename().to_owned(), dir: base.parent_dir() })
}

// --- the recycle store ------------------------------------------------------

/// Moves a recycled file back to where it came from.
pub fn restore(
    store: &Store,
    item: &RecycleItem,
    options: &ResolveOptions<'_>,
) -> Result<PathBuf, ResolveError> {
    if !item.stored_path.is_file() {
        return Err(ResolveError::Vanished { path: item.stored_path.clone() });
    }
    if options.mode == Mode::DryRun {
        return Ok(item.original_path.clone());
    }
    restore_to_original(store, &item.profile, &item.stored_path, &item.original_path)?;
    store.recycle_remove(item.id)?;
    Ok(item.original_path.clone())
}

/// Permanently removes a recycled file.
///
/// The only path in the codebase that destroys anything, reachable only from
/// `bower recycle purge` -- never from a run, and never from approving a
/// deletion, which merely moves the file into the recycle store.
pub fn purge(
    store: &Store,
    item: &RecycleItem,
    options: &ResolveOptions<'_>,
) -> Result<(), ResolveError> {
    if options.mode == Mode::DryRun {
        return Ok(());
    }
    let intent = crate::state::Intent {
        profile: &item.profile,
        action: JournalAction::Purge,
        source: &item.stored_path,
        dest: None,
        dest_dir: None,
        file_hash: Some(&item.file_hash),
    };
    let op = store.record_intent(&intent)?;

    let removed = match std::fs::remove_file(&item.stored_path) {
        Ok(()) => Ok(()),
        // Already gone is the outcome we wanted; the index row still needs
        // clearing.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    };

    match removed {
        Ok(()) => {
            store.record_result(&op, &intent, &crate::state::Outcome::Committed)?;
            store.recycle_remove(item.id)?;
            Ok(())
        }
        Err(source) => {
            let _ = store.record_result(
                &op,
                &intent,
                &crate::state::Outcome::Failed { detail: source.to_string() },
            );
            Err(ResolveError::Io { path: item.stored_path.clone(), source })
        }
    }
}
