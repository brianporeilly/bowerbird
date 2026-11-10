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
//!
//! # Link modes
//!
//! Under `link_mode = "hardlink"` or `"symlink"` the source is left in place
//! and a link is created at the destination. `link(2)` and `symlink(2)` both
//! fail with `EEXIST` rather than replacing anything, so the no-overwrite
//! guarantee is kept for free. A hard link that cannot be made (a different
//! filesystem) is an error, **not** a copy: someone who asked for links did not
//! ask for a second copy of every file, and quietly making one would defeat the
//! point of the mode.

use std::fs;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};

use crate::model::{DestPath, NoOpReason, ResolvedAction};
use crate::state::{Intent, JournalAction, JournalSink, Outcome, Provenance, StateError};
use bower_config::LinkMode;

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
    Quarantine {
        reason: String,
    },
    Recycle {
        reason: String,
        confidence: f32,
    },
    /// `confidence` is the model's, when a model produced the proposal that
    /// ended up here. Recorded because it is usually *why* the item is here at
    /// all, and because a person approving it later writes it to the journal --
    /// where dropping it would lose the number permanently.
    Review {
        reason: String,
        confidence: Option<f32>,
    },
}

/// What the executor did, or would have done.
#[derive(Debug, Clone, PartialEq)]
pub enum Executed {
    Moved {
        from: PathBuf,
        to: PathBuf,
        renamed: bool,
        /// How the file was placed. Anything but [`LinkMode::Move`] left the
        /// source where it was.
        via: LinkMode,
    },
    WouldMove {
        from: PathBuf,
        to: PathBuf,
        renamed: bool,
        via: LinkMode,
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
    /// What proposed this operation and who let it through. Recorded on every
    /// journal row; see [`crate::state::Provenance`].
    pub provenance: Provenance,
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

/// Carries out one resolved action, moving files.
pub fn apply(
    action: &ResolvedAction,
    source: &Path,
    ctx: &ExecContext<'_>,
) -> Result<Executed, ExecError> {
    apply_via(action, source, ctx, LinkMode::Move)
}

/// Carries out one resolved action, placing a filed file the way `via` says.
///
/// Only the placement into the organized view honours `via`. A deferred
/// decision, or anything that is not a move, is unaffected.
pub fn apply_via(
    action: &ResolvedAction,
    source: &Path,
    ctx: &ExecContext<'_>,
    via: LinkMode,
) -> Result<Executed, ExecError> {
    match action {
        ResolvedAction::Move { dest } => relocate_via(source, dest, JournalAction::Move, via, ctx),
        ResolvedAction::MoveAndRename { dest } => {
            relocate_via(source, dest, JournalAction::MoveAndRename, via, ctx)
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
        ResolvedAction::NeedsManualReview { reason, raw, .. } => {
            Ok(Executed::Deferred(Pending::Review {
                reason: reason.clone(),
                confidence: raw.confidence(),
            }))
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
    relocate_via(source, dest, what, LinkMode::Move, ctx)
}

/// As [`relocate`], but a filing operation may leave the source in place and
/// link to it instead.
///
/// `via` applies only when `what` is a filing (`Move` or `MoveAndRename`).
/// Parking, recycling and restoring are always real moves: those exist to get
/// a file out of the way, and a link would leave it exactly where it was.
pub fn relocate_via(
    source: &Path,
    dest: &DestPath,
    what: JournalAction,
    via: LinkMode,
    ctx: &ExecContext<'_>,
) -> Result<Executed, ExecError> {
    let filing = matches!(what, JournalAction::Move | JournalAction::MoveAndRename);
    let via = if filing { via } else { LinkMode::Move };
    // What the journal says happened: a link is not a move, and a reader should
    // not have to infer from the config of the day which one it was.
    let recorded = match via {
        LinkMode::Move => what,
        LinkMode::Hardlink => JournalAction::Hardlink,
        LinkMode::Symlink => JournalAction::Symlink,
    };
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
            via,
        });
    }

    let parent = dest.parent_dir();
    let intent = Intent {
        profile: ctx.profile,
        action: recorded,
        source,
        dest: Some(dest.as_path()),
        dest_dir: Some(&parent),
        file_hash: ctx.file_hash,
        provenance: ctx.provenance,
    };
    let op = ctx.journal.record_intent(&intent)?;

    let outcome = fs::create_dir_all(&parent)
        .map_err(|e| ExecError::Io { op: "create directory", path: parent.clone(), source: e })
        .and_then(|()| match via {
            LinkMode::Move => move_no_clobber(source, dest.as_path()),
            LinkMode::Hardlink => hardlink_no_clobber(source, dest.as_path()),
            LinkMode::Symlink => symlink_no_clobber(source, dest.as_path()),
        });

    match outcome {
        Ok(()) => {
            ctx.journal.record_result(&op, &intent, &Outcome::Committed)?;
            Ok(Executed::Moved {
                from: source.to_path_buf(),
                to: dest.as_path().to_path_buf(),
                renamed,
                via,
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
pub(crate) fn move_no_clobber(src: &Path, dest: &Path) -> Result<(), ExecError> {
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

/// Adds a hard link at `dest` and leaves `src` alone.
///
/// Never copies. A different filesystem is reported as such, with the way out.
pub(crate) fn hardlink_no_clobber(src: &Path, dest: &Path) -> Result<(), ExecError> {
    match fs::hard_link(src, dest) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            Err(ExecError::DestinationOccupied { dest: dest.to_path_buf() })
        }
        Err(e) if e.kind() == ErrorKind::CrossesDevices => Err(ExecError::Io {
            op: "hard link across filesystems (link_mode = \"symlink\" can cross them) to",
            path: dest.to_path_buf(),
            source: e,
        }),
        Err(source) => Err(ExecError::Io { op: "hard link to", path: dest.to_path_buf(), source }),
    }
}

/// Adds a symbolic link at `dest` pointing at `src`, and leaves `src` alone.
///
/// The target is `src` exactly as given, so it must be absolute: a relative
/// target would be read from the link's own directory and point nowhere.
#[cfg(unix)]
pub(crate) fn symlink_no_clobber(src: &Path, dest: &Path) -> Result<(), ExecError> {
    if !src.is_absolute() {
        return Err(ExecError::Io {
            op: "symlink to a relative source from",
            path: dest.to_path_buf(),
            source: io::Error::new(ErrorKind::InvalidInput, "the link target must be absolute"),
        });
    }
    match std::os::unix::fs::symlink(src, dest) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            Err(ExecError::DestinationOccupied { dest: dest.to_path_buf() })
        }
        Err(source) => {
            Err(ExecError::Io { op: "create a symlink at", path: dest.to_path_buf(), source })
        }
    }
}

#[cfg(not(unix))]
pub(crate) fn symlink_no_clobber(_src: &Path, dest: &Path) -> Result<(), ExecError> {
    Err(ExecError::Io {
        op: "create a symlink at",
        path: dest.to_path_buf(),
        source: io::Error::new(ErrorKind::Unsupported, "symlink mode needs a unix platform"),
    })
}

/// Whether `dest` is still the link an earlier run made to `src`.
///
/// Used to recognise a file that has already been filed, so a re-run neither
/// asks the model again nor reports its own link as a conflicting file.
#[must_use]
pub fn is_link_to(dest: &Path, src: &Path, via: LinkMode) -> bool {
    match via {
        LinkMode::Move => false,
        LinkMode::Symlink => fs::read_link(dest).is_ok_and(|target| target == src),
        LinkMode::Hardlink => same_file(dest, src),
    }
}

/// Whether two paths are the same file on disk -- a hard link of each other --
/// without following a symlink at `a`.
#[cfg(unix)]
fn same_file(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (fs::symlink_metadata(a), fs::symlink_metadata(b)) {
        (Ok(x), Ok(y)) => x.is_file() && y.is_file() && x.dev() == y.dev() && x.ino() == y.ino(),
        _ => false,
    }
}

#[cfg(not(unix))]
fn same_file(_a: &Path, _b: &Path) -> bool {
    false
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
        ExecContext {
            profile: "test",
            mode,
            file_hash: None,
            journal: &DISCARD,
            provenance: Provenance::model_auto(Some(0.9)),
        }
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
            provenance: Provenance::model_auto(Some(0.9)),
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
            provenance: Provenance::model_auto(Some(0.9)),
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
            provenance: Provenance::model_auto(Some(0.9)),
            profile: "downloads",
            mode: Mode::DryRun,
            file_hash: None,
            journal: &store,
        };
        apply(&ResolvedAction::Move { dest }, &src, &context).unwrap();

        assert!(
            store.unfinished_operations().unwrap().is_empty(),
            "a dry run records no intent, so nothing can be left unfinished"
        );
        assert!(store.managed_dirs("downloads").unwrap().is_empty(), "nothing happened to record");
    }
}
