#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::case_sensitive_file_extension_comparisons
    )
)]
//! LLM backend adapters.
//!
//! Each API family gets its own adapter: OpenAI-compatible and
//! Anthropic-compatible are not wire-compatible, so one client with a flag
//! would be a client with two code paths and a worse name.
//!
//! The trait itself lives in `bower_core::llm`, so that the core owns the shape
//! of the conversation and a backend remains a detail plugged into it.

pub mod stub;

pub use stub::StubBackend;

use bower_config::{Backend, Provider};
use bower_core::llm::LlmBackend;

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error(
        "the {provider} adapter is not implemented yet; \
         pass --stub-llm to exercise the pipeline without a model"
    )]
    NotImplemented { provider: &'static str },
}

/// Builds the adapter a profile's backend config asks for.
pub fn build(backend: &Backend) -> Result<Box<dyn LlmBackend>, BuildError> {
    match backend.provider {
        Provider::OpenaiCompatible => {
            Err(BuildError::NotImplemented { provider: "OpenAI-compatible" })
        }
        Provider::AnthropicCompatible => {
            Err(BuildError::NotImplemented { provider: "Anthropic-compatible" })
        }
    }
}
