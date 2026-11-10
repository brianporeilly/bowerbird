#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::duration_suboptimal_units
)]
//! The rule-based fast path: matching, bypassing the model, and provenance.

use bower_config::{Metadata, OnConflict, Profile, Rename, ReviewPlacement, Rule};
use bower_core::context::BatchContext;
use bower_core::exec::Mode;
use bower_core::hash;
use bower_core::llm::{BatchResponse, LlmBackend, LlmError};
use bower_core::model::{FileFacts, FileId, FileRecord, Proposal, ProposalOutcome, RawProposal};
use bower_core::review::{self, ResolveOptions};
use bower_core::rules::RuleSet;
use bower_core::run::{RunOptions, run_profile};
use bower_core::scan::ScanOptions;
use bower_core::state::{DecidedBy, NewReviewItem, Origin, ReviewKind, Store};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

/// Files the model was asked about, so a test can assert it was never asked.
struct Recording {
    category: String,
    asked: Mutex<Vec<String>>,
}

impl Recording {
    fn new(category: &str) -> Self {
        Self { category: category.to_owned(), asked: Mutex::new(Vec::new()) }
    }
    fn asked(&self) -> Vec<String> {
        let mut v = self.asked.lock().unwrap().clone();
        v.sort();
        v
    }
}

impl LlmBackend for Recording {
    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "recording"
    }

    fn classify(&self, ctx: &BatchContext) -> Result<BatchResponse, LlmError> {
        let mut outcomes = BTreeMap::new();
        for file in &ctx.files {
            self.asked.lock().unwrap().push(file.file_name.clone());
            outcomes.insert(
                file.file_id.clone(),
                ProposalOutcome::Ok(Proposal::Categorize(RawProposal {
                    file_id: file.file_id.clone(),
                    category: self.category.clone(),
                    is_new_category: false,
                    name_tokens: BTreeMap::new(),
                    confidence: 0.9,
                    reasoning: "model".to_owned(),
                })),
            );
        }
        Ok(BatchResponse { outcomes })
    }
}

fn rule(name: &str, category: &str) -> Rule {
    Rule {
        name: name.to_owned(),
        category: category.to_owned(),
        extensions: vec![],
        name_glob: None,
        mime: None,
    }
}

fn by_ext(name: &str, category: &str, exts: &[&str]) -> Rule {
    Rule { extensions: exts.iter().map(|e| (*e).to_owned()).collect(), ..rule(name, category) }
}

fn profile(path: &Path, rules: Vec<Rule>) -> Profile {
    Profile {
        name: "downloads".to_owned(),
        path: path.to_path_buf(),
        description: String::new(),
        enabled: true,
        llm_backend: "test".to_owned(),
        destination_root: path.to_path_buf(),
        categories: vec!["Documents".to_owned(), "Downloads".to_owned()],
        allow_dynamic_categories: false,
        allow_delete_suggestions: false,
        batch_size: 25,
        confidence_threshold: 0.75,
        on_conflict: OnConflict::Quarantine,
        stability_wait: Duration::ZERO,
        exclude_patterns: vec![],
        include_subdirs: false,
        rename: Rename::Disabled,
        rules,
        metadata: Metadata {
            detect_mime: false,
            extract_exif: false,
            extract_audio_tags: false,
            extract_pdf_metadata: false,
            content_sniff_bytes: 0,
        },
    }
}

fn options() -> RunOptions {
    RunOptions {
        mode: Mode::Execute,
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

fn record(name: &str, mime: Option<&str>) -> FileRecord {
    let path = PathBuf::from("/inbox").join(name);
    FileRecord {
        id: FileId::for_path(&path),
        relative: PathBuf::from(name),
        extension: Path::new(name)
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase),
        path,
        facts: FileFacts { size: 1, mtime: SystemTime::UNIX_EPOCH },
        mime: mime.map(str::to_owned),
        content_snippet: None,
    }
}

fn first<'a>(set: &RuleSet<'a>, name: &str, mime: Option<&str>) -> Option<&'a str> {
    set.first_match(&record(name, mime)).map(|r| r.name.as_str())
}

// --- matching ----------------------------------------------------------------

#[test]
fn an_extension_rule_matches_regardless_of_case_and_nothing_else() {
    let rules = [by_ext("usenet", "Downloads", &["nzb", "torrent"])];
    let set = RuleSet::compile(&rules).unwrap();

    assert_eq!(first(&set, "show.nzb", None), Some("usenet"));
    assert_eq!(first(&set, "SHOW.NZB", None), Some("usenet"));
    assert_eq!(first(&set, "linux.torrent", None), Some("usenet"));
    assert_eq!(first(&set, "show.nzb.txt", None), None);
    assert_eq!(first(&set, "nzb", None), None, "no extension is not the extension `nzb`");
}

#[test]
fn a_glob_matches_the_file_name_case_insensitively() {
    let rules = [Rule { name_glob: Some("*coloring*".into()), ..rule("coloring", "Downloads") }];
    let set = RuleSet::compile(&rules).unwrap();

    assert_eq!(first(&set, "US_Coloring_v02.pdf", None), Some("coloring"));
    assert_eq!(first(&set, "state of alaska free coloring page.pdf", None), Some("coloring"));
    assert_eq!(first(&set, "invoice.pdf", None), None);
}

#[test]
fn a_mime_family_matches_every_subtype_but_not_another_family() {
    let rules = [Rule { mime: Some("image/*".into()), ..rule("pictures", "Downloads") }];
    let set = RuleSet::compile(&rules).unwrap();

    assert_eq!(first(&set, "a.bin", Some("image/png")), Some("pictures"));
    assert_eq!(first(&set, "b.bin", Some("image/jpeg")), Some("pictures"));
    assert_eq!(first(&set, "c.bin", Some("application/pdf")), None);
    assert_eq!(first(&set, "d.png", None), None, "an unknown MIME type never matches a MIME rule");
}

#[test]
fn every_criterion_of_a_rule_must_hold() {
    let rules = [Rule {
        extensions: vec!["pdf".into()],
        name_glob: Some("scan*".into()),
        ..rule("scans", "Documents")
    }];
    let set = RuleSet::compile(&rules).unwrap();

    assert_eq!(first(&set, "scan-001.pdf", None), Some("scans"));
    assert_eq!(first(&set, "scan-001.png", None), None, "right name, wrong extension");
    assert_eq!(first(&set, "invoice.pdf", None), None, "right extension, wrong name");
}

#[test]
fn the_first_matching_rule_wins() {
    let rules = [by_ext("first", "Documents", &["pdf"]), by_ext("second", "Downloads", &["pdf"])];
    let set = RuleSet::compile(&rules).unwrap();
    assert_eq!(first(&set, "a.pdf", None), Some("first"));
}

#[test]
fn a_rule_with_no_criteria_matches_nothing_rather_than_everything() {
    // Config validation refuses this, so it can only arrive by hand. The point
    // is that a mistake here must not become "move every file".
    let rules = [rule("empty", "Documents")];
    let set = RuleSet::compile(&rules).unwrap();
    assert_eq!(first(&set, "a.pdf", None), None);
}

// --- the pipeline ------------------------------------------------------------

#[test]
fn a_matching_file_is_filed_without_the_model_ever_seeing_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().unwrap();
    touch(&dir.path().join("show.nzb"), "nzb");
    touch(&dir.path().join("letter.txt"), "dear sir");

    let p = profile(dir.path(), vec![by_ext("usenet", "Downloads", &["nzb"])]);
    let backend = Recording::new("Documents");
    let report = run_profile(&p, &backend, &options(), &store).unwrap();

    assert_eq!(report.moved(), 2);
    assert_eq!(report.by_rule(), 1);
    assert_eq!(backend.asked(), vec!["letter.txt"], "the model must only be asked about the rest");
    assert!(dir.path().join("Downloads/show.nzb").exists());
    assert!(dir.path().join("Documents/letter.txt").exists());
}

#[test]
fn when_every_file_matches_a_rule_the_model_is_not_called_at_all() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().unwrap();
    touch(&dir.path().join("a.nzb"), "1");
    touch(&dir.path().join("b.nzb"), "2");

    let p = profile(dir.path(), vec![by_ext("usenet", "Downloads", &["nzb"])]);
    let backend = Recording::new("Documents");
    let report = run_profile(&p, &backend, &options(), &store).unwrap();

    assert_eq!(report.moved(), 2);
    assert!(backend.asked().is_empty(), "no file needed the model, so it must not be called");
}

#[test]
fn a_profile_with_no_rules_sends_everything_to_the_model_as_before() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().unwrap();
    touch(&dir.path().join("a.nzb"), "1");

    let backend = Recording::new("Documents");
    let report = run_profile(&profile(dir.path(), vec![]), &backend, &options(), &store).unwrap();

    assert_eq!(report.by_rule(), 0);
    assert_eq!(backend.asked(), vec!["a.nzb"]);
}

#[test]
fn a_rule_move_is_journalled_as_a_rule_decision_with_no_invented_confidence() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().unwrap();
    touch(&dir.path().join("show.nzb"), "nzb");

    let p = profile(dir.path(), vec![by_ext("usenet", "Downloads", &["nzb"])]);
    run_profile(&p, &Recording::new("Documents"), &options(), &store).unwrap();

    let rows = store.journal_recent(None, 10).unwrap();
    assert!(!rows.is_empty(), "the move must have been journalled");
    for row in &rows {
        assert_eq!(row.provenance.origin, Origin::Rule);
        assert_eq!(row.provenance.decided_by, DecidedBy::Auto);
        assert_eq!(
            row.provenance.confidence, None,
            "a rule is deterministic; recording 1.0 would pass off a constant as a measurement"
        );
    }
}

#[test]
fn a_rule_goes_through_the_same_collision_handling_as_a_model() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().unwrap();
    touch(&dir.path().join("show.nzb"), "new");
    touch(&dir.path().join("Downloads/show.nzb"), "different, already filed");

    let p = profile(dir.path(), vec![by_ext("usenet", "Downloads", &["nzb"])]);
    let report = run_profile(&p, &Recording::new("Documents"), &options(), &store).unwrap();

    assert_eq!(report.moved(), 0, "a rule must never overwrite");
    assert_eq!(
        fs::read_to_string(dir.path().join("Downloads/show.nzb")).unwrap(),
        "different, already filed"
    );
    let queued = store.review_list(None, None).unwrap();
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].origin, Origin::Rule, "the queue must remember a rule asked for this");
    assert_eq!(queued[0].confidence, None);
}

#[test]
fn a_rule_naming_a_category_the_profile_does_not_allow_is_held_not_created() {
    // Config validation refuses this too; a hand-built profile shows the policy
    // engine does not rely on that.
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().unwrap();
    touch(&dir.path().join("show.nzb"), "nzb");

    let p = profile(dir.path(), vec![by_ext("usenet", "NotDeclared", &["nzb"])]);
    let report = run_profile(&p, &Recording::new("Documents"), &options(), &store).unwrap();

    assert_eq!(report.moved(), 0);
    assert!(!dir.path().join("NotDeclared").exists());
    assert_eq!(report.attention_count(), 1);
}

#[test]
fn approving_a_rules_queued_proposal_is_journalled_as_the_rules_not_the_models() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().unwrap();
    let file = dir.path().join("show.nzb");
    touch(&file, "nzb");
    let digest = hash::file_sha256(&file).unwrap();
    let proposed = dir.path().join("Downloads/show.nzb");

    store
        .enqueue_review(&NewReviewItem {
            profile: "downloads",
            kind: ReviewKind::Review,
            path: &file,
            original_path: &file,
            file_hash: &digest,
            category: "Downloads",
            proposed_dest: Some(&proposed),
            reasoning: "matched rule `usenet`",
            confidence: None,
            reason: "queued for the test",
            origin: Origin::Rule,
        })
        .unwrap();

    let p = profile(dir.path(), vec![]);
    let item = store.review_list(None, None).unwrap().pop().unwrap();
    assert_eq!(item.origin, Origin::Rule);
    review::approve(&store, &item, &p, &ResolveOptions { mode: Mode::Execute, recycle_dir: None })
        .unwrap();

    let rows = store.journal_recent(None, 10).unwrap();
    assert!(!rows.is_empty(), "the move must have been journalled");
    for row in &rows {
        assert_eq!(row.provenance.origin, Origin::Rule, "a rule proposed it");
        assert_eq!(row.provenance.decided_by, DecidedBy::Human, "a person approved it");
        assert_eq!(row.provenance.confidence, None);
    }
}
