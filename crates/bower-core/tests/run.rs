#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::duration_suboptimal_units
)]
//! The orchestrator end to end, against real directories and a real store.

use bower_config::{Metadata, OnConflict, Profile, Rename, ReviewPlacement};
use bower_core::context::BatchContext;
use bower_core::exec::Mode;
use bower_core::llm::{BatchResponse, LlmBackend, LlmError};
use bower_core::model::{Proposal, ProposalOutcome, RawProposal};
use bower_core::run::{RunOptions, RunReport, run_profile};
use bower_core::scan::ScanOptions;
use bower_core::state::{DecidedBy, Origin, ReviewKind, Store};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::time::Duration;

/// A backend that proposes one fixed category at one fixed confidence, so the
/// test controls exactly what the policy engine sees.
struct Fixed {
    category: String,
    confidence: f32,
}

impl Fixed {
    fn new(category: &str, confidence: f32) -> Self {
        Self { category: category.to_owned(), confidence }
    }
}

impl LlmBackend for Fixed {
    // The trait ties the lifetime to `&self` so backends can name themselves
    // from config; this one is a literal.
    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "fixed"
    }

    fn classify(&self, ctx: &BatchContext) -> Result<BatchResponse, LlmError> {
        let mut outcomes = BTreeMap::new();
        for file in &ctx.files {
            outcomes.insert(
                file.file_id.clone(),
                ProposalOutcome::Ok(Proposal::Categorize(RawProposal {
                    file_id: file.file_id.clone(),
                    category: self.category.clone(),
                    is_new_category: false,
                    name_tokens: BTreeMap::new(),
                    confidence: self.confidence,
                    reasoning: "fixed backend".to_owned(),
                })),
            );
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
        llm_backend: "fixed".to_owned(),
        destination_root: path.to_path_buf(),
        categories: vec!["Documents".to_owned()],
        allow_dynamic_categories: true,
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

fn run(p: &Profile, backend: &dyn LlmBackend, o: &RunOptions, s: &Store) -> RunReport {
    run_profile(p, backend, o, s).unwrap()
}

// --- the ADR-0002 §1 gap ----------------------------------------------------

#[test]
fn a_dynamically_created_category_is_not_re_ingested_on_the_next_run() {
    // The case ADR-0002 left open: an in-place profile that descends into
    // subdirectories, filing into a category the config never declared. Without
    // the journal there is nothing to tell the scanner that `Invoices/` is
    // output rather than input.
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().unwrap();
    touch(&dir.path().join("a.pdf"), "body");

    let p = profile(dir.path());
    assert!(p.is_in_place() && p.include_subdirs && p.allow_dynamic_categories);
    // "Invoices" is deliberately not in p.categories.
    let backend = Fixed::new("Invoices", 0.99);
    let opts = options(Mode::Execute);

    let first = run(&p, &backend, &opts, &store);
    assert_eq!(first.moved(), 1);
    assert!(dir.path().join("Invoices/a.pdf").exists());

    let second = run(&p, &backend, &opts, &store);
    assert_eq!(second.scanned, 0, "the first run's output must not be fresh input");
    assert_eq!(second.moved(), 0);
    assert!(!dir.path().join("Invoices/Invoices").exists());
}

#[test]
fn one_profiles_output_directory_does_not_hide_anothers_input() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().unwrap();
    touch(&dir.path().join("a.pdf"), "body");

    let p = profile(dir.path());
    run(&p, &Fixed::new("Invoices", 0.99), &options(Mode::Execute), &store);

    // A different profile scanning the same tree has its own journal history,
    // and has claimed nothing.
    let mut other = profile(dir.path());
    other.name = "other".to_owned();
    let report = run(&other, &Fixed::new("Documents", 0.99), &options(Mode::Execute), &store);
    assert_eq!(report.scanned, 1, "downloads' output is not other's managed directory");
}

// --- the review queue -------------------------------------------------------

#[test]
fn a_low_confidence_file_lands_in_the_review_queue_with_its_context() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().unwrap();
    touch(&dir.path().join("a.pdf"), "body");

    let report =
        run(&profile(dir.path()), &Fixed::new("Documents", 0.10), &options(Mode::Execute), &store);

    assert_eq!(report.attention_count(), 1);
    assert_eq!(report.newly_queued(), 1);

    let queued = store.review_list(None, None).unwrap();
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].kind, ReviewKind::Review);
    assert_eq!(queued[0].original_path, dir.path().join("a.pdf"));
    assert_eq!(queued[0].category, "Documents");
    assert!(!queued[0].file_hash.is_empty(), "the queue records the hash it saw");
    assert!(queued[0].proposed_dest.is_some(), "and where the file would have gone");
    assert!(dir.path().join("a.pdf").exists(), "in_place leaves the file where it is");
}

#[test]
fn a_repeated_run_does_not_pile_up_duplicate_queue_rows() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().unwrap();
    touch(&dir.path().join("a.pdf"), "body");

    let p = profile(dir.path());
    let backend = Fixed::new("Documents", 0.10);
    let opts = options(Mode::Execute);

    let first = run(&p, &backend, &opts, &store);
    let second = run(&p, &backend, &opts, &store);

    assert_eq!(first.newly_queued(), 1);
    assert_eq!(second.newly_queued(), 0, "the second run recognises the pending row");
    assert_eq!(store.review_list(None, None).unwrap().len(), 1);
}

// --- remembered rejections --------------------------------------------------

#[test]
fn a_rejected_proposal_is_not_re_surfaced_while_the_file_is_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().unwrap();
    let path = dir.path().join("a.pdf");
    touch(&path, "body");

    let digest = bower_core::hash::file_sha256(&path).unwrap();
    let size = fs::metadata(&path).unwrap().len();
    store
        .remember_rejection("downloads", ReviewKind::Review, &digest, size, "Documents", None)
        .unwrap();

    let report =
        run(&profile(dir.path()), &Fixed::new("Documents", 0.99), &options(Mode::Execute), &store);

    assert_eq!(report.moved(), 0, "a refused proposal must not be acted on");
    assert_eq!(report.attention_count(), 0, "nor asked again");
    assert!(path.exists());
}

#[test]
fn changing_the_file_makes_the_question_fresh_again() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().unwrap();
    let path = dir.path().join("a.pdf");
    touch(&path, "body");

    let digest = bower_core::hash::file_sha256(&path).unwrap();
    let size = fs::metadata(&path).unwrap().len();
    store
        .remember_rejection("downloads", ReviewKind::Review, &digest, size, "Documents", None)
        .unwrap();

    // Same length, different content: the size prefilter still admits it, and
    // the hash is what actually decides.
    touch(&path, "BODY");

    let report =
        run(&profile(dir.path()), &Fixed::new("Documents", 0.99), &options(Mode::Execute), &store);
    assert_eq!(report.moved(), 1, "a different file is a different question");
}

#[test]
fn a_rejection_in_one_profile_does_not_silence_another() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().unwrap();
    let path = dir.path().join("a.pdf");
    touch(&path, "body");

    let digest = bower_core::hash::file_sha256(&path).unwrap();
    let size = fs::metadata(&path).unwrap().len();
    store
        .remember_rejection("someone-else", ReviewKind::Review, &digest, size, "Documents", None)
        .unwrap();

    let report =
        run(&profile(dir.path()), &Fixed::new("Documents", 0.99), &options(Mode::Execute), &store);
    assert_eq!(report.moved(), 1);
}

// --- review placement -------------------------------------------------------

#[test]
fn quarantine_placement_parks_pending_items_where_they_can_be_browsed() {
    let dir = tempfile::tempdir().unwrap();
    let holding = dir.path().join("_review");
    let source = dir.path().join("downloads");
    let store = Store::open_in_memory().unwrap();
    touch(&source.join("a.pdf"), "body");

    let mut opts = options(Mode::Execute);
    opts.review_placement = ReviewPlacement::Quarantine;
    opts.quarantine_dir = Some(holding.clone());

    let report = run(&profile(&source), &Fixed::new("Documents", 0.10), &opts, &store);

    assert_eq!(report.attention_count(), 1);
    assert!(!source.join("a.pdf").exists(), "the file was moved out of the inbox");
    assert!(holding.join("downloads/a.pdf").exists(), "and into the holding folder");

    let queued = store.review_list(None, None).unwrap();
    assert_eq!(queued[0].path, holding.join("downloads/a.pdf"), "the queue knows where it is now");
    assert_eq!(
        queued[0].original_path,
        source.join("a.pdf"),
        "and where it came from, so the decision can be honoured"
    );
}

#[test]
fn parking_two_files_of_the_same_name_never_overwrites() {
    let dir = tempfile::tempdir().unwrap();
    let holding = dir.path().join("_review");
    let store = Store::open_in_memory().unwrap();

    let mut opts = options(Mode::Execute);
    opts.review_placement = ReviewPlacement::Quarantine;
    opts.quarantine_dir = Some(holding.clone());

    for body in ["first", "second"] {
        let source = dir.path().join("downloads");
        touch(&source.join("a.pdf"), body);
        run(&profile(&source), &Fixed::new("Documents", 0.10), &opts, &store);
    }

    assert_eq!(fs::read_to_string(holding.join("downloads/a.pdf")).unwrap(), "first");
    assert_eq!(fs::read_to_string(holding.join("downloads/a-1.pdf")).unwrap(), "second");
}

#[test]
fn quarantine_placement_without_a_holding_folder_is_refused_up_front() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().unwrap();
    touch(&dir.path().join("a.pdf"), "body");

    let mut opts = options(Mode::Execute);
    opts.review_placement = ReviewPlacement::Quarantine;
    opts.quarantine_dir = None;

    assert!(
        run_profile(&profile(dir.path()), &Fixed::new("Documents", 0.10), &opts, &store).is_err(),
        "a misconfiguration must fail before any file is touched"
    );
    assert!(dir.path().join("a.pdf").exists());
}

// --- dry run ----------------------------------------------------------------

#[test]
fn a_dry_run_writes_neither_files_nor_journal_nor_queue() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().unwrap();
    touch(&dir.path().join("a.pdf"), "body");

    let report =
        run(&profile(dir.path()), &Fixed::new("Documents", 0.99), &options(Mode::DryRun), &store);

    assert_eq!(report.would_move(), 1);
    assert!(dir.path().join("a.pdf").exists());
    assert!(!dir.path().join("Documents").exists());
    assert!(store.managed_dirs("downloads").unwrap().is_empty(), "nothing was executed to record");
    assert!(store.unfinished_operations().unwrap().is_empty());
}

#[test]
fn a_dry_run_still_reports_what_would_need_a_human() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().unwrap();
    touch(&dir.path().join("a.pdf"), "body");

    let report =
        run(&profile(dir.path()), &Fixed::new("Documents", 0.10), &options(Mode::DryRun), &store);
    assert!(report.needs_attention(), "a preview that hides pending work is not a preview");
}

// --- the journal ------------------------------------------------------------

#[test]
fn every_executed_move_leaves_a_completed_journal_entry() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().unwrap();
    for name in ["a.pdf", "b.pdf", "c.pdf"] {
        touch(&dir.path().join(name), name);
    }

    let report =
        run(&profile(dir.path()), &Fixed::new("Documents", 0.99), &options(Mode::Execute), &store);

    assert_eq!(report.moved(), 3);
    assert!(
        store.unfinished_operations().unwrap().is_empty(),
        "no operation was left half-recorded"
    );
    assert_eq!(store.managed_dirs("downloads").unwrap(), [dir.path().join("Documents")]);
}

#[test]
fn a_file_that_vanishes_mid_run_is_reported_not_fabricated() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().unwrap();
    let path = dir.path().join("a.pdf");
    touch(&path, "body");

    // Scan sees it; by decision time it is gone.
    let p = profile(dir.path());
    let scanned = bower_core::scan::scan(&p, &ScanOptions::default()).unwrap();
    assert_eq!(scanned.files.len(), 1);
    fs::remove_file(&path).unwrap();

    let report = run(&p, &Fixed::new("Documents", 0.99), &options(Mode::Execute), &store);
    assert_eq!(report.moved(), 0);
    assert_eq!(report.errors(), 0, "a vanished file is an expected outcome, not an error");
}

#[test]
fn the_holding_folder_is_never_scanned_as_input() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("downloads");
    // Deliberately nested inside the scanned tree.
    let holding = source.join("_review");
    let store = Store::open_in_memory().unwrap();
    touch(&source.join("a.pdf"), "body");

    let mut opts = options(Mode::Execute);
    opts.review_placement = ReviewPlacement::Quarantine;
    opts.quarantine_dir = Some(holding.clone());
    opts.scan.extra_excluded_roots = vec![holding.clone()];

    run(&profile(&source), &Fixed::new("Documents", 0.10), &opts, &store);
    assert!(holding.join("downloads/a.pdf").exists());

    let second = run(&profile(&source), &Fixed::new("Documents", 0.10), &opts, &store);
    assert_eq!(second.scanned, 0, "a parked file must not be picked up again");
}

// --- journal provenance (ADR-0005) ------------------------------------------

#[test]
fn an_automatic_move_records_the_model_and_the_confidence_that_cleared_the_gate() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().unwrap();
    touch(&dir.path().join("a.pdf"), "body");

    let report =
        run(&profile(dir.path()), &Fixed::new("Documents", 0.93), &options(Mode::Execute), &store);
    assert_eq!(report.moved(), 1);

    let rows = store.journal_recent(None, 10).unwrap();
    assert!(!rows.is_empty(), "an executed move must be journalled");
    for row in &rows {
        assert_eq!(row.provenance.origin, Origin::Model);
        assert_eq!(
            row.provenance.decided_by,
            DecidedBy::Auto,
            "nothing asked a human, so this must not claim one approved it"
        );
        assert_eq!(
            row.provenance.confidence,
            Some(0.93),
            "the confidence that cleared the gate is the fact worth keeping"
        );
    }
}

#[test]
fn a_dry_run_journals_nothing_at_all() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().unwrap();
    touch(&dir.path().join("a.pdf"), "body");

    run(&profile(dir.path()), &Fixed::new("Documents", 0.93), &options(Mode::DryRun), &store);
    assert!(
        store.journal_recent(None, 10).unwrap().is_empty(),
        "a dry run performs no operation, so it has nothing to record"
    );
}

#[test]
fn an_approved_item_is_journalled_as_a_human_decision_not_an_automatic_one() {
    // The distinction the rule engine and learned-corrections both need: the
    // model proposed this, but it did not clear the gate on its own -- a person
    // let it through. `action` is identical either way.
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().unwrap();
    touch(&dir.path().join("a.pdf"), "body");

    let p = profile(dir.path());
    let report = run(&p, &Fixed::new("Documents", 0.10), &options(Mode::Execute), &store);
    assert_eq!(report.newly_queued(), 1, "0.10 is below the threshold, so it queues");
    assert!(store.journal_recent(None, 10).unwrap().is_empty(), "queuing executes nothing");

    let item = store.review_list(None, None).unwrap().pop().unwrap();
    bower_core::review::approve(
        &store,
        &item,
        &p,
        &bower_core::review::ResolveOptions { mode: Mode::Execute, recycle_dir: None },
    )
    .unwrap();

    let rows = store.journal_recent(None, 10).unwrap();
    assert!(!rows.is_empty(), "approving executes the move, which is journalled");
    for row in &rows {
        assert_eq!(row.provenance.origin, Origin::Model, "a model still proposed it");
        assert_eq!(row.provenance.decided_by, DecidedBy::Human, "but a person allowed it");
        assert_eq!(row.provenance.confidence, Some(0.10));
    }
}
