#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::duration_suboptimal_units
)]
//! Policy engine behaviour.
//!
//! Every one of these runs without touching a filesystem, which is the point of
//! keeping the engine pure: the safety guarantees can be tested exhaustively
//! and adversarially, at unit-test speed.

use bower_config::{Metadata, OnConflict, Profile, Rename};
use bower_core::model::{
    DeleteProposal, FileFacts, FileId, FileRecord, NoOpReason, Proposal, ProposalOutcome,
    RawProposal, ResolvedAction,
};
use bower_core::policy::{self, Decision, Occupancy, PlanInput};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

// --- fixtures ---------------------------------------------------------------

const SRC: &str = "/data/downloads";
const DEST: &str = "/data/organized";

fn profile() -> Profile {
    Profile {
        name: "downloads".to_owned(),
        path: PathBuf::from(SRC),
        description: "test".to_owned(),
        enabled: true,
        llm_backend: "local".to_owned(),
        destination_root: PathBuf::from(DEST),
        categories: vec!["Documents".to_owned(), "Images".to_owned()],
        allow_dynamic_categories: false,
        allow_delete_suggestions: false,
        batch_size: 25,
        confidence_threshold: 0.75,
        on_conflict: OnConflict::Quarantine,
        stability_wait: Duration::ZERO,
        exclude_patterns: vec![],
        include_subdirs: false,
        rename: Rename::Disabled,
        metadata: Metadata {
            detect_mime: true,
            extract_exif: false,
            extract_audio_tags: false,
            extract_pdf_metadata: false,
            content_sniff_bytes: 0,
        },
    }
}

fn file(name: &str) -> FileRecord {
    let path = Path::new(SRC).join(name);
    FileRecord {
        id: FileId::for_path(&path),
        relative: PathBuf::from(name),
        facts: FileFacts { size: 42, mtime: SystemTime::UNIX_EPOCH },
        extension: path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase),
        mime: None,
        content_snippet: None,
        path,
    }
}

fn proposal(f: &FileRecord, category: &str, confidence: f32) -> ProposalOutcome {
    ProposalOutcome::Ok(Proposal::Categorize(RawProposal {
        file_id: f.id.clone(),
        category: category.to_owned(),
        is_new_category: false,
        name_tokens: BTreeMap::new(),
        confidence,
        reasoning: "because".to_owned(),
    }))
}

/// Runs the engine to a terminal action, answering every collision check with
/// `found`.
fn resolve(
    p: &Profile,
    f: &FileRecord,
    outcome: &ProposalOutcome,
    found: Occupancy,
) -> ResolvedAction {
    resolve_with(p, f, outcome, |_| found)
}

/// As [`resolve`], but the answer may depend on which candidate path is being
/// checked -- needed to exercise the suffix walk.
fn resolve_with(
    p: &Profile,
    f: &FileRecord,
    outcome: &ProposalOutcome,
    mut answer: impl FnMut(&Path) -> Occupancy,
) -> ResolvedAction {
    let mut decision =
        policy::plan(&PlanInput { file: f, outcome, profile: p, observed: Some(f.facts) });
    for _ in 0..200 {
        match decision {
            Decision::Final(a) => return a,
            Decision::CheckCollision(pending) => {
                let found = answer(pending.dest.as_path());
                decision = policy::resolve_collision(&pending, found);
            }
        }
    }
    panic!("collision resolution did not terminate");
}

fn assert_review(action: &ResolvedAction, expect: &str) {
    match action {
        ResolvedAction::NeedsManualReview { reason, .. } => {
            assert!(
                reason.to_lowercase().contains(&expect.to_lowercase()),
                "expected a reason mentioning `{expect}`, got `{reason}`"
            );
        }
        other => panic!("expected manual review mentioning `{expect}`, got {other:?}"),
    }
}

// --- stage 1: schema --------------------------------------------------------

#[test]
fn malformed_output_goes_to_review_with_the_detail_attached() {
    let f = file("a.pdf");
    let outcome = ProposalOutcome::Malformed { detail: "expected object, found array".to_owned() };
    assert_review(&resolve(&profile(), &f, &outcome, Occupancy::Vacant), "expected object");
}

#[test]
fn a_file_the_model_never_mentioned_goes_to_review() {
    let f = file("a.pdf");
    assert_review(
        &resolve(&profile(), &f, &ProposalOutcome::Missing, Occupancy::Vacant),
        "no proposal",
    );
}

#[test]
fn confidence_outside_the_unit_interval_is_a_schema_violation() {
    let f = file("a.pdf");
    for bad in [1.5, -0.2, f32::NAN, f32::INFINITY] {
        let outcome = proposal(&f, "Documents", bad);
        assert_review(&resolve(&profile(), &f, &outcome, Occupancy::Vacant), "confidence");
    }
}

// --- stage 2: staleness -----------------------------------------------------

#[test]
fn a_file_that_changed_since_the_scan_is_left_alone() {
    let f = file("a.pdf");
    let outcome = proposal(&f, "Documents", 0.99);
    let changed = FileFacts { size: 43, ..f.facts };
    let decision = policy::plan(&PlanInput {
        file: &f,
        outcome: &outcome,
        profile: &profile(),
        observed: Some(changed),
    });
    assert_eq!(decision.action(), Some(&ResolvedAction::NoOp { reason: NoOpReason::Stale }));
}

#[test]
fn a_file_that_vanished_since_the_scan_is_left_alone() {
    let f = file("a.pdf");
    let outcome = proposal(&f, "Documents", 0.99);
    let decision = policy::plan(&PlanInput {
        file: &f,
        outcome: &outcome,
        profile: &profile(),
        observed: None,
    });
    assert_eq!(decision.action(), Some(&ResolvedAction::NoOp { reason: NoOpReason::Stale }));
}

// --- stage 3: category ------------------------------------------------------

#[test]
fn an_undeclared_category_is_refused_when_the_taxonomy_is_fixed() {
    let f = file("a.pdf");
    let outcome = proposal(&f, "Invoices", 0.99);
    assert_review(&resolve(&profile(), &f, &outcome, Occupancy::Vacant), "not declared");
}

#[test]
fn an_undeclared_category_is_accepted_when_dynamic_categories_are_allowed() {
    let mut p = profile();
    p.allow_dynamic_categories = true;
    let f = file("a.pdf");
    let outcome = proposal(&f, "Invoices", 0.99);

    match resolve(&p, &f, &outcome, Occupancy::Vacant) {
        ResolvedAction::Move { dest } => assert_eq!(dest.category(), "Invoices"),
        other => panic!("expected a move, got {other:?}"),
    }
}

#[test]
fn casing_wobble_maps_onto_the_declared_spelling() {
    // Otherwise `documents/` and `Documents/` would both accumulate files.
    let f = file("a.pdf");
    for spelling in ["documents", "DOCUMENTS", "DoCuMeNtS"] {
        let outcome = proposal(&f, spelling, 0.99);
        match resolve(&profile(), &f, &outcome, Occupancy::Vacant) {
            ResolvedAction::Move { dest } => assert_eq!(dest.category(), "Documents"),
            other => panic!("{spelling}: expected a move, got {other:?}"),
        }
    }
}

#[test]
fn a_hostile_category_never_reaches_path_construction() {
    let mut p = profile();
    p.allow_dynamic_categories = true; // the permissive case, on purpose
    let f = file("a.pdf");

    for hostile in ["../../etc", "..", "/etc", ".ssh", "a/b", "", "   ", "con:", "x\0y"] {
        let outcome = proposal(&f, hostile, 0.99);
        let action = resolve(&p, &f, &outcome, Occupancy::Vacant);
        assert!(
            matches!(action, ResolvedAction::NeedsManualReview { .. }),
            "category {hostile:?} should have been refused, got {action:?}"
        );
    }
}

#[test]
fn is_new_category_is_advisory_and_cannot_widen_what_is_allowed() {
    // A model claiming a category is not new does not make it declared.
    let f = file("a.pdf");
    let outcome = ProposalOutcome::Ok(Proposal::Categorize(RawProposal {
        file_id: f.id.clone(),
        category: "Invoices".to_owned(),
        is_new_category: false,
        name_tokens: BTreeMap::new(),
        confidence: 0.99,
        reasoning: String::new(),
    }));
    assert_review(&resolve(&profile(), &f, &outcome, Occupancy::Vacant), "not declared");
}

// --- stage 4: filename ------------------------------------------------------

#[test]
fn renaming_off_keeps_the_original_name() {
    let f = file("Some Report (final).pdf");
    let outcome = proposal(&f, "Documents", 0.99);
    match resolve(&profile(), &f, &outcome, Occupancy::Vacant) {
        ResolvedAction::Move { dest } => assert_eq!(dest.filename(), "Some Report (final).pdf"),
        other => panic!("expected a move, got {other:?}"),
    }
}

#[test]
fn renaming_on_fills_the_template_and_reports_the_rename() {
    let mut p = profile();
    p.rename = Rename::Enabled { template: "{date}-{doc_type}-{vendor}{ext}".to_owned() };
    let f = file("scan001.pdf");

    let outcome = ProposalOutcome::Ok(Proposal::Categorize(RawProposal {
        file_id: f.id.clone(),
        category: "Documents".to_owned(),
        is_new_category: false,
        name_tokens: [("date", "2024-03-15"), ("doc_type", "invoice"), ("vendor", "Acme Corp")]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect(),
        confidence: 0.99,
        reasoning: String::new(),
    }));

    match resolve(&p, &f, &outcome, Occupancy::Vacant) {
        ResolvedAction::MoveAndRename { dest } => {
            assert_eq!(dest.filename(), "2024-03-15-invoice-Acme-Corp.pdf");
        }
        other => panic!("expected a rename, got {other:?}"),
    }
}

#[test]
fn a_template_the_model_could_not_fill_goes_to_review() {
    let mut p = profile();
    p.rename = Rename::Enabled { template: "{date}-{vendor}{ext}".to_owned() };
    let f = file("scan001.pdf");
    let outcome = proposal(&f, "Documents", 0.99); // no tokens at all
    assert_review(&resolve(&p, &f, &outcome, Occupancy::Vacant), "template");
}

// --- stage 5/6: paths and collisions ---------------------------------------

#[test]
fn a_file_already_at_its_destination_is_left_alone() {
    let mut p = profile();
    p.destination_root = PathBuf::from(SRC); // in-place
    let mut f = file("a.pdf");
    f.path = Path::new(SRC).join("Documents").join("a.pdf");

    let outcome = proposal(&f, "Documents", 0.99);
    assert_eq!(
        resolve(&p, &f, &outcome, Occupancy::Vacant),
        ResolvedAction::NoOp { reason: NoOpReason::AlreadyInPlace }
    );
}

#[test]
fn identical_content_at_the_destination_is_a_duplicate_not_a_conflict() {
    let f = file("a.pdf");
    let outcome = proposal(&f, "Documents", 0.99);
    assert_eq!(
        resolve(&profile(), &f, &outcome, Occupancy::Identical),
        ResolvedAction::NoOp { reason: NoOpReason::DuplicateOfDestination }
    );
}

#[test]
fn a_duplicate_is_recognised_even_below_the_confidence_threshold() {
    // The collision check runs ahead of the confidence gate precisely so that
    // nobody is asked to adjudicate a file that is already filed.
    let f = file("a.pdf");
    let outcome = proposal(&f, "Documents", 0.01);
    assert_eq!(
        resolve(&profile(), &f, &outcome, Occupancy::Identical),
        ResolvedAction::NoOp { reason: NoOpReason::DuplicateOfDestination }
    );
}

#[test]
fn different_content_is_never_overwritten() {
    let f = file("a.pdf");
    let outcome = proposal(&f, "Documents", 0.99);

    let mut skip = profile();
    skip.on_conflict = OnConflict::Skip;
    assert_eq!(
        resolve(&skip, &f, &outcome, Occupancy::Different),
        ResolvedAction::NoOp { reason: NoOpReason::ConflictSkipped }
    );

    let quarantine = profile();
    assert!(matches!(
        resolve(&quarantine, &f, &outcome, Occupancy::Different),
        ResolvedAction::Quarantine { .. }
    ));
}

#[test]
fn suffix_walks_to_the_first_free_name() {
    let mut p = profile();
    p.on_conflict = OnConflict::Suffix;
    let f = file("a.pdf");
    let outcome = proposal(&f, "Documents", 0.99);

    let action = resolve_with(&p, &f, &outcome, |dest| {
        if dest.ends_with("a-3.pdf") { Occupancy::Vacant } else { Occupancy::Different }
    });

    match action {
        ResolvedAction::MoveAndRename { dest } => assert_eq!(dest.filename(), "a-3.pdf"),
        other => panic!("expected a suffixed move, got {other:?}"),
    }
}

#[test]
fn an_endless_suffix_walk_gives_up_and_asks_a_human() {
    let mut p = profile();
    p.on_conflict = OnConflict::Suffix;
    let f = file("a.pdf");
    let outcome = proposal(&f, "Documents", 0.99);

    assert!(matches!(
        resolve(&p, &f, &outcome, Occupancy::Different),
        ResolvedAction::Quarantine { .. }
    ));
}

// --- stage 7: confidence ----------------------------------------------------

#[test]
fn confidence_below_the_threshold_goes_to_review() {
    let f = file("a.pdf");
    let outcome = proposal(&f, "Documents", 0.74);
    assert_review(&resolve(&profile(), &f, &outcome, Occupancy::Vacant), "threshold");
}

#[test]
fn confidence_at_the_threshold_passes() {
    let f = file("a.pdf");
    let outcome = proposal(&f, "Documents", 0.75);
    assert!(matches!(
        resolve(&profile(), &f, &outcome, Occupancy::Vacant),
        ResolvedAction::Move { .. }
    ));
}

#[test]
fn a_file_held_back_by_the_confidence_gate_carries_its_proposal_to_review() {
    let f = file("a.pdf");
    let outcome = proposal(&f, "Documents", 0.10);
    match resolve(&profile(), &f, &outcome, Occupancy::Vacant) {
        ResolvedAction::NeedsManualReview { raw, .. } => {
            assert!(
                matches!(*raw, ProposalOutcome::Ok(Proposal::Categorize(_))),
                "review needs the model's own output, not just a threshold complaint"
            );
        }
        other => panic!("expected review, got {other:?}"),
    }
}

// --- deletion ---------------------------------------------------------------

fn delete(f: &FileRecord, confidence: f32) -> ProposalOutcome {
    ProposalOutcome::Ok(Proposal::SuggestDelete(DeleteProposal {
        file_id: f.id.clone(),
        reason: "looks like a duplicate installer".to_owned(),
        confidence,
    }))
}

#[test]
fn a_deletion_suggestion_is_refused_outright_when_the_profile_forbids_it() {
    let f = file("a.pdf");
    assert_review(
        &resolve(&profile(), &f, &delete(&f, 0.99), Occupancy::Vacant),
        "allow_delete_suggestions",
    );
}

#[test]
fn a_deletion_suggestion_never_executes_at_any_confidence() {
    let mut p = profile();
    p.allow_delete_suggestions = true;
    p.confidence_threshold = 0.1; // as permissive as configuration allows
    let f = file("a.pdf");

    for confidence in [0.0, 0.5, 0.99, 1.0] {
        let action = resolve(&p, &f, &delete(&f, confidence), Occupancy::Vacant);
        assert!(
            matches!(action, ResolvedAction::RecycleSuggested { .. }),
            "confidence {confidence} produced {action:?}; deletion must always await a human"
        );
        assert!(!action.is_automatic());
        assert!(action.needs_attention());
    }
}

// --- the invariant the whole design rests on --------------------------------

#[test]
fn no_action_the_engine_can_produce_is_both_automatic_and_outside_the_root() {
    // Exhaustive over the closed action set: whatever the engine returns, an
    // action the executor will perform unattended always carries a destination
    // under the profile's root.
    let f = file("a.pdf");
    let mut p = profile();
    p.allow_dynamic_categories = true;
    p.allow_delete_suggestions = true;

    for category in ["Documents", "Invoices", "../etc", "..", "/tmp"] {
        for confidence in [0.0, 0.5, 1.0] {
            for found in [Occupancy::Vacant, Occupancy::Identical, Occupancy::Different] {
                let outcome = proposal(&f, category, confidence);
                let action = resolve(&p, &f, &outcome, found);
                if let Some(dest) = action.dest() {
                    assert!(
                        dest.as_path().starts_with(DEST),
                        "{category}/{confidence}/{found:?} produced {dest:?}"
                    );
                }
                if action.is_automatic() {
                    assert!(action.dest().is_some(), "an automatic action must name a destination");
                }
            }
        }
    }
}
