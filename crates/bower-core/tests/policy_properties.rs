#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::duration_suboptimal_units,
    // Adversarial paths are exactly where Debug's escaping earns its keep:
    // a path containing a newline or NUL must not print as though it were clean.
    clippy::unnecessary_debug_formatting
)]
//! Property tests for the guarantee the whole design exists to provide:
//!
//! **No model output, however hostile, can produce a destination outside the
//! profile's `destination_root`.**
//!
//! The policy engine is pure, so this can be hammered with adversarial input at
//! unit-test speed and with no filesystem in the loop.

use bower_config::{Metadata, OnConflict, Profile, Rename};
use bower_core::model::{
    FileFacts, FileId, FileRecord, Proposal, ProposalOutcome, RawProposal, ResolvedAction,
};
use bower_core::policy::{self, Decision, Occupancy, PlanInput, PriorRejections};
use proptest::prelude::*;
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

const SRC: &str = "/data/downloads";
const DEST: &str = "/data/organized";

/// Strings chosen to break path handling, mixed into the random corpus so the
/// generator spends real time on the cases that matter.
const HOSTILE: &[&str] = &[
    "..",
    "../..",
    "../../etc/passwd",
    "/etc/passwd",
    "/",
    "//",
    ".",
    ".ssh",
    ".git",
    "a/b",
    "a\\b",
    "C:\\Windows\\System32",
    "\0",
    "a\0b",
    "con",
    "nul",
    "  ",
    "",
    "\u{202e}txt.exe",
    "....//....//etc",
    "%2e%2e%2f",
    "~/.bashrc",
    "$HOME",
    "\n../\n",
];

fn nasty_string() -> impl Strategy<Value = String> {
    prop_oneof![
        4 => prop::sample::select(HOSTILE).prop_map(str::to_owned),
        1 => (1usize..600).prop_map(|n| "x".repeat(n)),
        3 => "[\\PC]{0,40}",
        2 => "[a-zA-Z0-9 ._-]{0,30}",
        1 => prop::collection::vec(prop::sample::select(HOSTILE), 1..4)
             .prop_map(|parts| parts.join("/")),
    ]
}

fn profile(rename: Rename, dynamic: bool, on_conflict: OnConflict) -> Profile {
    Profile {
        name: "downloads".to_owned(),
        path: PathBuf::from(SRC),
        description: String::new(),
        enabled: true,
        llm_backend: "local".to_owned(),
        destination_root: PathBuf::from(DEST),
        categories: vec!["Documents".to_owned(), "Images".to_owned()],
        allow_dynamic_categories: dynamic,
        allow_delete_suggestions: true,
        batch_size: 25,
        confidence_threshold: 0.75,
        on_conflict,
        stability_wait: Duration::ZERO,
        exclude_patterns: vec![],
        include_subdirs: false,
        rename,
        metadata: Metadata {
            detect_mime: true,
            extract_exif: false,
            extract_audio_tags: false,
            extract_pdf_metadata: false,
            content_sniff_bytes: 0,
        },
    }
}

fn record(name: &str) -> FileRecord {
    let path = Path::new(SRC).join(name);
    FileRecord {
        id: FileId::for_path(&path),
        relative: PathBuf::from(name),
        facts: FileFacts { size: 1, mtime: SystemTime::UNIX_EPOCH },
        extension: path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase),
        mime: None,
        content_snippet: None,
        path,
    }
}

/// Every destination the engine hands out must satisfy all of these.
fn assert_contained(dest: &Path) {
    assert!(dest.starts_with(DEST), "escaped the destination root: {dest:?}");
    assert!(
        !dest.components().any(|c| c == Component::ParentDir),
        "contains a parent-dir component: {dest:?}"
    );

    let extra: Vec<_> = dest.strip_prefix(DEST).expect("checked above").components().collect();
    assert_eq!(
        extra.len(),
        2,
        "expected exactly category/filename below the root, got {extra:?} from {dest:?}"
    );
    for c in extra {
        assert!(
            matches!(c, Component::Normal(_)),
            "expected a plain path component, got {c:?} in {dest:?}"
        );
    }
}

/// Runs the engine to a terminal action, answering collision checks with
/// `found`, and checking containment at every intermediate candidate too.
fn drive(
    p: &Profile,
    f: &FileRecord,
    outcome: &ProposalOutcome,
    found: Occupancy,
) -> ResolvedAction {
    let mut decision = policy::plan(&PlanInput {
        file: f,
        outcome,
        profile: p,
        observed: Some(f.facts),
        rejected: PriorRejections::default(),
    });
    for _ in 0..300 {
        match decision {
            Decision::Final(action) => {
                if let Some(dest) = action.dest() {
                    assert_contained(dest.as_path());
                }
                return action;
            }
            Decision::CheckCollision(pending) => {
                assert_contained(pending.dest.as_path());
                decision = policy::resolve_collision(&pending, found);
            }
        }
    }
    panic!("collision resolution did not terminate");
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2048))]

    /// The core invariant, with renaming off.
    #[test]
    fn no_category_can_escape_the_destination_root(
        category in nasty_string(),
        filename in nasty_string(),
        confidence in 0.0f32..=1.0,
        dynamic in any::<bool>(),
    ) {
        let p = profile(Rename::Disabled, dynamic, OnConflict::Quarantine);
        let f = record(&filename);
        let outcome = ProposalOutcome::Ok(Proposal::Categorize(RawProposal {
            file_id: f.id.clone(),
            category,
            is_new_category: false,
            name_tokens: BTreeMap::new(),
            confidence,
            reasoning: String::new(),
        }));

        for found in [Occupancy::Vacant, Occupancy::Identical, Occupancy::Different] {
            drive(&p, &f, &outcome, found);
        }
    }

    /// The same, with renaming on -- model-supplied tokens now reach the
    /// filename, which is the wider attack surface of the two.
    #[test]
    fn no_filename_token_can_escape_the_destination_root(
        category in nasty_string(),
        date in nasty_string(),
        vendor in nasty_string(),
        doc_type in nasty_string(),
        filename in nasty_string(),
        confidence in 0.0f32..=1.0,
    ) {
        let p = profile(
            Rename::Enabled { template: "{date}-{doc_type}-{vendor}{ext}".to_owned() },
            true,
            OnConflict::Suffix,
        );
        let f = record(&filename);
        let name_tokens = [("date", date), ("vendor", vendor), ("doc_type", doc_type)]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect();

        let outcome = ProposalOutcome::Ok(Proposal::Categorize(RawProposal {
            file_id: f.id.clone(),
            category,
            is_new_category: false,
            name_tokens,
            confidence,
            reasoning: String::new(),
        }));

        for found in [Occupancy::Vacant, Occupancy::Different] {
            drive(&p, &f, &outcome, found);
        }
    }

    /// Whatever the model proposes, an action the executor performs unattended
    /// is always a move to a contained destination -- never a deletion, never a
    /// bare path.
    #[test]
    fn automatic_actions_are_only_ever_contained_moves(
        category in nasty_string(),
        filename in nasty_string(),
        confidence in 0.0f32..=1.0,
    ) {
        let p = profile(Rename::Disabled, true, OnConflict::Suffix);
        let f = record(&filename);
        let outcome = ProposalOutcome::Ok(Proposal::Categorize(RawProposal {
            file_id: f.id.clone(),
            category,
            is_new_category: true,
            name_tokens: BTreeMap::new(),
            confidence,
            reasoning: String::new(),
        }));

        let action = drive(&p, &f, &outcome, Occupancy::Vacant);
        if action.is_automatic() {
            let dest = action.dest().expect("an automatic action must name a destination");
            assert_contained(dest.as_path());
            prop_assert!(confidence >= p.confidence_threshold,
                "moved a file at confidence {confidence} below threshold {}",
                p.confidence_threshold);
        }
    }

    /// Renaming never produces a name that reintroduces a separator, whatever
    /// the tokens contained.
    #[test]
    fn rendered_filenames_stay_single_components(
        date in nasty_string(),
        vendor in nasty_string(),
        doc_type in nasty_string(),
    ) {
        let p = profile(
            Rename::Enabled { template: "{date}-{doc_type}-{vendor}{ext}".to_owned() },
            true,
            OnConflict::Quarantine,
        );
        let f = record("scan.pdf");
        let name_tokens = [("date", date), ("vendor", vendor), ("doc_type", doc_type)]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect();

        let outcome = ProposalOutcome::Ok(Proposal::Categorize(RawProposal {
            file_id: f.id.clone(),
            category: "Documents".to_owned(),
            is_new_category: false,
            name_tokens,
            confidence: 1.0,
            reasoning: String::new(),
        }));

        if let Some(dest) = drive(&p, &f, &outcome, Occupancy::Vacant).dest() {
            let name = dest.filename();
            prop_assert!(!name.contains('/'), "{name:?}");
            prop_assert!(!name.contains('\\'), "{name:?}");
            prop_assert!(!name.contains('\0'), "{name:?}");
            prop_assert!(name != "." && name != "..", "{name:?}");
            prop_assert!(name.len() <= 255, "{name:?}");
        }
    }
}
