//! Domain types shared by every stage of the pipeline.

mod dest;

pub use dest::{DestPath, PathError, split_extension};

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Opaque handle the LLM uses to refer to a file.
///
/// The model is never shown a filesystem path in a position where it could echo
/// one back, so a hallucinated or attacker-influenced path cannot enter the
/// pipeline at all -- an unrecognised `FileId` simply matches nothing.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct FileId(String);

impl FileId {
    /// Derives a stable id from a path. Stable across runs so that a rejection
    /// recorded today still matches the same file tomorrow.
    #[must_use]
    pub fn for_path(path: &std::path::Path) -> Self {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(path.as_os_str().as_encoded_bytes());
        let digest = h.finalize();
        // 8 bytes, not 4. These ids key the map that routes a batch response
        // back to its files, so a collision would hand one file's proposal to
        // another; 2^64 makes that unreachable, and 16 hex characters are still
        // short enough to sit comfortably in a prompt.
        Self(format!("f_{}", hex::encode(digest.get(..8).unwrap_or(&digest))))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for FileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The mutable facts about a file that determine whether a proposal made
/// earlier is still safe to act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileFacts {
    pub size: u64,
    pub mtime: SystemTime,
}

impl FileFacts {
    /// The file's mtime as `YYYY-MM-DD`, in UTC.
    ///
    /// This backs the `{date}` filename token. It lives here, next to the fact
    /// it reports, because the engine fills that token itself rather than
    /// asking the model for it -- see `policy::template`.
    ///
    /// A clock before the epoch yields `1970-01-01` rather than an error. A
    /// filename is not the place to surface a broken timestamp, and the
    /// alternative is sending the file to manual review over something no
    /// human reviewing it could act on.
    #[must_use]
    pub fn modified_date(&self) -> String {
        let secs = self.mtime.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let (y, m, d) = civil_from_days(i64::try_from(secs / 86_400).unwrap_or(0));
        format!("{y:04}-{m:02}-{d:02}")
    }
}

/// Howard Hinnant's `civil_from_days`, the standard branch-free conversion from
/// a days-since-epoch count to a proleptic Gregorian date. Used so that a date
/// token needs no date crate.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, u32::try_from(m).unwrap_or(1), u32::try_from(d).unwrap_or(1))
}

/// One file as observed by the scanner.
#[derive(Debug, Clone)]
pub struct FileRecord {
    pub id: FileId,
    /// Absolute path at scan time.
    pub path: PathBuf,
    /// Path relative to the profile's scan root.
    pub relative: PathBuf,
    pub facts: FileFacts,
    /// Lowercased extension without the dot, if any.
    pub extension: Option<String>,
    /// MIME type from magic bytes, when `metadata.detect_mime` is on.
    pub mime: Option<String>,
    /// First `content_sniff_bytes` of the file as lossy UTF-8, when enabled.
    pub content_snippet: Option<String>,
}

impl FileRecord {
    /// The file's current name. Always present for a scanned file.
    #[must_use]
    pub fn file_name(&self) -> &str {
        self.path.file_name().and_then(|n| n.to_str()).unwrap_or_default()
    }
}

/// A categorization proposal, exactly as the LLM emitted it. Nothing here has
/// been validated; the policy engine is what turns it into something
/// actionable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RawProposal {
    pub file_id: FileId,
    pub category: String,
    #[serde(default)]
    pub is_new_category: bool,
    #[serde(default)]
    pub name_tokens: BTreeMap<String, String>,
    pub confidence: f32,
    pub reasoning: String,
}

/// A deletion suggestion. Held to stricter rules than categorization: it can
/// never be auto-executed, at any confidence, under any configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeleteProposal {
    pub file_id: FileId,
    pub reason: String,
    pub confidence: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Proposal {
    Categorize(RawProposal),
    SuggestDelete(DeleteProposal),
}

impl Proposal {
    #[must_use]
    pub fn file_id(&self) -> &FileId {
        match self {
            Self::Categorize(p) => &p.file_id,
            Self::SuggestDelete(p) => &p.file_id,
        }
    }

    #[must_use]
    pub fn confidence(&self) -> f32 {
        match self {
            Self::Categorize(p) => p.confidence,
            Self::SuggestDelete(p) => p.confidence,
        }
    }
}

/// What came back for one file: either a usable proposal, or a description of
/// why nothing usable came back. Malformed entries are per-item, so one bad
/// entry in a batch does not sink the rest.
#[derive(Debug, Clone, PartialEq)]
pub enum ProposalOutcome {
    Ok(Proposal),
    /// Schema or parse failure that survived the retry.
    Malformed {
        detail: String,
    },
    /// The batch response contained no entry for this file at all.
    Missing,
}

impl ProposalOutcome {
    /// The model's confidence, when there is a usable proposal at all.
    ///
    /// `None` for a malformed or absent entry: those have no confidence, and
    /// reporting one would invent a number the model never gave.
    #[must_use]
    pub fn confidence(&self) -> Option<f32> {
        match self {
            Self::Ok(p) => Some(p.confidence()),
            Self::Malformed { .. } | Self::Missing => None,
        }
    }
}

/// Why a file ended up doing nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoOpReason {
    /// The file changed or vanished between scan and decision.
    Stale,
    /// An identical file (same content hash) already sits at the destination.
    DuplicateOfDestination,
    /// The file is already exactly where it would be moved to.
    AlreadyInPlace,
    /// `on_conflict = "skip"` and something different occupies the destination.
    ConflictSkipped,
    /// A run with `dry_run = true` never executes.
    DryRun,
    /// A human already refused this exact proposal, and the file has not
    /// changed since.
    PreviouslyRejected,
}

impl fmt::Display for NoOpReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Stale => "file changed or vanished since it was scanned",
            Self::DuplicateOfDestination => "an identical file already exists at the destination",
            Self::AlreadyInPlace => "file is already at its destination",
            Self::ConflictSkipped => "destination occupied and on_conflict = skip",
            Self::DryRun => "dry run",
            Self::PreviouslyRejected => "this proposal was already rejected",
        };
        f.write_str(s)
    }
}

/// The closed set of things the pipeline can actually do.
///
/// There is deliberately no variant that expresses permanent deletion, and
/// every variant that writes carries a [`DestPath`], which cannot point outside
/// a profile's destination root. Unsafe operations are therefore not merely
/// rejected at runtime -- they cannot be named.
#[derive(Debug, Clone, PartialEq)]
pub enum ResolvedAction {
    /// Move, keeping the original filename.
    Move {
        dest: DestPath,
    },
    /// Move and rename per the profile's filename template.
    MoveAndRename {
        dest: DestPath,
    },
    /// Park for a human decision: a conflict, not a deletion.
    Quarantine {
        reason: String,
        /// Where the file would have gone. `None` when the conflict arose
        /// before a destination was settled on.
        proposed: Option<DestPath>,
    },
    /// Recorded for human review. Never executed automatically, at any
    /// confidence.
    RecycleSuggested {
        reason: String,
        confidence: f32,
    },
    /// Something the engine would not decide on its own.
    NeedsManualReview {
        reason: String,
        raw: Box<ProposalOutcome>,
        /// Where the file would have gone, for the cases that got far enough to
        /// know -- in practice, a proposal held back by the confidence gate.
        /// `None` when the proposal failed earlier than path construction.
        proposed: Option<DestPath>,
    },
    NoOp {
        reason: NoOpReason,
    },
}

impl ResolvedAction {
    /// Whether this action is one the executor will perform without asking.
    #[must_use]
    pub fn is_automatic(&self) -> bool {
        matches!(self, Self::Move { .. } | Self::MoveAndRename { .. })
    }

    /// Whether this action puts a row in the review queue, which is what drives
    /// the "needs human attention" exit code.
    #[must_use]
    pub fn needs_attention(&self) -> bool {
        matches!(
            self,
            Self::Quarantine { .. }
                | Self::RecycleSuggested { .. }
                | Self::NeedsManualReview { .. }
        )
    }

    /// The destination this action *will* write to. `None` for everything the
    /// executor will not carry out on its own.
    #[must_use]
    pub fn dest(&self) -> Option<&DestPath> {
        match self {
            Self::Move { dest } | Self::MoveAndRename { dest } => Some(dest),
            _ => None,
        }
    }

    /// The destination this action *would* write to if a human approved it.
    ///
    /// Distinct from [`ResolvedAction::dest`]: a queued decision has to
    /// remember where the file was headed, or approving it days later would
    /// mean re-running the whole pipeline -- including another call to the
    /// model -- just to recover an answer the engine already computed and
    /// validated.
    #[must_use]
    pub fn proposed_dest(&self) -> Option<&DestPath> {
        match self {
            Self::Move { dest } | Self::MoveAndRename { dest } => Some(dest),
            Self::Quarantine { proposed, .. } | Self::NeedsManualReview { proposed, .. } => {
                proposed.as_ref()
            }
            _ => None,
        }
    }
}
