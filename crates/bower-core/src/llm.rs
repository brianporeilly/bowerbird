//! The port the pipeline talks to. Adapters live in `bower-llm`.
//!
//! The trait is defined here rather than alongside the adapters so that the
//! dependency points inward: the core owns the shape of the conversation, and
//! a backend is a detail plugged into it. It also keeps the core testable with
//! a canned backend and no network at all.

use bower_config::Profile;
use std::collections::BTreeMap;

use crate::context::BatchContext;
use crate::model::{FileId, FileRecord, ProposalOutcome};

/// One batch of files, as the core holds them.
///
/// This is the *input to the context builder*, not to a backend: it still
/// carries the profile and the raw records. [`crate::context::build`] reduces
/// it to a [`BatchContext`], which is what a backend actually receives.
#[derive(Debug, Clone, Copy)]
pub struct BatchRequest<'a> {
    /// Supplies the category list, the human description of the directory's
    /// purpose, and whether deletion may be suggested at all.
    pub profile: &'a Profile,
    pub files: &'a [FileRecord],
}

/// One proposal per file, keyed by [`FileId`].
///
/// Validation is per item: a batch where one entry is malformed still yields
/// usable proposals for the rest, and the bad entry becomes
/// [`ProposalOutcome::Malformed`] rather than failing the whole call.
#[derive(Debug, Default, Clone)]
pub struct BatchResponse {
    pub outcomes: BTreeMap<FileId, ProposalOutcome>,
}

impl BatchResponse {
    /// The outcome for `id`, or [`ProposalOutcome::Missing`] when the model
    /// simply did not mention the file.
    #[must_use]
    pub fn outcome_for(&self, id: &FileId) -> ProposalOutcome {
        self.outcomes.get(id).cloned().unwrap_or(ProposalOutcome::Missing)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("backend `{backend}` is unreachable")]
    Unreachable {
        backend: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("backend `{backend}` returned an unusable response: {detail}")]
    BadResponse { backend: String, detail: String },
    #[error("environment variable `{var}` (api_key_env for backend `{backend}`) is not set")]
    MissingApiKey { backend: String, var: String },
}

/// A classifier.
///
/// It receives a [`BatchContext`] -- what the core has already decided the
/// model may see -- not a profile and a list of files. A backend therefore
/// cannot consult a policy setting the context builder chose not to disclose,
/// cannot reach a `FileRecord`'s absolute path, and cannot widen disclosure by
/// reading a field nobody meant to send. Deciding what leaves the machine stays
/// in one place: [`crate::context`].
pub trait LlmBackend: Send + Sync {
    fn name(&self) -> &str;

    /// Classifies one batch. Transport-level retries belong inside the
    /// implementation; a returned error means the batch could not be
    /// classified at all.
    fn classify(&self, ctx: &BatchContext) -> Result<BatchResponse, LlmError>;
}
