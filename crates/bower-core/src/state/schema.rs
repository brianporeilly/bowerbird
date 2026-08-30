//! Schema definition and forward migrations.
//!
//! The state file outlives any single release: a review queue can sit unactioned
//! for weeks, and the journal is meant to be a permanent record. So the schema
//! is versioned from the first release rather than from the first time it needs
//! to change.
//!
//! `PRAGMA user_version` holds the applied version. Each migration runs inside a
//! transaction, so a failure part-way leaves the file at its previous version
//! rather than half-migrated.

use rusqlite::{Connection, Transaction};

use super::StateError;

/// Migrations, applied in order. Append only -- never edit or renumber one that
/// has shipped, because a user's file may already have it applied.
const MIGRATIONS: &[&str] = &[
    // v1: journal, review queue, remembered rejections, recycle store.
    r#"
    -- Append-only. Two rows per operation: an `intent` written before the
    -- filesystem is touched, and a `committed` or `failed` row written after.
    -- An intent with no matching result is a crash mid-operation, which is
    -- precisely what the before-write exists to make visible.
    CREATE TABLE journal (
        id          INTEGER PRIMARY KEY,
        op_id       TEXT    NOT NULL,
        phase       TEXT    NOT NULL CHECK (phase IN ('intent','committed','failed')),
        at          INTEGER NOT NULL,
        profile     TEXT    NOT NULL,
        action      TEXT    NOT NULL,
        source      TEXT    NOT NULL,
        dest        TEXT,
        -- The directory the operation wrote into. Stored explicitly so the
        -- scanner can ask which directories a profile manages without parsing
        -- paths at query time.
        dest_dir    TEXT,
        file_hash   TEXT,
        detail      TEXT
    );
    CREATE INDEX journal_op    ON journal (op_id);
    CREATE INDEX journal_prof  ON journal (profile, phase);
    CREATE INDEX journal_at    ON journal (at);

    -- Mutable. One row per pending human decision. Carries enough context to
    -- act without a re-scan.
    CREATE TABLE review_queue (
        id             INTEGER PRIMARY KEY,
        created_at     INTEGER NOT NULL,
        profile        TEXT    NOT NULL,
        kind           TEXT    NOT NULL CHECK (kind IN ('review','recycle','quarantine')),
        -- Where the file is now. Differs from original_path when
        -- review_placement = "quarantine" moved it to a holding folder.
        path           TEXT    NOT NULL,
        original_path  TEXT    NOT NULL,
        file_hash      TEXT    NOT NULL,
        category       TEXT    NOT NULL DEFAULT '',
        proposed_dest  TEXT,
        reasoning      TEXT    NOT NULL DEFAULT '',
        confidence     REAL,
        reason         TEXT    NOT NULL DEFAULT ''
    );
    -- A repeated cron run must not pile up duplicate rows for a file nobody has
    -- got to yet.
    CREATE UNIQUE INDEX review_queue_identity
        ON review_queue (profile, original_path, file_hash, kind);

    -- Remembered rejections, so an identical proposal is not re-surfaced on the
    -- next run unless the file itself changes. `file_size` is a cheap prefilter:
    -- it lets a run skip hashing any file whose size matches no rejection.
    CREATE TABLE rejections (
        id           INTEGER PRIMARY KEY,
        rejected_at  INTEGER NOT NULL,
        profile      TEXT    NOT NULL,
        kind         TEXT    NOT NULL,
        file_hash    TEXT    NOT NULL,
        file_size    INTEGER NOT NULL DEFAULT 0,
        -- Empty string rather than NULL: SQLite treats NULLs as distinct in a
        -- UNIQUE index, which would defeat the deduplication below.
        category     TEXT    NOT NULL DEFAULT '',
        reason       TEXT
    );
    CREATE UNIQUE INDEX rejections_identity
        ON rejections (profile, file_hash, kind, category);
    CREATE INDEX rejections_size ON rejections (profile, file_size);

    -- Files moved to the recycle store. `original_path` is what makes restore a
    -- reverse move; the stored layout does not have to mirror it.
    CREATE TABLE recycle (
        id             INTEGER PRIMARY KEY,
        recycled_at    INTEGER NOT NULL,
        profile        TEXT    NOT NULL,
        original_path  TEXT    NOT NULL,
        stored_path    TEXT    NOT NULL UNIQUE,
        file_hash      TEXT    NOT NULL,
        reason         TEXT    NOT NULL DEFAULT ''
    );
    CREATE INDEX recycle_at ON recycle (recycled_at);
    "#,
    // v2: proposal provenance on the journal.
    //
    // The journal is append-only, so a fact not recorded when a row is written
    // can never be recovered later. A migration can add a column; it cannot
    // reconstruct history that was never captured.
    //
    // These record distinctions that already exist today -- an automatic run
    // versus an approved review item -- and that two planned features depend on:
    // a rule-based fast path introduces a second thing that produces proposals,
    // and learning from corrections requires knowing whether a person overrode
    // a *model* or a *rule*.
    //
    // Rows written before this migration are marked 'unknown' rather than
    // guessed at. Backfilling a plausible value would put a fabricated fact into
    // the one table whose entire purpose is to be trustworthy.
    r"
    ALTER TABLE journal ADD COLUMN origin TEXT NOT NULL DEFAULT 'unknown'
        CHECK (origin IN ('model','rule','human','unknown'));
    ALTER TABLE journal ADD COLUMN decided_by TEXT NOT NULL DEFAULT 'unknown'
        CHECK (decided_by IN ('auto','human','unknown'));
    ALTER TABLE journal ADD COLUMN confidence REAL;
    CREATE INDEX journal_origin ON journal (profile, origin);
    ",
];

/// The schema version this build writes.
pub(super) fn current_version() -> u32 {
    u32::try_from(MIGRATIONS.len()).unwrap_or(u32::MAX)
}

/// Brings `conn` up to [`current_version`], applying only the migrations it is
/// missing.
pub(super) fn migrate(conn: &mut Connection) -> Result<(), StateError> {
    let applied: u32 =
        conn.query_row("PRAGMA user_version", [], |row| row.get(0)).map_err(StateError::Sql)?;

    let current = current_version();
    if applied > current {
        return Err(StateError::FromTheFuture { found: applied, supported: current });
    }

    for (index, sql) in MIGRATIONS.iter().enumerate() {
        let version = u32::try_from(index).unwrap_or(u32::MAX).saturating_add(1);
        if version <= applied {
            continue;
        }
        let tx: Transaction<'_> = conn.transaction().map_err(StateError::Sql)?;
        tx.execute_batch(sql).map_err(StateError::Sql)?;
        // `PRAGMA user_version` does not accept a bound parameter.
        tx.pragma_update(None, "user_version", version).map_err(StateError::Sql)?;
        tx.commit().map_err(StateError::Sql)?;
    }

    Ok(())
}
