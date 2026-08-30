//! The executor: the only component that mutates the filesystem.
//!
//! It accepts a [`ResolvedAction`], never a bare path, so every write it
//! performs is one the policy engine already proved lies under a profile's
//! destination root.
//!
//! # Never overwriting
//!
//! `rename(2)` silently replaces an existing destination, which would make the
//! collision check advisory rather than binding -- anything that appeared at
//! the destination between the check and the move would be destroyed. So the
//! executor does not use it. Instead:
//!
//! * **Same filesystem:** `link(2)` then `unlink(2)`. `link` fails with
//!   `EEXIST` rather than clobbering, and the file is reachable at both paths
//!   throughout, so a crash mid-move loses nothing.
//! * **Otherwise** (different filesystem, or one without hard links): copy into
//!   the destination opened with `O_CREAT | O_EXCL`, fsync, then unlink the
//!   source. Not atomic, but still incapable of overwriting.
//!
//! Either way a lost race is reported as [`ExecError::DestinationOccupied`],
//! for the caller to re-plan, never as a silent replacement.

use std::fs;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};

use crate::model::{DestPath, NoOpReason, ResolvedAction};
use crate::state::{Intent, JournalAction, JournalSink, Outcome, StateError};

/// Whether the executor is allowed to write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Report what would happen; touch nothing.
    DryRun,
    Execute,
}

impl Mode {
    #[must_use]
    pub fn from_dry_run(dry_run: bool) -> Self {
        if dry_run { Self::DryRun } else { Self::Execute }
    }
}

/// A decision the executor cannot carry out on its own because it belongs in
/// the review queue.
#[derive(Debug, Clone, PartialEq)]
pub enum Pending {
    Quarantine { reason: String },
    Recycle { reason: String, confidence: f32 },
    Review { reason: String },
}

/// What the executor did, or would have done.
#[derive(Debug, Clone, PartialEq)]
pub enum Executed {
    Moved {
        from: PathBuf,
        to: PathBuf,
        renamed: bool,
    },
    WouldMove {
        from: PathBuf,
        to: PathBuf,
        renamed: bool,
    },
    Nothing {
        reason: NoOpReason,
    },
    /// Needs a row in the review queue, which the executor does not own.
    Deferred(Pending),
}

/// Everything an operation needs besides the action itself.
#[derive(Clone, Copy)]
pub struct ExecContext<'a> {
    pub profile: &'a str,
    pub mode: Mode,
    /// Content hash, when the caller already computed one. Recorded in the
    /// journal so an entry identifies the bytes, not just a path.
    pub file_hash: Option<&'a str>,
    pub journal: &'a dyn JournalSink,
}

impl std::fmt::Debug for ExecContext<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecContext")
            .field("profile", &self.profile)
            .field("mode", &self.mode)
            .field("file_hash", &self.file_hash)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    #[error("something already exists at {dest}; refusing to overwrite")]
    DestinationOccupied { dest: PathBuf },
    #[error("could not record the operation in the journal; refusing to proceed unrecorded")]
    Journal(#[from] StateError),
    #[error("source file {path} vanished before it could be moved")]
    SourceVanished { path: PathBuf },
    #[error("could not {op} {path}")]
    Io {
        op: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// Carries out one resolved action.
pub fn apply(
    action: &ResolvedAction,
    source: &Path,
    ctx: &ExecContext<'_>,
) -> Result<Executed, ExecError> {
    match action {
        ResolvedAction::Move { dest } => relocate(source, dest, JournalAction::Move, ctx),
        ResolvedAction::MoveAndRename { dest } => {
            relocate(source, dest, JournalAction::MoveAndRename, ctx)
        }
        ResolvedAction::NoOp { reason } => Ok(Executed::Nothing { reason: reason.clone() }),
        ResolvedAction::Quarantine { reason, .. } => {
            Ok(Executed::Deferred(Pending::Quarantine { reason: reason.clone() }))
        }
        ResolvedAction::RecycleSuggested { reason, confidence } => {
            Ok(Executed::Deferred(Pending::Recycle {
                reason: reason.clone(),
                confidence: *confidence,
            }))
        }
        ResolvedAction::NeedsManualReview { reason, .. } => {
            Ok(Executed::Deferred(Pending::Review { reason: reason.clone() }))
        }
    }
}

/// Moves a file to a proven-contained destination, journalling before and
/// after.
///
/// The intent is recorded *before* the filesystem is touched, so a crash
/// part-way leaves an intent with no result -- the only way an interrupted move
/// can be told apart from one that never started. A journal that will not
/// accept the intent aborts the operation rather than proceeding unrecorded.
pub fn relocate(
    source: &Path,
    dest: &DestPath,
    what: JournalAction,
    ctx: &ExecContext<'_>,
) -> Result<Executed, ExecError> {
    let renamed = what == JournalAction::MoveAndRename
        || source.file_name().is_none_or(|n| n != dest.filename());

    if !source.exists() {
        return Err(ExecError::SourceVanished { path: source.to_path_buf() });
    }
    if ctx.mode == Mode::DryRun {
        return Ok(Executed::WouldMove {
            from: source.to_path_buf(),
            to: dest.as_path().to_path_buf(),
            renamed,
        });
    }

    let parent = dest.parent_dir();
    let intent = Intent {
        profile: ctx.profile,
        action: what,
        source,
        dest: Some(dest.as_path()),
        dest_dir: Some(&parent),
        file_hash: ctx.file_hash,
    };
    let op = ctx.journal.record_intent(&intent)?;

    let outcome = fs::create_dir_all(&parent)
        .map_err(|e| ExecError::Io { op: "create directory", path: parent.clone(), source: e })
        .and_then(|()| move_no_clobber(source, dest.as_path()));

    match outcome {
        Ok(()) => {
            ctx.journal.record_result(&op, &intent, &Outcome::Committed)?;
            Ok(Executed::Moved {
                from: source.to_path_buf(),
                to: dest.as_path().to_path_buf(),
                renamed,
            })
        }
        Err(e) => {
            // Best effort: the operation already failed, and losing the record
            // of that must not mask the original error.
            let _ =
                ctx.journal.record_result(&op, &intent, &Outcome::Failed { detail: e.to_string() });
            Err(e)
        }
    }
}

/// Moves `src` to `dest`, failing rather than replacing anything already there.
fn move_no_clobber(src: &Path, dest: &Path) -> Result<(), ExecError> {
    match fs::hard_link(src, dest) {
        Ok(()) => fs::remove_file(src).map_err(|source| ExecError::Io {
            op: "remove source after linking",
            path: src.to_path_buf(),
            source,
        }),
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            Err(ExecError::DestinationOccupied { dest: dest.to_path_buf() })
        }
        // A different filesystem, or one without hard links. Fall back to a
        // copy that still cannot overwrite.
        Err(_) => copy_no_clobber(src, dest),
    }
}

/// Streams `src` into a newly created `dest`, then removes `src`.
///
/// `create_new` is what makes this safe: it fails if anything is already there.
/// A partially written destination can survive a crash here, which is the price
/// of crossing a filesystem boundary without `rename(2)`.
pub(crate) fn copy_no_clobber(src: &Path, dest: &Path) -> Result<(), ExecError> {
    let mut reader = fs::File::open(src).map_err(|source| ExecError::Io {
        op: "open source",
        path: src.to_path_buf(),
        source,
    })?;

    let mut writer = match fs::OpenOptions::new().write(true).create_new(true).open(dest) {
        Ok(w) => w,
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            return Err(ExecError::DestinationOccupied { dest: dest.to_path_buf() });
        }
        Err(source) => {
            return Err(ExecError::Io {
                op: "create destination",
                path: dest.to_path_buf(),
                source,
            });
        }
    };

    // Any failure past this point leaves a partial file behind, so clean it up
    // rather than leaving something that looks like a filed document.
    let copied = io::copy(&mut reader, &mut writer).and_then(|_| writer.sync_all());
    if let Err(source) = copied {
        let _ = fs::remove_file(dest);
        return Err(ExecError::Io { op: "copy into", path: dest.to_path_buf(), source });
    }
    drop(writer);

    fs::remove_file(src).map_err(|source| ExecError::Io {
        op: "remove source after copying",
        path: src.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{NoJournal, Store};
    use std::io::Write;

    /// A context that records nothing, for tests of the move mechanics.
    fn ctx(mode: Mode) -> ExecContext<'static> {
        const DISCARD: NoJournal = NoJournal;
        ExecContext { profile: "test", mode, file_hash: None, journal: &DISCARD }
    }

    fn write(path: &Path, body: &str) {
        if let Some(p) = path.parent() {
            fs::create_dir_all(p).unwrap();
        }
        fs::File::create(path).unwrap().write_all(body.as_bytes()).unwrap();
    }

    #[test]
    fn moves_a_file_and_creates_the_category_directory() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("invoice.pdf");
        write(&src, "body");

        let dest = DestPath::under(dir.path(), "Invoices", "invoice.pdf").unwrap();
        let out =
            apply(&ResolvedAction::Move { dest: dest.clone() }, &src, &ctx(Mode::Execute)).unwrap();

        assert!(matches!(out, Executed::Moved { renamed: false, .. }));
        assert!(!src.exists(), "source should be gone");
        assert_eq!(fs::read_to_string(dest.as_path()).unwrap(), "body");
    }

    #[test]
    fn refuses_to_overwrite_an_occupied_destination() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("invoice.pdf");
        write(&src, "new");

        let dest = DestPath::under(dir.path(), "Invoices", "invoice.pdf").unwrap();
        write(dest.as_path(), "existing");

        let err = apply(&ResolvedAction::Move { dest: dest.clone() }, &src, &ctx(Mode::Execute))
            .unwrap_err();

        assert!(matches!(err, ExecError::DestinationOccupied { .. }));
        assert_eq!(fs::read_to_string(dest.as_path()).unwrap(), "existing", "must not clobber");
        assert!(src.exists(), "source must survive a refused move");
    }

    #[test]
    fn copy_fallback_also_refuses_to_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("a.txt");
        let dest = dir.path().join("b.txt");
        write(&src, "new");
        write(&dest, "existing");

        let err = copy_no_clobber(&src, &dest).unwrap_err();
        assert!(matches!(err, ExecError::DestinationOccupied { .. }));
        assert_eq!(fs::read_to_string(&dest).unwrap(), "existing");
        assert!(src.exists());
    }

    #[test]
    fn copy_fallback_moves_content_and_removes_the_source() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("a.txt");
        let dest = dir.path().join("nested").join("b.txt");
        write(&src, "payload");
        fs::create_dir_all(dest.parent().unwrap()).unwrap();

        copy_no_clobber(&src, &dest).unwrap();
        assert_eq!(fs::read_to_string(&dest).unwrap(), "payload");
        assert!(!src.exists());
    }

    #[test]
    fn dry_run_touches_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("invoice.pdf");
        write(&src, "body");

        let dest = DestPath::under(dir.path(), "Invoices", "invoice.pdf").unwrap();
        let out =
            apply(&ResolvedAction::Move { dest: dest.clone() }, &src, &ctx(Mode::DryRun)).unwrap();

        assert!(matches!(out, Executed::WouldMove { .. }));
        assert!(src.exists(), "source must be untouched");
        assert!(!dest.parent_dir().exists(), "must not even create the category directory");
    }

    #[test]
    fn a_vanished_source_is_an_error_not_a_silent_success() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("gone.pdf");
        let dest = DestPath::under(dir.path(), "Invoices", "gone.pdf").unwrap();

        let err = apply(&ResolvedAction::Move { dest }, &src, &ctx(Mode::Execute)).unwrap_err();
        assert!(matches!(err, ExecError::SourceVanished { .. }));
    }

    #[test]
    fn review_actions_are_deferred_not_executed() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("x.pdf");
        write(&src, "b");

        let action = ResolvedAction::RecycleSuggested {
            reason: "looks like a duplicate installer".into(),
            confidence: 0.99,
        };
        let out = apply(&action, &src, &ctx(Mode::Execute)).unwrap();

        assert!(matches!(out, Executed::Deferred(Pending::Recycle { .. })));
        assert!(src.exists(), "a recycle suggestion must never move a file on its own");
    }

    #[test]
    fn a_move_is_journalled_before_and_after() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        let src = dir.path().join("invoice.pdf");
        write(&src, "body");
        let dest = DestPath::under(dir.path(), "Invoices", "invoice.pdf").unwrap();

        let context = ExecContext {
            profile: "downloads",
            mode: Mode::Execute,
            file_hash: Some("deadbeef"),
            journal: &store,
        };
        apply(&ResolvedAction::Move { dest: dest.clone() }, &src, &context).unwrap();

        assert!(
            store.unfinished_operations().unwrap().is_empty(),
            "a completed move leaves no dangling intent"
        );
        assert_eq!(
            store.managed_dirs("downloads").unwrap(),
            [dest.parent_dir()],
            "the journal records which directory was written into"
        );
    }

    #[test]
    fn a_refused_move_is_journalled_as_failed_and_claims_no_directory() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        let src = dir.path().join("invoice.pdf");
        write(&src, "new");
        let dest = DestPath::under(dir.path(), "Invoices", "invoice.pdf").unwrap();
        write(dest.as_path(), "existing");

        let context = ExecContext {
            profile: "downloads",
            mode: Mode::Execute,
            file_hash: None,
            journal: &store,
        };
        apply(&ResolvedAction::Move { dest }, &src, &context).unwrap_err();

        assert!(store.unfinished_operations().unwrap().is_empty(), "the failure was recorded");
        assert!(
            store.managed_dirs("downloads").unwrap().is_empty(),
            "a directory nothing landed in is not managed"
        );
    }

    #[test]
    fn a_dry_run_records_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        let src = dir.path().join("invoice.pdf");
        write(&src, "body");
        let dest = DestPath::under(dir.path(), "Invoices", "invoice.pdf").unwrap();

        let context = ExecContext {
            profile: "downloads",
            mode: Mode::DryRun,
            file_hash: None,
            journal: &store,
        };
        apply(&ResolvedAction::Move { dest }, &src, &context).unwrap();

        assert!(store.unfinished_operations().unwrap().is_empty());
        assert!(store.managed_dirs("downloads").unwrap().is_empty(), "nothing happened to record");
    }
}
