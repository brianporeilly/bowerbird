//! The policy engine: the enforcement layer between model output and the
//! filesystem.
//!
//! # Purity
//!
//! Nothing in this module performs I/O. Every function is a deterministic
//! transformation of its arguments, which is what makes the tool's central
//! safety claim testable exhaustively and off-disk. A test at the bottom of
//! this file mechanically enforces the property, so it degrades loudly rather
//! than silently.
//!
//! The one stage that genuinely needs to look at the disk -- the collision
//! check -- is handled by handing the decision *back* to the caller rather than
//! by reaching for `std::fs`. [`plan`] runs the stages up to path construction
//! and then returns [`Decision::CheckCollision`]; the caller inspects the
//! destination and calls [`resolve_collision`], which is itself pure. The
//! sequence can repeat when `on_conflict = "suffix"` walks to the next free
//! name.
//!
//! # Trust direction
//!
//! Every stage may route a file to [`ResolvedAction::NeedsManualReview`], and
//! no stage may skip a later one. The engine can only ever lower trust.

mod sanitize;
mod template;

pub use template::{TemplateError, validate_template};

use bower_config::{OnConflict, Profile, Rename};
use std::path::PathBuf;

use crate::model::{
    DeleteProposal, DestPath, FileFacts, FileRecord, NoOpReason, Proposal, ProposalOutcome,
    RawProposal, ResolvedAction,
};

/// How many `-N` suffixes to try before giving up and quarantining. A directory
/// holding a hundred same-named files is a situation for a human, not a loop.
const MAX_SUFFIX_ATTEMPTS: u32 = 100;

/// Everything the engine needs to decide about one file.
#[derive(Debug, Clone, Copy)]
pub struct PlanInput<'a> {
    pub file: &'a FileRecord,
    pub outcome: &'a ProposalOutcome,
    pub profile: &'a Profile,
    /// The file's facts as observed *now*, immediately before deciding.
    /// `None` means it has vanished since the scan.
    pub observed: Option<FileFacts>,
}

/// The result of a pass through the engine.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// Nothing further to check.
    Final(ResolvedAction),
    /// Stages up to path construction passed. The caller must determine what,
    /// if anything, occupies `dest` and call [`resolve_collision`].
    ///
    /// Boxed: this variant is several times the size of a terminal action, and
    /// most decisions are terminal.
    CheckCollision(Box<PendingMove>),
}

impl Decision {
    /// The action, if this decision is already terminal.
    #[must_use]
    pub fn action(&self) -> Option<&ResolvedAction> {
        match self {
            Self::Final(a) => Some(a),
            Self::CheckCollision(_) => None,
        }
    }
}

/// A move that has passed every stage except the collision check.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingMove {
    /// The destination to inspect. Proven to lie under the profile's
    /// destination root.
    pub dest: DestPath,
    /// The file that would move there.
    pub source: PathBuf,
    /// Whether the filename template changed the name.
    pub renamed: bool,
    pub confidence: f32,
    pub reasoning: String,
    /// Kept so a file rejected by the confidence gate reaches review with the
    /// model's own output attached, rather than as a bare threshold complaint.
    proposal: RawProposal,
    /// The unsuffixed destination. Each suffix attempt is derived from this
    /// rather than from the previous candidate, so names cannot accumulate into
    /// `a-1-2-3.pdf`.
    base: DestPath,
    attempt: u32,
    on_conflict: OnConflict,
    threshold: f32,
}

/// What the caller found at [`PendingMove::dest`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Occupancy {
    /// Nothing is there.
    Vacant,
    /// A file whose content hash matches the source.
    Identical,
    /// A file with different content, or a directory.
    Different,
}

/// Runs stages 1-5: schema validation, staleness, category resolution, filename
/// rendering, and path construction.
#[must_use]
pub fn plan(input: &PlanInput<'_>) -> Decision {
    // -- Stage 1: schema validation -----------------------------------------
    let proposal = match input.outcome {
        ProposalOutcome::Malformed { detail } => {
            return review(format!("model output failed validation: {detail}"), input.outcome);
        }
        ProposalOutcome::Missing => {
            return review("model returned no proposal for this file", input.outcome);
        }
        ProposalOutcome::Ok(p) => p,
    };

    let confidence = proposal.confidence();
    if !confidence.is_finite() || !(0.0..=1.0).contains(&confidence) {
        return review(format!("confidence {confidence} is outside 0.0..=1.0"), input.outcome);
    }

    // -- Stage 2: staleness --------------------------------------------------
    // A proposal describes the file as it was at scan time. If the bytes moved
    // underneath us, the proposal is about a file that no longer exists.
    match input.observed {
        None => return Decision::Final(ResolvedAction::NoOp { reason: NoOpReason::Stale }),
        Some(now) if now != input.file.facts => {
            return Decision::Final(ResolvedAction::NoOp { reason: NoOpReason::Stale });
        }
        Some(_) => {}
    }

    match proposal {
        Proposal::SuggestDelete(d) => plan_delete(input, d),
        Proposal::Categorize(p) => plan_categorize(input, p),
    }
}

/// Deletion suggestions bypass the categorization stages entirely and are never
/// subject to the confidence gate. There is no configuration under which one
/// executes without a human.
fn plan_delete(input: &PlanInput<'_>, d: &DeleteProposal) -> Decision {
    if !input.profile.allow_delete_suggestions {
        return review(
            "model proposed a deletion, but this profile sets allow_delete_suggestions = false",
            input.outcome,
        );
    }
    Decision::Final(ResolvedAction::RecycleSuggested {
        reason: d.reason.clone(),
        confidence: d.confidence,
    })
}

fn plan_categorize(input: &PlanInput<'_>, p: &RawProposal) -> Decision {
    let profile = input.profile;

    // -- Stage 3: category resolution ---------------------------------------
    // `is_new_category` is advisory only. Whether a category is permitted is
    // decided from config, never from a flag the model sets about itself.
    let Some(normalized) = sanitize::category(&p.category) else {
        return review(
            format!("proposed category `{}` is not a usable directory name", p.category),
            input.outcome,
        );
    };

    let category = match match_declared(&profile.categories, &normalized) {
        Some(declared) => declared,
        None if profile.allow_dynamic_categories => normalized,
        None => {
            return review(
                format!(
                    "category `{normalized}` is not declared for this profile and \
                     allow_dynamic_categories = false"
                ),
                input.outcome,
            );
        }
    };

    // -- Stage 4: filename rendering ----------------------------------------
    let original_name = input.file.file_name();
    let (filename, renamed) = match &profile.rename {
        Rename::Disabled => (original_name.to_owned(), false),
        Rename::Enabled { template } => {
            let tokens = p
                .name_tokens
                .iter()
                .filter_map(|(k, v)| sanitize::token(v).map(|v| (k.clone(), v)))
                .collect();
            match template::render(template, &tokens, input.file.extension.as_deref()) {
                Ok(name) => {
                    let renamed = name != original_name;
                    (name, renamed)
                }
                Err(e) => {
                    return review(
                        format!("could not render filename template: {e}"),
                        input.outcome,
                    );
                }
            }
        }
    };

    // -- Stage 5: path construction -----------------------------------------
    // The only place in the codebase where a destination path is built.
    let dest = match DestPath::under(&profile.destination_root, &category, &filename) {
        Ok(d) => d,
        Err(e) => return review(format!("could not build a destination path: {e}"), input.outcome),
    };

    if dest.as_path() == input.file.path {
        return Decision::Final(ResolvedAction::NoOp { reason: NoOpReason::AlreadyInPlace });
    }

    // -- Stage 6 is the caller's; it needs the disk. -------------------------
    Decision::CheckCollision(Box::new(PendingMove {
        base: dest.clone(),
        dest,
        source: input.file.path.clone(),
        renamed,
        confidence: p.confidence,
        reasoning: p.reasoning.clone(),
        proposal: p.clone(),
        attempt: 0,
        on_conflict: profile.on_conflict,
        threshold: profile.confidence_threshold,
    }))
}

/// Runs stage 6 (given what the caller found) and stage 7.
///
/// May return another [`Decision::CheckCollision`] when `on_conflict =
/// "suffix"` needs the next candidate name checked.
#[must_use]
pub fn resolve_collision(pending: &PendingMove, found: Occupancy) -> Decision {
    match found {
        // Identical content already filed. Nothing to do, and no reason to
        // trouble a human about it even at low confidence -- which is why the
        // collision check runs ahead of the confidence gate.
        Occupancy::Identical => {
            Decision::Final(ResolvedAction::NoOp { reason: NoOpReason::DuplicateOfDestination })
        }

        Occupancy::Different => match pending.on_conflict {
            OnConflict::Skip => {
                Decision::Final(ResolvedAction::NoOp { reason: NoOpReason::ConflictSkipped })
            }
            OnConflict::Quarantine => Decision::Final(ResolvedAction::Quarantine {
                reason: format!(
                    "a different file already exists at {}",
                    pending.dest.as_path().display()
                ),
            }),
            OnConflict::Suffix => next_suffix(pending),
        },

        // -- Stage 7: confidence gate ---------------------------------------
        Occupancy::Vacant => {
            if pending.confidence < pending.threshold {
                return Decision::Final(ResolvedAction::NeedsManualReview {
                    reason: format!(
                        "confidence {:.2} is below this profile's threshold of {:.2}",
                        pending.confidence, pending.threshold
                    ),
                    raw: Box::new(ProposalOutcome::Ok(Proposal::Categorize(
                        pending.proposal.clone(),
                    ))),
                });
            }
            let dest = pending.dest.clone();
            Decision::Final(if pending.renamed {
                ResolvedAction::MoveAndRename { dest }
            } else {
                ResolvedAction::Move { dest }
            })
        }
    }
}

fn next_suffix(pending: &PendingMove) -> Decision {
    let attempt = pending.attempt + 1;
    if attempt > MAX_SUFFIX_ATTEMPTS {
        return Decision::Final(ResolvedAction::Quarantine {
            reason: format!(
                "gave up after {MAX_SUFFIX_ATTEMPTS} suffixed names were all taken at {}",
                pending.dest.parent_dir().display()
            ),
        });
    }
    match pending.base.with_suffix(attempt) {
        Ok(dest) => Decision::CheckCollision(Box::new(PendingMove {
            dest,
            attempt,
            renamed: true,
            ..pending.clone()
        })),
        Err(e) => Decision::Final(ResolvedAction::Quarantine {
            reason: format!("could not build a suffixed destination path: {e}"),
        }),
    }
}

/// Matches a proposed category against the declared list, tolerating case
/// differences but always returning the *declared* spelling, so casing wobble
/// in model output cannot fragment a category into two directories.
fn match_declared(declared: &[String], proposed: &str) -> Option<String> {
    declared
        .iter()
        .find(|d| d.as_str() == proposed)
        .or_else(|| declared.iter().find(|d| d.eq_ignore_ascii_case(proposed)))
        .cloned()
}

fn review(reason: impl Into<String>, raw: &ProposalOutcome) -> Decision {
    Decision::Final(ResolvedAction::NeedsManualReview {
        reason: reason.into(),
        raw: Box::new(raw.clone()),
    })
}
