#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::duration_suboptimal_units
)]
//! `link_mode = "hardlink"` and `"symlink"`: the source stays, a link appears,
//! and a re-run neither re-asks the model nor trips over its own work.

use bower_config::{LinkMode, Metadata, OnConflict, Profile, Rename, ReviewPlacement};
use bower_core::context::BatchContext;
use bower_core::exec::{self, ExecContext, ExecError, Executed, Mode};
use bower_core::llm::{BatchResponse, LlmBackend, LlmError};
use bower_core::model::{DestPath, Proposal, ProposalOutcome, RawProposal, ResolvedAction};
use bower_core::review::{self, ResolveOptions};
use bower_core::run::{RunOptions, run_profile};
use bower_core::scan::{ScanOptions, SkipReason};
use bower_core::state::{JournalAction, Origin, Provenance, Store};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

/// Proposes one category at one confidence, and remembers every file it was
/// asked about.
struct Recording {
    category: Mutex<String>,
    confidence: f32,
    asked: Mutex<Vec<String>>,
}

impl Recording {
    fn new(category: &str, confidence: f32) -> Self {
        Self {
            category: Mutex::new(category.to_owned()),
            confidence,
            asked: Mutex::new(Vec::new()),
        }
    }
    fn changes_its_mind(&self, category: &str) {
        category.clone_into(&mut self.category.lock().unwrap());
    }
    fn asked(&self) -> usize {
        self.asked.lock().unwrap().len()
    }
}

impl LlmBackend for Recording {
    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "recording"
    }

    fn classify(&self, ctx: &BatchContext) -> Result<BatchResponse, LlmError> {
        let category = self.category.lock().unwrap().clone();
        let mut outcomes = BTreeMap::new();
        for file in &ctx.files {
            self.asked.lock().unwrap().push(file.file_name.clone());
            outcomes.insert(
                file.file_id.clone(),
                ProposalOutcome::Ok(Proposal::Categorize(RawProposal {
                    file_id: file.file_id.clone(),
                    category: category.clone(),
                    is_new_category: false,
                    name_tokens: BTreeMap::new(),
                    confidence: self.confidence,
                    reasoning: "model".to_owned(),
                })),
            );
        }
        Ok(BatchResponse { outcomes })
    }
}

fn profile(path: &Path, link_mode: LinkMode) -> Profile {
    Profile {
        name: "downloads".to_owned(),
        path: path.to_path_buf(),
        description: String::new(),
        enabled: true,
        llm_backend: "test".to_owned(),
        destination_root: path.to_path_buf(),
        categories: vec!["Documents".to_owned(), "Photos".to_owned()],
        allow_dynamic_categories: false,
        allow_delete_suggestions: false,
        batch_size: 25,
        confidence_threshold: 0.75,
        on_conflict: OnConflict::Quarantine,
        link_mode,
        stability_wait: Duration::ZERO,
        exclude_patterns: vec![],
        include_subdirs: false,
        rename: Rename::Disabled,
        rules: vec![],
        metadata: Metadata {
            detect_mime: false,
            extract_exif: false,
            extract_audio_tags: false,
            extract_pdf_metadata: false,
            content_sniff_bytes: 0,
        },
    }
}

fn options(mode: Mode) -> RunOptions {
    RunOptions {
        mode,
        scan: ScanOptions::default(),
        review_placement: ReviewPlacement::InPlace,
        quarantine_dir: None,
        utc_offset_secs: 0,
    }
}

fn touch(path: &Path, body: &str) {
    if let Some(p) = path.parent() {
        fs::create_dir_all(p).unwrap();
    }
    fs::write(path, body).unwrap();
}

fn same_inode(a: &Path, b: &Path) -> bool {
    let (x, y) = (fs::metadata(a).unwrap(), fs::metadata(b).unwrap());
    x.dev() == y.dev() && x.ino() == y.ino()
}

fn ctx(store: &Store, mode: Mode) -> ExecContext<'_> {
    ExecContext {
        profile: "p",
        mode,
        file_hash: None,
        journal: store,
        provenance: Provenance::model_auto(Some(0.9)),
    }
}

fn dest(root: &Path, name: &str) -> DestPath {
    DestPath::under(root, "Documents", name).unwrap()
}

fn actions(store: &Store) -> Vec<String> {
    store
        .journal_recent(None, 20)
        .unwrap()
        .into_iter()
        .filter(|r| r.phase == "committed")
        .map(|r| r.action)
        .collect()
}

// --- the executor ------------------------------------------------------------

#[test]
fn a_hardlink_leaves_the_source_and_shares_its_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().unwrap();
    let src = dir.path().join("a.pdf");
    touch(&src, "body");
    let d = dest(dir.path(), "a.pdf");

    let out = exec::apply_via(
        &ResolvedAction::Move { dest: d.clone() },
        &src,
        &ctx(&store, Mode::Execute),
        LinkMode::Hardlink,
    )
    .unwrap();

    assert!(matches!(out, Executed::Moved { via: LinkMode::Hardlink, .. }));
    assert!(src.exists(), "a link mode must not remove the source");
    assert!(same_inode(&src, d.as_path()), "a hard link is the same file under two names");
    assert_eq!(actions(&store), vec!["hardlink"], "the journal says what actually happened");
}

#[test]
fn a_symlink_points_at_the_source_and_leaves_it_alone() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().unwrap();
    let src = dir.path().join("a.pdf");
    touch(&src, "body");
    let d = dest(dir.path(), "a.pdf");

    exec::apply_via(
        &ResolvedAction::Move { dest: d.clone() },
        &src,
        &ctx(&store, Mode::Execute),
        LinkMode::Symlink,
    )
    .unwrap();

    assert!(src.exists());
    assert_eq!(fs::read_link(d.as_path()).unwrap(), src, "the target is the source, absolute");
    assert_eq!(fs::read_to_string(d.as_path()).unwrap(), "body", "and it resolves");
    assert_eq!(actions(&store), vec!["symlink"]);
}

#[test]
fn neither_link_mode_will_replace_something_already_there() {
    for via in [LinkMode::Hardlink, LinkMode::Symlink] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        let src = dir.path().join("a.pdf");
        touch(&src, "new");
        let d = dest(dir.path(), "a.pdf");
        touch(d.as_path(), "precious");

        let err = exec::apply_via(
            &ResolvedAction::Move { dest: d.clone() },
            &src,
            &ctx(&store, Mode::Execute),
            via,
        )
        .unwrap_err();

        assert!(matches!(err, ExecError::DestinationOccupied { .. }), "{via:?}: {err}");
        assert_eq!(fs::read_to_string(d.as_path()).unwrap(), "precious");
        assert!(src.exists());
    }
}

#[test]
fn parking_a_file_is_a_real_move_whatever_the_link_mode() {
    // A link would leave the file exactly where it was, which defeats parking.
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().unwrap();
    let src = dir.path().join("a.pdf");
    touch(&src, "body");
    let d = dest(dir.path(), "a.pdf");

    exec::relocate_via(
        &src,
        &d,
        JournalAction::Quarantine,
        LinkMode::Symlink,
        &ctx(&store, Mode::Execute),
    )
    .unwrap();

    assert!(!src.exists(), "parked files leave the inbox");
    assert!(!fs::symlink_metadata(d.as_path()).unwrap().file_type().is_symlink());
    assert_eq!(actions(&store), vec!["quarantine"]);
}

#[test]
fn a_dry_run_creates_no_link() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().unwrap();
    let src = dir.path().join("a.pdf");
    touch(&src, "body");
    let d = dest(dir.path(), "a.pdf");

    let out = exec::apply_via(
        &ResolvedAction::Move { dest: d.clone() },
        &src,
        &ctx(&store, Mode::DryRun),
        LinkMode::Symlink,
    )
    .unwrap();

    assert!(matches!(out, Executed::WouldMove { via: LinkMode::Symlink, .. }));
    assert!(fs::symlink_metadata(d.as_path()).is_err());
    assert!(actions(&store).is_empty());
}

// --- a whole run ---------------------------------------------------------------

#[test]
fn a_linking_run_files_the_file_and_keeps_it_in_the_inbox() {
    for via in [LinkMode::Hardlink, LinkMode::Symlink] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        touch(&dir.path().join("a.pdf"), "body");

        let report = run_profile(
            &profile(dir.path(), via),
            &Recording::new("Documents", 0.9),
            &options(Mode::Execute),
            &store,
        )
        .unwrap();

        assert_eq!(report.moved(), 1, "{via:?}");
        assert!(dir.path().join("a.pdf").exists(), "{via:?}: the source stays");
        assert!(dir.path().join("Documents/a.pdf").exists(), "{via:?}");
    }
}

#[test]
fn a_second_run_neither_asks_the_model_again_nor_links_the_file_somewhere_else() {
    for via in [LinkMode::Hardlink, LinkMode::Symlink] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        touch(&dir.path().join("a.pdf"), "body");
        let p = profile(dir.path(), via);
        let backend = Recording::new("Documents", 0.9);

        run_profile(&p, &backend, &options(Mode::Execute), &store).unwrap();
        assert_eq!(backend.asked(), 1);

        // The model would now file it under Photos. It must never be asked.
        backend.changes_its_mind("Photos");
        let second = run_profile(&p, &backend, &options(Mode::Execute), &store).unwrap();

        assert_eq!(backend.asked(), 1, "{via:?}: the model must not be asked about it again");
        assert!(second.outcomes.is_empty(), "{via:?}");
        assert!(
            second.skipped.iter().any(|s| s.reason == SkipReason::AlreadyLinked),
            "{via:?}: the skip should say why"
        );
        assert!(!dir.path().join("Photos").exists(), "{via:?}: no stray link in another category");
    }
}

#[test]
fn deleting_the_link_makes_the_next_run_file_the_source_again() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().unwrap();
    touch(&dir.path().join("a.pdf"), "body");
    let p = profile(dir.path(), LinkMode::Symlink);
    let backend = Recording::new("Documents", 0.9);

    run_profile(&p, &backend, &options(Mode::Execute), &store).unwrap();
    fs::remove_file(dir.path().join("Documents/a.pdf")).unwrap();
    let second = run_profile(&p, &backend, &options(Mode::Execute), &store).unwrap();

    assert_eq!(second.moved(), 1);
    assert_eq!(backend.asked(), 2);
    assert!(dir.path().join("Documents/a.pdf").exists());
}

#[test]
fn a_link_that_a_lost_journal_cannot_vouch_for_is_still_not_mistaken_for_a_conflict() {
    // The state database can be wiped or lost; the links are on disk. A run
    // that finds its own earlier link must not quarantine it as "a different
    // file already exists".
    for via in [LinkMode::Hardlink, LinkMode::Symlink] {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("a.pdf"), "body");
        let p = profile(dir.path(), via);
        let backend = Recording::new("Documents", 0.9);

        run_profile(&p, &backend, &options(Mode::Execute), &Store::open_in_memory().unwrap())
            .unwrap();
        let fresh = Store::open_in_memory().unwrap();
        let second = run_profile(&p, &backend, &options(Mode::Execute), &fresh).unwrap();

        assert_eq!(second.attention_count(), 0, "{via:?}: nothing here needs a human");
        assert!(fresh.review_list(None, None).unwrap().is_empty(), "{via:?}");
    }
}

#[test]
fn a_different_file_at_the_destination_is_still_a_conflict_in_a_link_mode() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().unwrap();
    touch(&dir.path().join("a.pdf"), "mine");
    touch(&dir.path().join("Documents/a.pdf"), "someone else's");

    let report = run_profile(
        &profile(dir.path(), LinkMode::Symlink),
        &Recording::new("Documents", 0.9),
        &options(Mode::Execute),
        &store,
    )
    .unwrap();

    assert_eq!(report.moved(), 0);
    assert_eq!(report.attention_count(), 1);
    assert_eq!(fs::read_to_string(dir.path().join("Documents/a.pdf")).unwrap(), "someone else's");
}

#[test]
fn approving_a_queued_item_links_it_too() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().unwrap();
    touch(&dir.path().join("a.pdf"), "body");
    let p = profile(dir.path(), LinkMode::Symlink);

    // 0.10 is below the threshold, so it waits for a person.
    run_profile(&p, &Recording::new("Documents", 0.10), &options(Mode::Execute), &store).unwrap();
    let item = store.review_list(None, None).unwrap().pop().unwrap();
    review::approve(&store, &item, &p, &ResolveOptions { mode: Mode::Execute, recycle_dir: None })
        .unwrap();

    let linked = dir.path().join("Documents/a.pdf");
    assert!(fs::symlink_metadata(&linked).unwrap().file_type().is_symlink());
    assert!(dir.path().join("a.pdf").exists());
    let rows = store.journal_recent(None, 10).unwrap();
    assert!(rows.iter().any(|r| r.action == "symlink" && r.provenance.origin == Origin::Model));

    // And it is remembered as linked, so the next run leaves it alone.
    let next =
        run_profile(&p, &Recording::new("Photos", 0.9), &options(Mode::Execute), &store).unwrap();
    assert!(next.outcomes.is_empty());
}

#[test]
fn move_mode_is_untouched_by_all_of_this() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().unwrap();
    touch(&dir.path().join("a.pdf"), "body");

    run_profile(
        &profile(dir.path(), LinkMode::Move),
        &Recording::new("Documents", 0.9),
        &options(Mode::Execute),
        &store,
    )
    .unwrap();

    assert!(!dir.path().join("a.pdf").exists());
    assert!(dir.path().join("Documents/a.pdf").exists());
    assert_eq!(actions(&store), vec!["move"]);
}
