#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::duration_suboptimal_units
)]
//! The SQLite state store: journal, review queue, rejections, recycle.

use bower_core::state::{
    DecidedBy, Intent, JournalAction, NewReviewItem, Origin, Outcome, Provenance, ReviewKind,
    StateError, Store,
};
use std::path::Path;

/// The schema version this build writes. Bump deliberately, with the migration.
const CURRENT_SCHEMA: u32 = 2;

fn store() -> Store {
    Store::open_in_memory().unwrap()
}

fn intent<'a>(profile: &'a str, source: &'a Path, dest: &'a Path, dir: &'a Path) -> Intent<'a> {
    Intent {
        profile,
        action: JournalAction::Move,
        source,
        dest: Some(dest),
        dest_dir: Some(dir),
        file_hash: Some("abc123"),
        provenance: Provenance::model_auto(Some(0.87)),
    }
}

// --- schema and migrations --------------------------------------------------

#[test]
fn a_fresh_store_is_migrated_to_the_current_version() {
    // A literal on purpose: adding a migration should require deliberately
    // updating this, so a schema change can never happen by accident.
    assert_eq!(store().schema_version().unwrap(), CURRENT_SCHEMA);
}

#[test]
fn reopening_an_existing_store_is_a_no_op() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");

    let first = Store::open(&path).unwrap();
    first.remember_rejection("p", ReviewKind::Review, "hash", 10, "Documents", None).unwrap();
    drop(first);

    let second = Store::open(&path).unwrap();
    assert_eq!(second.schema_version().unwrap(), CURRENT_SCHEMA);
    assert!(!second.rejections_for("p").unwrap().is_empty(), "data must survive a reopen");
}

#[test]
fn the_parent_directory_is_created_if_missing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nested/deeper/state.db");
    Store::open(&path).unwrap();
    assert!(path.exists());
}

#[test]
fn a_store_from_a_newer_release_is_refused_rather_than_downgraded() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    {
        let s = Store::open(&path).unwrap();
        drop(s);
    }
    // Simulate a file written by a future build.
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.pragma_update(None, "user_version", 99u32).unwrap();
    drop(conn);

    match Store::open(&path) {
        Err(StateError::FromTheFuture { found, supported }) => {
            assert_eq!(found, 99);
            assert_eq!(supported, CURRENT_SCHEMA);
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

// --- journal ----------------------------------------------------------------

#[test]
fn an_operation_writes_an_intent_before_and_a_result_after() {
    let s = store();
    let i = intent(
        "downloads",
        Path::new("/src/a.pdf"),
        Path::new("/d/Docs/a.pdf"),
        Path::new("/d/Docs"),
    );

    let op = s.record_intent(&i).unwrap();
    assert_eq!(s.unfinished_operations().unwrap(), vec![op.as_str().to_owned()]);

    s.record_result(&op, &i, &Outcome::Committed).unwrap();
    assert!(
        s.unfinished_operations().unwrap().is_empty(),
        "an operation with a result is finished"
    );
}

#[test]
fn a_crash_between_intent_and_result_stays_visible() {
    let s = store();
    let i = intent(
        "downloads",
        Path::new("/src/a.pdf"),
        Path::new("/d/Docs/a.pdf"),
        Path::new("/d/Docs"),
    );
    let op = s.record_intent(&i).unwrap();

    // No result recorded: the process died mid-move.
    let unfinished = s.unfinished_operations().unwrap();
    assert_eq!(unfinished, vec![op.as_str().to_owned()]);
}

#[test]
fn a_failed_operation_is_finished_but_not_committed() {
    let s = store();
    let i = intent(
        "downloads",
        Path::new("/src/a.pdf"),
        Path::new("/d/Docs/a.pdf"),
        Path::new("/d/Docs"),
    );
    let op = s.record_intent(&i).unwrap();
    s.record_result(&op, &i, &Outcome::Failed { detail: "occupied".to_owned() }).unwrap();

    assert!(s.unfinished_operations().unwrap().is_empty());
    assert!(
        s.managed_dirs("downloads").unwrap().is_empty(),
        "a directory only counts as managed once something actually landed in it"
    );
}

#[test]
fn managed_dirs_reports_only_directories_actually_written_into() {
    let s = store();

    let committed = intent(
        "downloads",
        Path::new("/src/a.pdf"),
        Path::new("/d/Invoices/a.pdf"),
        Path::new("/d/Invoices"),
    );
    let op = s.record_intent(&committed).unwrap();
    s.record_result(&op, &committed, &Outcome::Committed).unwrap();

    // An intent that never completed must not count.
    let pending = intent(
        "downloads",
        Path::new("/src/b.pdf"),
        Path::new("/d/Ghost/b.pdf"),
        Path::new("/d/Ghost"),
    );
    s.record_intent(&pending).unwrap();

    // Another profile's output is not this profile's business.
    let other =
        intent("docs", Path::new("/src/c.pdf"), Path::new("/e/Tax/c.pdf"), Path::new("/e/Tax"));
    let op = s.record_intent(&other).unwrap();
    s.record_result(&op, &other, &Outcome::Committed).unwrap();

    assert_eq!(s.managed_dirs("downloads").unwrap(), [Path::new("/d/Invoices")]);
}

#[test]
fn repeated_moves_into_one_directory_report_it_once() {
    let s = store();
    for name in ["a.pdf", "b.pdf", "c.pdf"] {
        let dest = Path::new("/d/Invoices").join(name);
        let i = intent("downloads", Path::new("/src/x"), &dest, Path::new("/d/Invoices"));
        let op = s.record_intent(&i).unwrap();
        s.record_result(&op, &i, &Outcome::Committed).unwrap();
    }
    assert_eq!(s.managed_dirs("downloads").unwrap().len(), 1);
}

// --- review queue -----------------------------------------------------------

fn new_item<'a>(
    profile: &'a str,
    kind: ReviewKind,
    path: &'a Path,
    hash: &'a str,
) -> NewReviewItem<'a> {
    NewReviewItem {
        profile,
        kind,
        path,
        original_path: path,
        file_hash: hash,
        category: "Documents",
        proposed_dest: None,
        reasoning: "because",
        confidence: Some(0.5),
        reason: "below threshold",
    }
}

#[test]
fn a_queued_decision_round_trips_with_everything_needed_to_act_on_it() {
    let s = store();
    let path = Path::new("/data/downloads/a.pdf");
    let id = s
        .enqueue_review(&NewReviewItem {
            proposed_dest: Some(Path::new("/data/organized/Documents/a.pdf")),
            ..new_item("downloads", ReviewKind::Review, path, "hash-a")
        })
        .unwrap()
        .expect("a new row");

    let item = s.review_get(id).unwrap().expect("present");
    assert_eq!(item.profile, "downloads");
    assert_eq!(item.kind, ReviewKind::Review);
    assert_eq!(item.path, path);
    assert_eq!(item.file_hash, "hash-a");
    assert_eq!(item.category, "Documents");
    assert_eq!(item.proposed_dest.as_deref(), Some(Path::new("/data/organized/Documents/a.pdf")));
    assert_eq!(item.confidence, Some(0.5));
    assert!(item.created_at > 0);
}

#[test]
fn a_repeated_run_does_not_pile_up_duplicate_rows() {
    let s = store();
    let path = Path::new("/data/downloads/a.pdf");
    let item = new_item("downloads", ReviewKind::Review, path, "hash-a");

    assert!(s.enqueue_review(&item).unwrap().is_some(), "first run queues it");
    assert!(s.enqueue_review(&item).unwrap().is_none(), "second run must not re-queue it");
    assert_eq!(s.review_list(None, None).unwrap().len(), 1);
}

#[test]
fn a_changed_file_queues_separately_from_the_version_already_pending() {
    let s = store();
    let path = Path::new("/data/downloads/a.pdf");
    s.enqueue_review(&new_item("downloads", ReviewKind::Review, path, "hash-a")).unwrap();
    s.enqueue_review(&new_item("downloads", ReviewKind::Review, path, "hash-b")).unwrap();
    assert_eq!(s.review_list(None, None).unwrap().len(), 2);
}

#[test]
fn the_queue_can_be_filtered_by_profile_and_by_kind() {
    let s = store();
    s.enqueue_review(&new_item("downloads", ReviewKind::Review, Path::new("/a"), "h1")).unwrap();
    s.enqueue_review(&new_item("downloads", ReviewKind::Recycle, Path::new("/b"), "h2")).unwrap();
    s.enqueue_review(&new_item("docs", ReviewKind::Review, Path::new("/c"), "h3")).unwrap();

    assert_eq!(s.review_list(None, None).unwrap().len(), 3);
    assert_eq!(s.review_list(Some("downloads"), None).unwrap().len(), 2);
    assert_eq!(s.review_list(None, Some(ReviewKind::Recycle)).unwrap().len(), 1);
    assert_eq!(s.review_list(Some("downloads"), Some(ReviewKind::Review)).unwrap().len(), 1);
}

#[test]
fn removing_an_item_that_is_not_there_is_an_error() {
    let s = store();
    assert!(matches!(s.review_remove(404), Err(StateError::NoSuchReviewItem(404))));
}

// --- rejections -------------------------------------------------------------

#[test]
fn a_rejection_is_remembered_and_matched_on_hash_and_category() {
    let s = store();
    s.remember_rejection("downloads", ReviewKind::Review, "hash-a", 4096, "Documents", Some("no"))
        .unwrap();

    let index = s.rejections_for("downloads").unwrap();
    assert!(index.contains("hash-a", ReviewKind::Review, "Documents"));
    assert!(
        !index.contains("hash-a", ReviewKind::Review, "Images"),
        "a different category is a different question"
    );
    assert!(!index.contains("hash-b", ReviewKind::Review, "Documents"));
}

#[test]
fn rejections_are_scoped_to_their_profile() {
    let s = store();
    s.remember_rejection("downloads", ReviewKind::Review, "hash-a", 10, "Documents", None).unwrap();
    assert!(s.rejections_for("docs").unwrap().is_empty(), "one profile's answer is not another's");
}

#[test]
fn the_size_prefilter_admits_matching_sizes_and_rejects_others() {
    let s = store();
    s.remember_rejection("downloads", ReviewKind::Review, "hash-a", 4096, "Documents", None)
        .unwrap();

    let index = s.rejections_for("downloads").unwrap();
    assert!(index.might_match_size(4096), "a file of the rejected size must be hashed");
    assert!(!index.might_match_size(1234), "any other size can skip hashing entirely");
}

#[test]
fn re_rejecting_the_same_proposal_does_not_duplicate_it() {
    let s = store();
    for _ in 0..3 {
        s.remember_rejection("downloads", ReviewKind::Review, "hash-a", 10, "Documents", None)
            .unwrap();
    }
    assert_eq!(s.rejections_for("downloads").unwrap().rejected_categories("hash-a").len(), 1);
}

#[test]
fn a_deletion_rejection_carries_no_category_and_stays_distinct() {
    let s = store();
    s.remember_rejection("downloads", ReviewKind::Recycle, "hash-a", 10, "", None).unwrap();
    s.remember_rejection("downloads", ReviewKind::Review, "hash-a", 10, "Documents", None).unwrap();

    let index = s.rejections_for("downloads").unwrap();
    assert!(index.contains("hash-a", ReviewKind::Recycle, ""));
    assert!(index.contains("hash-a", ReviewKind::Review, "Documents"));
    assert_eq!(
        index.rejected_categories("hash-a"),
        ["Documents"],
        "a refused deletion is not a refused category"
    );
}

#[test]
fn an_empty_index_lets_a_run_skip_hashing_altogether() {
    let s = store();
    let index = s.rejections_for("downloads").unwrap();
    assert!(index.is_empty());
    assert!(!index.might_match_size(4096));
}

// --- recycle store ----------------------------------------------------------

#[test]
fn a_recycled_file_records_where_it_came_from() {
    let s = store();
    let id = s
        .record_recycled(
            "downloads",
            Path::new("/data/downloads/old.iso"),
            Path::new("/data/_recycled/downloads/old.iso"),
            "hash-a",
            "duplicate installer",
        )
        .unwrap();

    let item = s.recycle_get(id).unwrap().expect("present");
    assert_eq!(item.original_path, Path::new("/data/downloads/old.iso"));
    assert_eq!(item.stored_path, Path::new("/data/_recycled/downloads/old.iso"));
    assert_eq!(item.reason, "duplicate installer");
    assert!(item.recycled_at > 0);
}

#[test]
fn purge_candidates_are_selected_by_age() {
    let s = store();
    let id = s.record_recycled("p", Path::new("/a"), Path::new("/r/a"), "h", "reason").unwrap();

    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();

    assert!(s.recycle_older_than(now.saturating_sub(3600)).unwrap().is_empty(), "too new to purge");
    let due = s.recycle_older_than(now + 3600).unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].id, id);
}

#[test]
fn removing_a_recycled_item_that_is_not_there_is_an_error() {
    let s = store();
    assert!(matches!(s.recycle_remove(404), Err(StateError::NoSuchRecycleItem(404))));
}

#[test]
fn the_same_stored_path_cannot_be_claimed_twice() {
    let s = store();
    s.record_recycled("p", Path::new("/a"), Path::new("/r/a"), "h", "").unwrap();
    assert!(
        s.record_recycled("p", Path::new("/b"), Path::new("/r/a"), "h", "").is_err(),
        "two files must never map onto one slot in the recycle store"
    );
}

// --- provenance (schema v2) -------------------------------------------------

/// A file written by a v1 build, complete with a journal row. Used to prove the
/// migration is applied to real data rather than only to empty files.
fn write_v1_store(path: &std::path::Path) {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch(
        "CREATE TABLE journal (
            id          INTEGER PRIMARY KEY,
            op_id       TEXT    NOT NULL,
            phase       TEXT    NOT NULL CHECK (phase IN ('intent','committed','failed')),
            at          INTEGER NOT NULL,
            profile     TEXT    NOT NULL,
            action      TEXT    NOT NULL,
            source      TEXT    NOT NULL,
            dest        TEXT,
            dest_dir    TEXT,
            file_hash   TEXT,
            detail      TEXT
        );
        CREATE TABLE review_queue (
            id INTEGER PRIMARY KEY, created_at INTEGER NOT NULL, profile TEXT NOT NULL,
            kind TEXT NOT NULL CHECK (kind IN ('review','recycle','quarantine')),
            path TEXT NOT NULL, original_path TEXT NOT NULL, file_hash TEXT NOT NULL,
            category TEXT NOT NULL DEFAULT '', proposed_dest TEXT,
            reasoning TEXT NOT NULL DEFAULT '', confidence REAL,
            reason TEXT NOT NULL DEFAULT ''
        );
        CREATE UNIQUE INDEX review_queue_identity
            ON review_queue (profile, original_path, file_hash, kind);
        CREATE TABLE rejections (
            id INTEGER PRIMARY KEY, rejected_at INTEGER NOT NULL, profile TEXT NOT NULL,
            kind TEXT NOT NULL, file_hash TEXT NOT NULL,
            file_size INTEGER NOT NULL DEFAULT 0,
            category TEXT NOT NULL DEFAULT '', reason TEXT
        );
        CREATE UNIQUE INDEX rejections_identity
            ON rejections (profile, file_hash, kind, category);
        CREATE TABLE recycle (
            id INTEGER PRIMARY KEY, recycled_at INTEGER NOT NULL, profile TEXT NOT NULL,
            original_path TEXT NOT NULL, stored_path TEXT NOT NULL UNIQUE,
            file_hash TEXT NOT NULL, reason TEXT NOT NULL DEFAULT ''
        );
        INSERT INTO journal (op_id, phase, at, profile, action, source, dest)
        VALUES ('op-from-v1', 'committed', 1000, 'downloads', 'move', '/a.pdf', '/D/a.pdf');",
    )
    .unwrap();
    conn.pragma_update(None, "user_version", 1u32).unwrap();
}

#[test]
fn a_v1_file_migrates_without_losing_its_journal() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    write_v1_store(&path);

    let store = Store::open(&path).unwrap();
    assert_eq!(store.schema_version().unwrap(), CURRENT_SCHEMA);

    let rows = store.journal_recent(None, 10).unwrap();
    assert_eq!(rows.len(), 1, "the v1 row must survive the migration");
    assert_eq!(rows[0].op_id, "op-from-v1");
}

#[test]
fn rows_predating_provenance_say_unknown_rather_than_guessing() {
    // The journal's value is that it is trustworthy. Backfilling a plausible
    // origin onto rows written before the column existed would put a fabricated
    // fact into the one table that must never contain one.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    write_v1_store(&path);

    let store = Store::open(&path).unwrap();
    let row = store.journal_recent(None, 10).unwrap().pop().unwrap();

    assert_eq!(row.provenance.origin, Origin::Unknown);
    assert_eq!(row.provenance.decided_by, DecidedBy::Unknown);
    assert_eq!(row.provenance.confidence, None);
}

#[test]
fn a_recorded_intent_carries_its_provenance_to_both_rows() {
    let s = store();
    let src = Path::new("/data/downloads/a.pdf");
    let dst = Path::new("/data/downloads/Documents/a.pdf");
    let dir = Path::new("/data/downloads/Documents");

    let intent = Intent {
        provenance: Provenance::model_auto(Some(0.91)),
        ..intent("downloads", src, dst, dir)
    };
    let op = s.record_intent(&intent).unwrap();
    s.record_result(&op, &intent, &Outcome::Committed).unwrap();

    let rows = s.journal_recent(Some("downloads"), 10).unwrap();
    assert_eq!(rows.len(), 2, "an intent and its result");
    for row in &rows {
        assert_eq!(row.provenance.origin, Origin::Model);
        assert_eq!(row.provenance.decided_by, DecidedBy::Auto);
        assert_eq!(row.provenance.confidence, Some(0.91));
    }
}

#[test]
fn human_initiated_operations_are_distinguishable_from_model_proposals() {
    // The whole point: a move that a person asked for and a move a model
    // proposed look identical in `action`, and must not in the journal.
    let s = store();
    let src = Path::new("/recycled/a.pdf");
    let dst = Path::new("/data/a.pdf");
    let dir = Path::new("/data");

    let by_model =
        Intent { provenance: Provenance::model_auto(Some(0.8)), ..intent("p", src, dst, dir) };
    let by_human = Intent { provenance: Provenance::human(), ..intent("p", src, dst, dir) };
    s.record_intent(&by_model).unwrap();
    s.record_intent(&by_human).unwrap();

    let origins: Vec<_> =
        s.journal_recent(Some("p"), 10).unwrap().iter().map(|r| r.provenance.origin).collect();
    assert!(origins.contains(&Origin::Model));
    assert!(origins.contains(&Origin::Human));
}
