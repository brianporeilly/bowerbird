#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::duration_suboptimal_units
)]
//! The SQLite state store: journal, review queue, rejections, recycle.

use bower_core::state::{
    Intent, JournalAction, NewReviewItem, Outcome, ReviewKind, StateError, Store,
};
use std::path::Path;

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
    }
}

// --- schema and migrations --------------------------------------------------

#[test]
fn a_fresh_store_is_migrated_to_the_current_version() {
    assert_eq!(store().schema_version().unwrap(), 1);
}

#[test]
fn reopening_an_existing_store_is_a_no_op() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");

    let first = Store::open(&path).unwrap();
    first.remember_rejection("p", ReviewKind::Review, "hash", 10, "Documents", None).unwrap();
    drop(first);

    let second = Store::open(&path).unwrap();
    assert_eq!(second.schema_version().unwrap(), 1);
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
            assert_eq!(supported, 1);
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
