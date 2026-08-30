//! The port the pipeline talks to. Adapters live in `bower-llm`.
//!
//! The trait is defined here rather than alongside the adapters so that the
//! dependency points inward: the core owns the shape of the conversation, and
//! a backend is a detail plugged into it. It also keeps the core testable with
//! a canned backend and no network at all.

use bower_config::Profile;
use std::collections::BTreeMap;

use crate::model::{FileId, FileRecord, ProposalOutcome};

/// One batch of files to classify.
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

/// A classifier. Implementations must not touch the filesystem; they receive
/// already-gathered [`FileRecord`]s and return proposals.
pub trait LlmBackend: Send + Sync {
    fn name(&self) -> &str;

    /// Classifies one batch. Transport-level retries belong inside the
    /// implementation; a returned error means the batch could not be
    /// classified at all.
    fn classify(&self, request: BatchRequest<'_>) -> Result<BatchResponse, LlmError>;
}
