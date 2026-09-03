//! The SQLite state store: journal, review queue, remembered rejections, and
//! recycle store.
//!
//! One file holds all four (ADR-0001 §7), but they have different mutability
//! guarantees and the API keeps them apart:
//!
//! * [`Store::record_intent`] / [`Store::record_result`] append to the
//!   **journal**, which is never updated or deleted from.
//! * The **review queue** is mutable: rows appear when a run defers a decision
//!   and disappear when a human resolves it.
//! * **Rejections** accumulate so a resolved question is not asked again.
//! * The **recycle store** tracks files moved out of the way but not destroyed.
//!
//! Nothing here decides anything. It records what was decided elsewhere, which
//! is why the policy engine can stay pure while still benefiting from
//! remembered rejections: the caller reads them out and hands them in as data.

mod schema;

use rusqlite::{Connection, OptionalExtension, params};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("could not open the state store at {path}")]
    Open {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },
    #[error("could not create the directory for the state store at {path}")]
    Dir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "the state store was written by a newer version of bowerbird \
         (schema v{found}; this build understands v{supported})"
    )]
    FromTheFuture { found: u32, supported: u32 },
    #[error("state store operation failed")]
    Sql(#[from] rusqlite::Error),
    #[error("no review item with id {0}")]
    NoSuchReviewItem(i64),
    #[error("no recycled item with id {0}")]
    NoSuchRecycleItem(i64),
}

/// What kind of operation a journal entry describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournalAction {
    Move,
    MoveAndRename,
    /// Moved to the holding folder under `review_placement = "quarantine"`.
    Quarantine,
    /// Moved into the recycle store after a human approved a deletion.
    Recycle,
    /// Moved back out of the recycle store.
    Restore,
    /// Permanently removed. The only action that destroys anything.
    Purge,
}

impl JournalAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Move => "move",
            Self::MoveAndRename => "move_and_rename",
            Self::Quarantine => "quarantine",
            Self::Recycle => "recycle",
            Self::Restore => "restore",
            Self::Purge => "purge",
        }
    }
}

/// An operation about to be attempted.
#[derive(Debug, Clone)]
pub struct Intent<'a> {
    pub profile: &'a str,
    pub action: JournalAction,
    pub source: &'a Path,
    pub dest: Option<&'a Path>,
    /// The directory the operation writes into, recorded so the scanner can ask
    /// which directories a profile manages.
    pub dest_dir: Option<&'a Path>,
    pub file_hash: Option<&'a str>,
    /// What proposed this, and who let it through.
    pub provenance: Provenance,
}

/// What produced a proposal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// A model proposed it.
    Model,
    /// A deterministic rule matched and no model was consulted. Not yet
    /// produced by any code path; the value exists so the rule-based fast path
    /// does not require a second migration.
    Rule,
    /// A person initiated it directly, as with a restore or a purge.
    Human,
    /// Written before the journal recorded provenance. Never written by this
    /// build -- only read back from rows predating schema v2.
    Unknown,
}

/// Who allowed an operation to proceed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecidedBy {
    /// Cleared the confidence gate and executed without asking anyone.
    Auto,
    /// A person approved it through `bower review`.
    Human,
    /// Predates schema v2. See [`Origin::Unknown`].
    Unknown,
}

/// Why an operation happened, recorded alongside what happened.
///
/// Kept separate from [`JournalAction`] because they answer different
/// questions: the action is *what* the executor did, the provenance is *what
/// asked for it*. A move looks identical whether a model proposed it, a rule
/// matched it, or a person approved it, and later analysis needs to tell those
/// apart.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Provenance {
    pub origin: Origin,
    pub decided_by: DecidedBy,
    /// The proposal's confidence, when a model produced one.
    pub confidence: Option<f32>,
}

impl Provenance {
    /// A model proposed it and it cleared the confidence gate unattended.
    #[must_use]
    pub fn model_auto(confidence: Option<f32>) -> Self {
        Self { origin: Origin::Model, decided_by: DecidedBy::Auto, confidence }
    }

    /// A model proposed it and a person approved it through `bower review`.
    #[must_use]
    pub fn model_approved(confidence: Option<f32>) -> Self {
        Self { origin: Origin::Model, decided_by: DecidedBy::Human, confidence }
    }

    /// A person initiated it directly: a restore or a purge, which no model
    /// ever proposed.
    #[must_use]
    pub fn human() -> Self {
        Self { origin: Origin::Human, decided_by: DecidedBy::Human, confidence: None }
    }
}

/// One row read back from the journal.
#[derive(Debug, Clone, PartialEq)]
pub struct JournalRow {
    pub op_id: String,
    /// `intent`, `committed`, or `failed`.
    pub phase: String,
    /// Unix seconds.
    pub at: i64,
    pub profile: String,
    pub action: String,
    pub source: PathBuf,
    pub dest: Option<PathBuf>,
    pub provenance: Provenance,
    pub detail: Option<String>,
}

impl Origin {
    /// Anything unrecognised reads back as [`Origin::Unknown`]. A row written by
    /// a future release with a value this build has never heard of is still a
    /// row worth showing; refusing to read the journal because one field is
    /// unfamiliar would be worse than admitting the field is unfamiliar.
    fn from_str(s: &str) -> Self {
        match s {
            "model" => Self::Model,
            "rule" => Self::Rule,
            "human" => Self::Human,
            _ => Self::Unknown,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::Rule => "rule",
            Self::Human => "human",
            Self::Unknown => "unknown",
        }
    }
}

impl DecidedBy {
    /// See [`Origin::from_str`] for why an unrecognised value is tolerated.
    fn from_str(s: &str) -> Self {
        match s {
            "auto" => Self::Auto,
            "human" => Self::Human,
            _ => Self::Unknown,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Human => "human",
            Self::Unknown => "unknown",
        }
    }
}

/// Handle linking an intent to its outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpId(String);

impl OpId {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// How an operation ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Committed,
    Failed { detail: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewKind {
    /// The engine would not decide on its own.
    Review,
    /// A deletion suggestion. Never executed without a human.
    Recycle,
    /// Parked because the destination was occupied.
    Quarantine,
}

impl ReviewKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Review => "review",
            Self::Recycle => "recycle",
            Self::Quarantine => "quarantine",
        }
    }

    fn from_str(s: &str) -> Option<Self> {
        match s {
            "review" => Some(Self::Review),
            "recycle" => Some(Self::Recycle),
            "quarantine" => Some(Self::Quarantine),
            _ => None,
        }
    }
}

/// A pending decision, with everything needed to act on it without a re-scan.
#[derive(Debug, Clone)]
pub struct ReviewItem {
    pub id: i64,
    pub created_at: u64,
    pub profile: String,
    pub kind: ReviewKind,
    /// Where the file is now.
    pub path: PathBuf,
    /// Where it was when the proposal was made.
    pub original_path: PathBuf,
    /// Content hash at proposal time. Re-checked before anything is executed.
    pub file_hash: String,
    pub category: String,
    pub proposed_dest: Option<PathBuf>,
    pub reasoning: String,
    pub confidence: Option<f32>,
    pub reason: String,
}

/// A new row for the review queue.
#[derive(Debug, Clone)]
pub struct NewReviewItem<'a> {
    pub profile: &'a str,
    pub kind: ReviewKind,
    pub path: &'a Path,
    pub original_path: &'a Path,
    pub file_hash: &'a str,
    pub category: &'a str,
    pub proposed_dest: Option<&'a Path>,
    pub reasoning: &'a str,
    pub confidence: Option<f32>,
    pub reason: &'a str,
}

#[derive(Debug, Clone)]
pub struct RecycleItem {
    pub id: i64,
    pub recycled_at: u64,
    pub profile: String,
    pub original_path: PathBuf,
    pub stored_path: PathBuf,
    pub file_hash: String,
    pub reason: String,
}

/// Everything a run needs to know about what this profile's user has already
/// said no to.
///
/// Handed to the policy engine as plain data, so the engine can honour
/// rejections without acquiring a database connection or losing its purity.
#[derive(Debug, Default, Clone)]
pub struct RejectionIndex {
    entries: HashSet<(String, &'static str, String)>,
    sizes: HashSet<u64>,
}

impl RejectionIndex {
    /// Whether any rejection exists at all. A profile with none can skip
    /// hashing files entirely.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Whether a file of this size could possibly match a rejection.
    ///
    /// Size is free -- the scanner already has it -- and hashing is not, so
    /// this prunes almost every file before anything is read.
    #[must_use]
    pub fn might_match_size(&self, size: u64) -> bool {
        self.sizes.contains(&size)
    }

    /// Whether this exact proposal was already rejected. `category` is empty
    /// for deletion suggestions, which carry no category.
    #[must_use]
    pub fn contains(&self, file_hash: &str, kind: ReviewKind, category: &str) -> bool {
        self.entries.contains(&(file_hash.to_owned(), kind.as_str(), category.to_owned()))
    }

    /// Every category rejected for this file hash, which is what the policy
    /// engine checks after it has resolved a category of its own.
    #[must_use]
    pub fn rejected_categories(&self, file_hash: &str) -> Vec<String> {
        self.entries
            .iter()
            .filter(|(hash, kind, _)| hash == file_hash && *kind == ReviewKind::Review.as_str())
            .map(|(_, _, category)| category.clone())
            .collect()
    }
}

/// Where the executor reports what it is about to do, and what happened.
///
/// A trait rather than a concrete [`Store`] so the executor keeps its own
/// before/after discipline (ADR-0001 §2) while staying testable without a
/// database, and so a dry run can be handed a sink that records nothing.
pub trait JournalSink {
    fn record_intent(&self, intent: &Intent<'_>) -> Result<OpId, StateError>;
    fn record_result(
        &self,
        op: &OpId,
        intent: &Intent<'_>,
        outcome: &Outcome,
    ) -> Result<(), StateError>;
}

impl JournalSink for Store {
    fn record_intent(&self, intent: &Intent<'_>) -> Result<OpId, StateError> {
        Self::record_intent(self, intent)
    }

    fn record_result(
        &self,
        op: &OpId,
        intent: &Intent<'_>,
        outcome: &Outcome,
    ) -> Result<(), StateError> {
        Self::record_result(self, op, intent, outcome)
    }
}

/// A sink that discards everything, for dry runs and for tests of the move
/// mechanics themselves.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoJournal;

impl JournalSink for NoJournal {
    fn record_intent(&self, _intent: &Intent<'_>) -> Result<OpId, StateError> {
        Ok(OpId(String::from("dry-run")))
    }

    fn record_result(
        &self,
        _op: &OpId,
        _intent: &Intent<'_>,
        _outcome: &Outcome,
    ) -> Result<(), StateError> {
        Ok(())
    }
}

/// The state store.
#[derive(Debug)]
pub struct Store {
    conn: Connection,
}

impl Store {
    /// Opens (creating if necessary) the state file, applying any missing
    /// migrations.
    pub fn open(path: &Path) -> Result<Self, StateError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|source| StateError::Dir { path: parent.to_path_buf(), source })?;
        }
        let conn = Connection::open(path)
            .map_err(|source| StateError::Open { path: path.to_path_buf(), source })?;
        Self::prepare(conn)
    }

    /// An ephemeral store, for tests.
    pub fn open_in_memory() -> Result<Self, StateError> {
        Self::prepare(Connection::open_in_memory()?)
    }

    fn prepare(conn: Connection) -> Result<Self, StateError> {
        // WAL keeps a long-running review session from blocking a cron run, and
        // foreign_keys is off by default in SQLite for backwards compatibility.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", true)?;
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        let mut store = Self { conn };
        schema::migrate(&mut store.conn)?;
        Ok(store)
    }

    /// The schema version currently applied to this file.
    pub fn schema_version(&self) -> Result<u32, StateError> {
        Ok(self.conn.query_row("PRAGMA user_version", [], |row| row.get(0))?)
    }

    // -- journal ------------------------------------------------------------

    /// Records that an operation is about to be attempted, before the
    /// filesystem is touched.
    pub fn record_intent(&self, intent: &Intent<'_>) -> Result<OpId, StateError> {
        let op = next_op_id();
        self.conn.execute(
            "INSERT INTO journal
               (op_id, phase, at, profile, action, source, dest, dest_dir, file_hash,
                origin, decided_by, confidence)
             VALUES (?1, 'intent', ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                op.as_str(),
                now_secs(),
                intent.profile,
                intent.action.as_str(),
                path_str(intent.source),
                intent.dest.map(path_str),
                intent.dest_dir.map(path_str),
                intent.file_hash,
                intent.provenance.origin.as_str(),
                intent.provenance.decided_by.as_str(),
                intent.provenance.confidence,
            ],
        )?;
        Ok(op)
    }

    /// Records how the operation ended. Appends a second row rather than
    /// editing the first: the journal is never updated in place.
    pub fn record_result(
        &self,
        op: &OpId,
        intent: &Intent<'_>,
        outcome: &Outcome,
    ) -> Result<(), StateError> {
        let (phase, detail) = match outcome {
            Outcome::Committed => ("committed", None),
            Outcome::Failed { detail } => ("failed", Some(detail.as_str())),
        };
        self.conn.execute(
            "INSERT INTO journal
               (op_id, phase, at, profile, action, source, dest, dest_dir, file_hash, detail,
                origin, decided_by, confidence)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                op.as_str(),
                phase,
                now_secs(),
                intent.profile,
                intent.action.as_str(),
                path_str(intent.source),
                intent.dest.map(path_str),
                intent.dest_dir.map(path_str),
                intent.file_hash,
                detail,
                intent.provenance.origin.as_str(),
                intent.provenance.decided_by.as_str(),
                intent.provenance.confidence,
            ],
        )?;
        Ok(())
    }

    /// Operations that recorded an intent but never a result -- a crash
    /// part-way through a move.
    pub fn unfinished_operations(&self) -> Result<Vec<String>, StateError> {
        let mut stmt = self.conn.prepare(
            "SELECT op_id FROM journal WHERE phase = 'intent'
             AND op_id NOT IN (SELECT op_id FROM journal WHERE phase IN ('committed','failed'))",
        )?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Directories this profile has actually written into.
    ///
    /// This is what closes the gap ADR-0002 §1 left open: an in-place profile
    /// with `allow_dynamic_categories = true` creates category directories the
    /// config never names, and without this they would be re-scanned as fresh
    /// input on the next run.
    pub fn managed_dirs(&self, profile: &str) -> Result<Vec<PathBuf>, StateError> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT dest_dir FROM journal
             WHERE profile = ?1 AND phase = 'committed' AND dest_dir IS NOT NULL",
        )?;
        let rows = stmt.query_map(params![profile], |row| row.get::<_, String>(0))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?.into_iter().map(PathBuf::from).collect())
    }

    // -- review queue -------------------------------------------------------

    /// Adds a pending decision. Returns `None` when an identical row is already
    /// queued, so a repeated cron run does not pile up duplicates.
    pub fn enqueue_review(&self, item: &NewReviewItem<'_>) -> Result<Option<i64>, StateError> {
        let changed = self.conn.execute(
            "INSERT OR IGNORE INTO review_queue
               (created_at, profile, kind, path, original_path, file_hash,
                category, proposed_dest, reasoning, confidence, reason)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                now_secs(),
                item.profile,
                item.kind.as_str(),
                path_str(item.path),
                path_str(item.original_path),
                item.file_hash,
                item.category,
                item.proposed_dest.map(path_str),
                item.reasoning,
                item.confidence,
                item.reason,
            ],
        )?;
        Ok((changed > 0).then(|| self.conn.last_insert_rowid()))
    }

    pub fn review_list(
        &self,
        profile: Option<&str>,
        kind: Option<ReviewKind>,
    ) -> Result<Vec<ReviewItem>, StateError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, created_at, profile, kind, path, original_path, file_hash,
                    category, proposed_dest, reasoning, confidence, reason
             FROM review_queue
             WHERE (?1 IS NULL OR profile = ?1) AND (?2 IS NULL OR kind = ?2)
             ORDER BY created_at, id",
        )?;
        let rows =
            stmt.query_map(params![profile, kind.map(ReviewKind::as_str)], review_from_row)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Journal rows, newest first.
    ///
    /// The journal is the record of what the tool actually did and why. Reading
    /// it back is what makes provenance more than a write-only column: it is
    /// how "which of these did a model choose, and how sure was it?" gets
    /// answered.
    pub fn journal_recent(
        &self,
        profile: Option<&str>,
        limit: usize,
    ) -> Result<Vec<JournalRow>, StateError> {
        let mut stmt = self.conn.prepare(
            "SELECT op_id, phase, at, profile, action, source, dest,
                    origin, decided_by, confidence, detail
             FROM journal
             WHERE (?1 IS NULL OR profile = ?1)
             ORDER BY at DESC, id DESC
             LIMIT ?2",
        )?;
        let rows =
            stmt.query_map(params![profile, i64::try_from(limit).unwrap_or(i64::MAX)], |row| {
                Ok(JournalRow {
                    op_id: row.get(0)?,
                    phase: row.get(1)?,
                    at: row.get(2)?,
                    profile: row.get(3)?,
                    action: row.get(4)?,
                    source: PathBuf::from(row.get::<_, String>(5)?),
                    dest: row.get::<_, Option<String>>(6)?.map(PathBuf::from),
                    provenance: Provenance {
                        origin: Origin::from_str(&row.get::<_, String>(7)?),
                        decided_by: DecidedBy::from_str(&row.get::<_, String>(8)?),
                        confidence: row.get(9)?,
                    },
                    detail: row.get(10)?,
                })
            })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn review_get(&self, id: i64) -> Result<Option<ReviewItem>, StateError> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, created_at, profile, kind, path, original_path, file_hash,
                        category, proposed_dest, reasoning, confidence, reason
                 FROM review_queue WHERE id = ?1",
                params![id],
                review_from_row,
            )
            .optional()?)
    }

    pub fn review_remove(&self, id: i64) -> Result<(), StateError> {
        let changed = self.conn.execute("DELETE FROM review_queue WHERE id = ?1", params![id])?;
        if changed == 0 {
            return Err(StateError::NoSuchReviewItem(id));
        }
        Ok(())
    }

    /// Drops a queued row whose file no longer matches, without recording a
    /// rejection -- the question became moot rather than being answered.
    pub fn review_discard_stale(&self, id: i64) -> Result<(), StateError> {
        self.conn.execute("DELETE FROM review_queue WHERE id = ?1", params![id])?;
        Ok(())
    }

    // -- rejections ---------------------------------------------------------

    /// Remembers that this proposal was refused.
    pub fn remember_rejection(
        &self,
        profile: &str,
        kind: ReviewKind,
        file_hash: &str,
        file_size: u64,
        category: &str,
        reason: Option<&str>,
    ) -> Result<(), StateError> {
        self.conn.execute(
            "INSERT OR REPLACE INTO rejections
               (rejected_at, profile, kind, file_hash, file_size, category, reason)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                now_secs(),
                profile,
                kind.as_str(),
                file_hash,
                i64::try_from(file_size).unwrap_or(i64::MAX),
                category,
                reason
            ],
        )?;
        Ok(())
    }

    /// Everything this profile's user has already refused.
    pub fn rejections_for(&self, profile: &str) -> Result<RejectionIndex, StateError> {
        let mut stmt = self.conn.prepare(
            "SELECT file_hash, kind, category, file_size FROM rejections WHERE profile = ?1",
        )?;
        let rows = stmt.query_map(params![profile], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?;

        let mut index = RejectionIndex::default();
        for row in rows {
            let (hash, kind, category, size) = row?;
            let Some(kind) = ReviewKind::from_str(&kind) else { continue };
            index.entries.insert((hash, kind.as_str(), category));
            index.sizes.insert(u64::try_from(size).unwrap_or(0));
        }
        Ok(index)
    }

    // -- recycle store ------------------------------------------------------

    pub fn record_recycled(
        &self,
        profile: &str,
        original_path: &Path,
        stored_path: &Path,
        file_hash: &str,
        reason: &str,
    ) -> Result<i64, StateError> {
        self.conn.execute(
            "INSERT INTO recycle
               (recycled_at, profile, original_path, stored_path, file_hash, reason)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                now_secs(),
                profile,
                path_str(original_path),
                path_str(stored_path),
                file_hash,
                reason
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn recycle_list(&self) -> Result<Vec<RecycleItem>, StateError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, recycled_at, profile, original_path, stored_path, file_hash, reason
             FROM recycle ORDER BY recycled_at, id",
        )?;
        let rows = stmt.query_map([], recycle_from_row)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn recycle_get(&self, id: i64) -> Result<Option<RecycleItem>, StateError> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, recycled_at, profile, original_path, stored_path, file_hash, reason
                 FROM recycle WHERE id = ?1",
                params![id],
                recycle_from_row,
            )
            .optional()?)
    }

    /// Items recycled before `cutoff` (unix seconds) -- the candidates for a
    /// purge.
    pub fn recycle_older_than(&self, cutoff: u64) -> Result<Vec<RecycleItem>, StateError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, recycled_at, profile, original_path, stored_path, file_hash, reason
             FROM recycle WHERE recycled_at < ?1 ORDER BY recycled_at, id",
        )?;
        let rows =
            stmt.query_map(params![i64::try_from(cutoff).unwrap_or(i64::MAX)], recycle_from_row)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn recycle_remove(&self, id: i64) -> Result<(), StateError> {
        let changed = self.conn.execute("DELETE FROM recycle WHERE id = ?1", params![id])?;
        if changed == 0 {
            return Err(StateError::NoSuchRecycleItem(id));
        }
        Ok(())
    }
}

fn review_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ReviewItem> {
    let kind: String = row.get(3)?;
    Ok(ReviewItem {
        id: row.get(0)?,
        created_at: u64::try_from(row.get::<_, i64>(1)?).unwrap_or(0),
        profile: row.get(2)?,
        kind: ReviewKind::from_str(&kind).unwrap_or(ReviewKind::Review),
        path: PathBuf::from(row.get::<_, String>(4)?),
        original_path: PathBuf::from(row.get::<_, String>(5)?),
        file_hash: row.get(6)?,
        category: row.get(7)?,
        proposed_dest: row.get::<_, Option<String>>(8)?.map(PathBuf::from),
        reasoning: row.get(9)?,
        confidence: row.get::<_, Option<f32>>(10)?,
        reason: row.get(11)?,
    })
}

fn recycle_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RecycleItem> {
    Ok(RecycleItem {
        id: row.get(0)?,
        recycled_at: u64::try_from(row.get::<_, i64>(1)?).unwrap_or(0),
        profile: row.get(2)?,
        original_path: PathBuf::from(row.get::<_, String>(3)?),
        stored_path: PathBuf::from(row.get::<_, String>(4)?),
        file_hash: row.get(5)?,
        reason: row.get(6)?,
    })
}

/// Paths are stored as their lossy string form. Bowerbird only ever constructs
/// destination paths from validated components, and refuses to act on a source
/// path it cannot round-trip, so this cannot silently mangle a path it will
/// later act on.
fn path_str(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// Unique within a process; the timestamp and pid keep it unique across them.
fn next_op_id() -> OpId {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    OpId(format!("{}-{nanos}-{n}", std::process::id()))
}
