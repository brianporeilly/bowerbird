#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::duration_suboptimal_units
)]
//! Resolving pending decisions, against real directories and a real store.

use bower_config::{Metadata, OnConflict, Profile, Rename, ReviewPlacement};
use bower_core::context::BatchContext;
use bower_core::exec::Mode;
use bower_core::llm::{BatchResponse, LlmBackend, LlmError};
use bower_core::model::{DeleteProposal, Proposal, ProposalOutcome, RawProposal};
use bower_core::review::{self, Approved, ResolveError, ResolveOptions};
use bower_core::run::{RunOptions, run_profile};
use bower_core::scan::ScanOptions;
use bower_core::state::{ReviewItem, ReviewKind, Store};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

struct Backend {
    category: Option<String>,
    confidence: f32,
}

impl Backend {
    fn filing(category: &str, confidence: f32) -> Self {
        Self { category: Some(category.to_owned()), confidence }
    }

    /// Proposes deletion for everything, which no shipped backend does; the
    /// point is to exercise the guarded path.
    fn deleting() -> Self {
        Self { category: None, confidence: 0.99 }
    }
}

impl LlmBackend for Backend {
    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "test"
    }

    fn classify(&self, ctx: &BatchContext) -> Result<BatchResponse, LlmError> {
        let mut outcomes = BTreeMap::new();
        for file in &ctx.files {
            let proposal = match &self.category {
                Some(category) => Proposal::Categorize(RawProposal {
                    file_id: file.file_id.clone(),
                    category: category.clone(),
                    is_new_category: false,
                    name_tokens: BTreeMap::new(),
                    confidence: self.confidence,
                    reasoning: "test".to_owned(),
                }),
                None => Proposal::SuggestDelete(DeleteProposal {
                    file_id: file.file_id.clone(),
                    reason: "looks like a duplicate installer".to_owned(),
                    confidence: self.confidence,
                }),
            };
            outcomes.insert(file.file_id.clone(), ProposalOutcome::Ok(proposal));
        }
        Ok(BatchResponse { outcomes })
    }
}

fn profile(path: &Path) -> Profile {
    Profile {
        name: "downloads".to_owned(),
        path: path.to_path_buf(),
        description: String::new(),
        enabled: true,
        llm_backend: "test".to_owned(),
        destination_root: path.to_path_buf(),
        categories: vec!["Documents".to_owned()],
        allow_dynamic_categories: false,
        allow_delete_suggestions: false,
        batch_size: 25,
        confidence_threshold: 0.75,
        on_conflict: OnConflict::Quarantine,
        stability_wait: Duration::ZERO,
        exclude_patterns: vec![],
        include_subdirs: true,
        rename: Rename::Disabled,
        metadata: Metadata {
            detect_mime: false,
            extract_exif: false,
            extract_audio_tags: false,
            extract_pdf_metadata: false,
            content_sniff_bytes: 0,
        },
    }
}

fn run_options() -> RunOptions {
    RunOptions {
        mode: Mode::Execute,
        scan: ScanOptions::default(),
        review_placement: ReviewPlacement::InPlace,
        quarantine_dir: None,
    }
}

fn touch(path: &Path, body: &str) {
    if let Some(p) = path.parent() {
        fs::create_dir_all(p).unwrap();
    }
    fs::write(path, body).unwrap();
}

/// Runs once and returns the single queued item it produced.
fn queue_one(dir: &Path, store: &Store, backend: &Backend, p: &Profile) -> ReviewItem {
    run_profile(p, backend, &run_options(), store).unwrap();
    let items = store.review_list(None, None).unwrap();
    assert_eq!(items.len(), 1, "expected exactly one queued item in {}", dir.display());
    items.into_iter().next().unwrap()
}

fn opts(recycle_dir: Option<&Path>) -> ResolveOptions<'_> {
    ResolveOptions { mode: Mode::Execute, recycle_dir }
}

// --- approving a filing decision --------------------------------------------

#[test]
fn approving_files_the_document_where_a_confident_run_would_have() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().unwrap();
    touch(&dir.path().join("a.pdf"), "body");

    let p = profile(dir.path());
    let item = queue_one(dir.path(), &store, &Backend::filing("Documents", 0.10), &p);

    let outcome = review::approve(&store, &item, &p, &opts(None)).unwrap();
    assert_eq!(outcome, Approved::Filed { to: dir.path().join("Documents/a.pdf") });
    assert!(dir.path().join("Documents/a.pdf").exists());
    assert!(store.review_list(None, None).unwrap().is_empty(), "the row is resolved");
}

#[test]
fn approving_re_validates_the_hash_and_refuses_a_changed_file() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().unwrap();
    let path = dir.path().join("a.pdf");
    touch(&path, "body");

    let p = profile(dir.path());
    let item = queue_one(dir.path(), &store, &Backend::filing("Documents", 0.10), &p);

    // Days pass; the file is edited.
    touch(&path, "something else entirely");

    let err = review::approve(&store, &item, &p, &opts(None)).unwrap_err();
    assert!(matches!(err, ResolveError::Changed { .. }), "got {err:?}");
    assert!(path.exists(), "the file must not be filed on a stale decision");
    assert!(
        store.review_list(None, None).unwrap().is_empty(),
        "the stale row is discarded rather than left to be approved later"
    );
}

#[test]
fn approving_refuses_a_file_that_vanished() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().unwrap();
    let path = dir.path().join("a.pdf");
    touch(&path, "body");

    let p = profile(dir.path());
    let item = queue_one(dir.path(), &store, &Backend::filing("Documents", 0.10), &p);
    fs::remove_file(&path).unwrap();

    assert!(matches!(
        review::approve(&store, &item, &p, &opts(None)).unwrap_err(),
        ResolveError::Vanished { .. }
    ));
}

#[test]
fn approving_respects_the_config_as_it_stands_now() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().unwrap();
    touch(&dir.path().join("a.pdf"), "body");

    let p = profile(dir.path());
    let item = queue_one(dir.path(), &store, &Backend::filing("Documents", 0.10), &p);

    // The category is removed from the profile before anyone gets to the queue.
    let mut narrowed = p.clone();
    narrowed.categories = vec!["Invoices".to_owned()];
    narrowed.allow_dynamic_categories = false;

    let err = review::approve(&store, &item, &narrowed, &opts(None)).unwrap_err();
    assert!(matches!(err, ResolveError::CategoryNoLongerAllowed { .. }), "got {err:?}");
    assert!(dir.path().join("a.pdf").exists());
}

#[test]
fn approving_never_overwrites_something_that_arrived_first() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().unwrap();
    touch(&dir.path().join("a.pdf"), "queued");

    let p = profile(dir.path());
    let item = queue_one(dir.path(), &store, &Backend::filing("Documents", 0.10), &p);

    // Something else takes the destination while the decision was pending.
    touch(&dir.path().join("Documents/a.pdf"), "got there first");

    let outcome = review::approve(&store, &item, &p, &opts(None)).unwrap();
    assert_eq!(outcome, Approved::Filed { to: dir.path().join("Documents/a-1.pdf") });
    assert_eq!(fs::read_to_string(dir.path().join("Documents/a.pdf")).unwrap(), "got there first");
    assert_eq!(fs::read_to_string(dir.path().join("Documents/a-1.pdf")).unwrap(), "queued");
}

#[test]
fn an_item_that_never_had_a_destination_cannot_be_approved() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().unwrap();
    touch(&dir.path().join("a.pdf"), "body");

    // An undeclared category fails at stage 3, long before a path exists.
    let p = profile(dir.path());
    let item = queue_one(dir.path(), &store, &Backend::filing("Nonexistent", 0.99), &p);
    assert!(item.proposed_dest.is_none());

    assert!(matches!(
        review::approve(&store, &item, &p, &opts(None)).unwrap_err(),
        ResolveError::NothingProposed
    ));
}

// --- rejecting ---------------------------------------------------------------

#[test]
fn rejecting_remembers_the_refusal_so_the_next_run_stays_quiet() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().unwrap();
    touch(&dir.path().join("a.pdf"), "body");

    let p = profile(dir.path());
    let item = queue_one(dir.path(), &store, &Backend::filing("Documents", 0.10), &p);
    review::reject(&store, &item, Some("not a document"), &opts(None)).unwrap();

    // A later, more confident run must still leave it alone.
    let report =
        run_profile(&p, &Backend::filing("Documents", 0.99), &run_options(), &store).unwrap();
    assert_eq!(report.moved(), 0);
    assert!(dir.path().join("a.pdf").exists());
}

#[test]
fn rejecting_a_parked_item_puts_it_back_where_it_came_from() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("downloads");
    let holding = dir.path().join("_review");
    let store = Store::open_in_memory().unwrap();
    touch(&source.join("a.pdf"), "body");

    let p = profile(&source);
    let mut options = run_options();
    options.review_placement = ReviewPlacement::Quarantine;
    options.quarantine_dir = Some(holding.clone());
    options.scan.extra_excluded_roots = vec![holding.clone()];

    run_profile(&p, &Backend::filing("Documents", 0.10), &options, &store).unwrap();
    let item = store.review_list(None, None).unwrap().into_iter().next().unwrap();
    assert!(holding.join("downloads/a.pdf").exists());

    let outcome = review::reject(&store, &item, None, &opts(None)).unwrap();
    assert_eq!(outcome.restored_to, Some(source.join("a.pdf")));
    assert!(source.join("a.pdf").exists(), "no longer pending, so no longer in the pending folder");
    assert!(!holding.join("downloads/a.pdf").exists());
}

// --- the recycle lifecycle ---------------------------------------------------

fn deleting_profile(path: &Path) -> Profile {
    let mut p = profile(path);
    p.allow_delete_suggestions = true;
    p
}

#[test]
fn approving_a_deletion_moves_the_file_but_destroys_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let recycle = dir.path().join("_recycled");
    let source = dir.path().join("downloads");
    let store = Store::open_in_memory().unwrap();
    touch(&source.join("old.iso"), "precious bytes");

    let p = deleting_profile(&source);
    let item = queue_one(&source, &store, &Backend::deleting(), &p);
    assert_eq!(item.kind, ReviewKind::Recycle);

    let outcome = review::approve(&store, &item, &p, &opts(Some(&recycle))).unwrap();
    let Approved::Recycled { to } = outcome else { panic!("expected a recycle, got {outcome:?}") };

    assert!(!source.join("old.iso").exists(), "moved out of the way");
    assert_eq!(
        fs::read_to_string(&to).unwrap(),
        "precious bytes",
        "approving a deletion must never destroy anything"
    );
    assert_eq!(store.recycle_list().unwrap().len(), 1);
}

#[test]
fn a_recycled_file_can_be_restored_to_exactly_where_it_was() {
    let dir = tempfile::tempdir().unwrap();
    let recycle = dir.path().join("_recycled");
    let source = dir.path().join("downloads");
    let store = Store::open_in_memory().unwrap();
    let original = source.join("old.iso");
    touch(&original, "precious bytes");

    let p = deleting_profile(&source);
    let item = queue_one(&source, &store, &Backend::deleting(), &p);
    review::approve(&store, &item, &p, &opts(Some(&recycle))).unwrap();

    let recycled = store.recycle_list().unwrap().into_iter().next().unwrap();
    let restored = review::restore(&store, &recycled, &opts(Some(&recycle))).unwrap();

    assert_eq!(restored, original);
    assert_eq!(fs::read_to_string(&original).unwrap(), "precious bytes");
    assert!(store.recycle_list().unwrap().is_empty());
}

#[test]
fn purge_is_the_only_thing_that_actually_deletes() {
    let dir = tempfile::tempdir().unwrap();
    let recycle = dir.path().join("_recycled");
    let source = dir.path().join("downloads");
    let store = Store::open_in_memory().unwrap();
    touch(&source.join("old.iso"), "precious bytes");

    let p = deleting_profile(&source);
    let item = queue_one(&source, &store, &Backend::deleting(), &p);
    review::approve(&store, &item, &p, &opts(Some(&recycle))).unwrap();

    let recycled = store.recycle_list().unwrap().into_iter().next().unwrap();
    let stored = recycled.stored_path.clone();
    assert!(stored.exists(), "still on disk after the deletion was approved");

    // A dry run still destroys nothing.
    review::purge(&store, &recycled, &ResolveOptions { mode: Mode::DryRun, recycle_dir: None })
        .unwrap();
    assert!(stored.exists(), "a dry-run purge must not delete");

    review::purge(&store, &recycled, &opts(Some(&recycle))).unwrap();
    assert!(!stored.exists(), "purge is the one operation that destroys");
    assert!(store.recycle_list().unwrap().is_empty());
}

#[test]
fn a_deletion_cannot_be_approved_without_somewhere_to_put_it() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("downloads");
    let store = Store::open_in_memory().unwrap();
    touch(&source.join("old.iso"), "precious bytes");

    let p = deleting_profile(&source);
    let item = queue_one(&source, &store, &Backend::deleting(), &p);

    assert!(matches!(
        review::approve(&store, &item, &p, &opts(None)).unwrap_err(),
        ResolveError::NoRecycleDir
    ));
    assert!(source.join("old.iso").exists());
}

#[test]
fn two_recycled_files_of_the_same_name_do_not_collide() {
    let dir = tempfile::tempdir().unwrap();
    let recycle = dir.path().join("_recycled");
    let source = dir.path().join("downloads");
    let store = Store::open_in_memory().unwrap();
    let p = deleting_profile(&source);

    let mut stored: Vec<PathBuf> = Vec::new();
    for body in ["first", "second"] {
        touch(&source.join("old.iso"), body);
        run_profile(&p, &Backend::deleting(), &run_options(), &store).unwrap();
        let item = store
            .review_list(None, None)
            .unwrap()
            .into_iter()
            .find(|i| i.path == source.join("old.iso"))
            .expect("queued");
        let outcome = review::approve(&store, &item, &p, &opts(Some(&recycle))).unwrap();
        let Approved::Recycled { to } = outcome else { panic!("expected a recycle") };
        stored.push(to);
    }

    assert_eq!(fs::read_to_string(&stored[0]).unwrap(), "first");
    assert_eq!(fs::read_to_string(&stored[1]).unwrap(), "second");
    assert_ne!(stored[0], stored[1], "the second must not have replaced the first");
}
