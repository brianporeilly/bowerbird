//! Undoing link-mode runs.
//!
//! A link mode leaves the source where it was, so undoing it is removing the
//! links: nothing is moved back and nothing is lost. That is also why this only
//! covers links. Undoing a *move* means moving a file back over whatever may
//! have happened since, which is a different and riskier operation that this
//! deliberately does not attempt.
//!
//! The journal says what was made; the disk says what is there now. A link can
//! have been replaced by a real file, repointed, or its source deleted, so
//! every removal is checked against the disk first (see
//! [`exec::remove_link`]) and anything that is no longer exactly the link that
//! was made is left alone and reported.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::exec::{self, ExecError, LinkState};
use crate::state::{Intent, JournalAction, Outcome, Provenance, StateError, Store};
use bower_config::LinkMode;

/// One link the journal says a run made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub profile: String,
    pub source: PathBuf,
    pub dest: PathBuf,
    pub via: LinkMode,
}

/// What happened (or would happen) to one candidate.
#[derive(Debug)]
pub enum Undone {
    /// The link was removed.
    Removed,
    /// Dry run: the link would be removed.
    WouldRemove,
    /// Something else is there now; left alone.
    Changed,
    /// A hard link whose source is gone: the only copy; left alone.
    LastCopy,
    /// Already gone, so nothing to do.
    Gone,
    /// The removal failed.
    Failed(ExecError),
}

/// The links a run made, with the latest record per destination.
///
/// Limited to `profile` and to records at or after `since`. A destination
/// linked, undone and linked again appears once.
pub fn plan(
    store: &Store,
    profile: Option<&str>,
    since: Option<u64>,
) -> Result<Vec<Candidate>, StateError> {
    let mut latest: HashMap<PathBuf, Candidate> = HashMap::new();
    let mut order: Vec<PathBuf> = Vec::new();
    for r in store.link_history(profile, since)? {
        let via = if r.symlink { LinkMode::Symlink } else { LinkMode::Hardlink };
        let c = Candidate { profile: r.profile, source: r.source, dest: r.dest.clone(), via };
        if latest.insert(r.dest.clone(), c).is_none() {
            order.push(r.dest);
        }
    }
    Ok(order.into_iter().filter_map(|d| latest.remove(&d)).collect())
}

/// Removes one link, journalling it as a human action.
///
/// With `dry_run` nothing is written: not the link, and not the journal.
pub fn undo_one(store: &Store, c: &Candidate, dry_run: bool) -> Result<Undone, StateError> {
    match exec::link_state(&c.dest, &c.source, c.via) {
        LinkState::Gone => return Ok(Undone::Gone),
        LinkState::Changed => return Ok(Undone::Changed),
        LinkState::LastCopy => return Ok(Undone::LastCopy),
        LinkState::Ours if dry_run => return Ok(Undone::WouldRemove),
        LinkState::Ours => {}
    }

    let dest_dir = c.dest.parent();
    let intent = Intent {
        profile: &c.profile,
        action: JournalAction::Unlink,
        source: &c.source,
        dest: Some(&c.dest),
        dest_dir,
        file_hash: None,
        provenance: Provenance::human(),
    };
    let op = store.record_intent(&intent)?;
    match exec::remove_link(&c.dest, &c.source, c.via) {
        Ok(LinkState::Ours) => {
            store.record_result(&op, &intent, &Outcome::Committed)?;
            Ok(Undone::Removed)
        }
        // The disk changed between the check above and the removal.
        Ok(other) => {
            let detail = format!("not removed: {other:?}");
            store.record_result(&op, &intent, &Outcome::Failed { detail })?;
            Ok(match other {
                LinkState::Gone => Undone::Gone,
                LinkState::LastCopy => Undone::LastCopy,
                _ => Undone::Changed,
            })
        }
        Err(e) => {
            store.record_result(&op, &intent, &Outcome::Failed { detail: e.to_string() })?;
            Ok(Undone::Failed(e))
        }
    }
}
