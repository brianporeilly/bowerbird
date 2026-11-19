#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
//! `bower undo`: removing the links a link-mode run made, and only those.

use bower_config::LinkMode;
use bower_core::state::{Intent, JournalAction, Outcome, Provenance, Store};
use bower_core::undo::{self, Candidate, Undone};
use std::fs;
use std::path::Path;

/// Makes a link on disk and journals it the way a run would.
fn link(store: &Store, profile: &str, src: &Path, dest: &Path, via: LinkMode) {
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    let action = if via == LinkMode::Symlink {
        std::os::unix::fs::symlink(src, dest).unwrap();
        JournalAction::Symlink
    } else {
        fs::hard_link(src, dest).unwrap();
        JournalAction::Hardlink
    };
    let intent = Intent {
        profile,
        action,
        source: src,
        dest: Some(dest),
        dest_dir: dest.parent(),
        file_hash: None,
        provenance: Provenance::model_auto(Some(0.9)),
    };
    let op = store.record_intent(&intent).unwrap();
    store.record_result(&op, &intent, &Outcome::Committed).unwrap();
}

fn unlink_rows(store: &Store) -> usize {
    store
        .journal_recent(None, 100)
        .unwrap()
        .iter()
        .filter(|r| r.action == "unlink" && r.phase == "committed")
        .count()
}

struct World {
    _dir: tempfile::TempDir,
    root: std::path::PathBuf,
    store: Store,
}

fn world() -> World {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    fs::create_dir_all(root.join("inbox")).unwrap();
    World { _dir: dir, root, store: Store::open_in_memory().unwrap() }
}

fn one(w: &World, via: LinkMode) -> (Candidate, std::path::PathBuf) {
    let src = w.root.join("inbox/a.txt");
    fs::write(&src, "hello").unwrap();
    let dest = w.root.join("organized/Docs/a.txt");
    link(&w.store, "p", &src, &dest, via);
    let plan = undo::plan(&w.store, None, None).unwrap();
    assert_eq!(plan.len(), 1);
    (plan.into_iter().next().unwrap(), src)
}

#[test]
fn removes_a_symlink_and_a_hardlink_and_never_the_source() {
    for via in [LinkMode::Symlink, LinkMode::Hardlink] {
        let w = world();
        let (c, src) = one(&w, via);
        assert!(matches!(undo::undo_one(&w.store, &c, false).unwrap(), Undone::Removed));
        assert!(fs::symlink_metadata(&c.dest).is_err(), "{via:?} link should be gone");
        assert_eq!(fs::read_to_string(&src).unwrap(), "hello", "{via:?} source is untouched");
        assert_eq!(unlink_rows(&w.store), 1, "{via:?} undo is journalled");
    }
}

#[test]
fn a_dry_run_changes_nothing_and_writes_no_journal_row() {
    let w = world();
    let (c, _) = one(&w, LinkMode::Symlink);
    assert!(matches!(undo::undo_one(&w.store, &c, true).unwrap(), Undone::WouldRemove));
    assert!(fs::symlink_metadata(&c.dest).is_ok());
    assert_eq!(unlink_rows(&w.store), 0);
}

#[test]
fn a_file_saved_over_the_link_is_left_alone() {
    for via in [LinkMode::Symlink, LinkMode::Hardlink] {
        let w = world();
        let (c, _) = one(&w, via);
        fs::remove_file(&c.dest).unwrap();
        fs::write(&c.dest, "someone's own work").unwrap();
        assert!(matches!(undo::undo_one(&w.store, &c, false).unwrap(), Undone::Changed));
        assert_eq!(fs::read_to_string(&c.dest).unwrap(), "someone's own work");
        assert_eq!(unlink_rows(&w.store), 0);
    }
}

#[test]
fn a_repointed_symlink_is_left_alone() {
    let w = world();
    let (c, _) = one(&w, LinkMode::Symlink);
    let other = w.root.join("inbox/other.txt");
    fs::write(&other, "x").unwrap();
    fs::remove_file(&c.dest).unwrap();
    std::os::unix::fs::symlink(&other, &c.dest).unwrap();
    assert!(matches!(undo::undo_one(&w.store, &c, false).unwrap(), Undone::Changed));
    assert!(fs::symlink_metadata(&c.dest).is_ok());
}

#[test]
fn a_hardlink_whose_source_is_gone_is_the_last_copy_and_is_kept() {
    let w = world();
    let (c, src) = one(&w, LinkMode::Hardlink);
    fs::remove_file(&src).unwrap();
    assert!(matches!(undo::undo_one(&w.store, &c, false).unwrap(), Undone::LastCopy));
    assert_eq!(fs::read_to_string(&c.dest).unwrap(), "hello", "the data must survive");
    assert_eq!(unlink_rows(&w.store), 0);
}

#[test]
fn a_dangling_symlink_is_still_removed() {
    let w = world();
    let (c, src) = one(&w, LinkMode::Symlink);
    fs::remove_file(&src).unwrap();
    assert!(matches!(undo::undo_one(&w.store, &c, false).unwrap(), Undone::Removed));
    assert!(fs::symlink_metadata(&c.dest).is_err());
}

#[test]
fn a_link_already_gone_is_reported_as_such() {
    let w = world();
    let (c, _) = one(&w, LinkMode::Symlink);
    fs::remove_file(&c.dest).unwrap();
    assert!(matches!(undo::undo_one(&w.store, &c, false).unwrap(), Undone::Gone));
    assert_eq!(unlink_rows(&w.store), 0);
}

#[test]
fn undoing_twice_is_harmless() {
    let w = world();
    let (c, _) = one(&w, LinkMode::Hardlink);
    undo::undo_one(&w.store, &c, false).unwrap();
    assert!(matches!(undo::undo_one(&w.store, &c, false).unwrap(), Undone::Gone));
    assert_eq!(unlink_rows(&w.store), 1);
}

#[test]
fn the_plan_filters_by_profile_and_lists_a_destination_once() {
    let w = world();
    let a = w.root.join("inbox/a.txt");
    let b = w.root.join("inbox/b.txt");
    fs::write(&a, "a").unwrap();
    fs::write(&b, "b").unwrap();
    let dest_a = w.root.join("organized/Docs/a.txt");
    link(&w.store, "one", &a, &dest_a, LinkMode::Symlink);
    link(&w.store, "two", &b, &w.root.join("organized/Docs/b.txt"), LinkMode::Symlink);
    // Linked, removed by hand, linked again: one destination, one candidate.
    fs::remove_file(&dest_a).unwrap();
    link(&w.store, "one", &a, &dest_a, LinkMode::Symlink);

    assert_eq!(undo::plan(&w.store, None, None).unwrap().len(), 2);
    let only_one = undo::plan(&w.store, Some("one"), None).unwrap();
    assert_eq!(only_one.len(), 1);
    assert_eq!(only_one[0].dest, dest_a);
    let future = u64::MAX / 2;
    assert!(undo::plan(&w.store, None, Some(future)).unwrap().is_empty());
}
