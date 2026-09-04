//! The OpenAI-compatible adapter.
//!
//! ADR-0001 §9 makes this the first real backend, aimed at the common
//! self-hosted surface: llama.cpp's server, Ollama, vLLM, and anything else
//! speaking `/chat/completions`.
//!
//! # Two retry mechanisms, deliberately separate
//!
//! * **Transport retries** (`max_retries`) cover a connection that failed, a
//!   timeout, a 429, or a 5xx — the request never got a real answer. Same
//!   request, sent again.
//! * **The reformat retry** covers a request that was answered with something
//!   unusable. There is exactly one, per ADR-0001 §4 stage 1, and it is a
//!   *different* request: the bad reply and a description of what was wrong
//!   are appended to the conversation.
//!
//! Conflating them would let a model that reliably emits bad JSON burn the
//! whole transport budget on it, so the code keeps them apart: each HTTP call
//! gets its own transport budget, and there is never more than one reformat.

pub mod parse;
pub mod prompt;

use bower_config::{Backend, StructuredOutput};
use bower_core::context::BatchContext;
use bower_core::llm::{BatchResponse, LlmBackend, LlmError};
use bower_core::model::{FileId, ProposalOutcome};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::time::Duration;

use parse::{ParsedBatch, parse_reply};

/// Cap on a response body. A backend that streams gigabytes at us is broken or
/// hostile, and either way we should not try to hold it in memory.
const MAX_RESPONSE_BYTES: u64 = 8 * 1024 * 1024;

/// Base delay between transport retries; doubled each attempt.
const DEFAULT_BACKOFF: Duration = Duration::from_millis(250);

pub struct OpenAiBackend {
    name: String,
    url: String,
    model: String,
    api_key_env: Option<String>,
    max_retries: u32,
    structured_output: StructuredOutput,
    backoff: Duration,
    key_source: KeySource,
    agent: ureq::Agent,
    /// Kept alongside the agent that enforces it, so a timeout can say how long
    /// it waited. `ureq` does not report that back in the error.
    timeout: Duration,
}

/// Where a named credential is looked up. The environment today; a keyring or
/// a secrets agent could slot in here without touching the transport.
pub type KeySource = fn(&str) -> Option<String>;

fn key_from_env(var: &str) -> Option<String> {
    std::env::var(var).ok()
}

impl std::fmt::Debug for OpenAiBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Hand-written so no future derive can start printing an agent that
        // may hold credentials.
        f.debug_struct("OpenAiBackend")
            .field("name", &self.name)
            .field("url", &self.url)
            .field("model", &self.model)
            .field("api_key_env", &self.api_key_env)
            .field("structured_output", &self.structured_output)
            .finish_non_exhaustive()
    }
}

impl OpenAiBackend {
    #[must_use]
    pub fn new(backend: &Backend) -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(backend.timeout))
            // We want the status and body of a 4xx to report on, not an opaque
            // error, so failures are handled here rather than by ureq.
            .http_status_as_error(false)
            .build()
            .new_agent();

        Self {
            name: backend.name.clone(),
            url: chat_completions_url(&backend.endpoint),
            model: backend.model.clone(),
            api_key_env: backend.api_key_env.clone(),
            max_retries: backend.max_retries,
            structured_output: backend.structured_output,
            backoff: DEFAULT_BACKOFF,
            key_source: key_from_env,
            agent,
            timeout: backend.timeout,
        }
    }

    /// Shortens the transport backoff. For tests, which should not sleep.
    #[must_use]
    pub fn with_backoff(mut self, backoff: Duration) -> Self {
        self.backoff = backoff;
        self
    }

    /// Replaces the credential lookup. The default reads the environment.
    #[must_use]
    pub fn with_key_source(mut self, key_source: KeySource) -> Self {
        self.key_source = key_source;
        self
    }

    /// Reads the API key from the environment, never from config.
    fn api_key(&self) -> Result<Option<String>, LlmError> {
        let Some(var) = &self.api_key_env else { return Ok(None) };
        match (self.key_source)(var) {
            Some(v) if !v.trim().is_empty() => Ok(Some(v)),
            _ => Err(LlmError::MissingApiKey { backend: self.name.clone(), var: var.clone() }),
        }
    }

    fn body(&self, messages: &[Value]) -> Value {
        let response_format = match self.structured_output {
            StructuredOutput::Prompt => None,
            StructuredOutput::JsonObject => Some(json!({ "type": "json_object" })),
            StructuredOutput::JsonSchema => Some(json!({
                "type": "json_schema",
                "json_schema": {
                    "name": "bowerbird_proposals",
                    "strict": true,
                    "schema": prompt::response_schema(),
                }
            })),
        };

        let mut body = json!({
            "model": self.model,
            "messages": messages,
            // Classification wants the same answer for the same file, not
            // variety. It also makes a dry run reproducible.
            "temperature": 0,
        });
        if let (Some(format), Some(object)) = (response_format, body.as_object_mut()) {
            object.insert("response_format".to_owned(), format);
        }
        body
    }

    /// One HTTP exchange, with transport-level retries.
    fn send(&self, messages: &[Value], key: Option<&str>) -> Result<String, LlmError> {
        let payload = self.body(messages).to_string();
        let attempts = self.max_retries.saturating_add(1);
        let mut last: Option<LlmError> = None;

        for attempt in 0..attempts {
            if attempt > 0 {
                std::thread::sleep(self.backoff * 2u32.saturating_pow(attempt - 1));
            }
            match self.attempt(&payload, key) {
                Ok(content) => return Ok(content),
                Err(Attempt::Fatal(e)) => return Err(e),
                Err(Attempt::Retryable(e)) => {
                    tracing::debug!(backend = %self.name, attempt, "retrying: {e}");
                    last = Some(e);
                }
            }
        }
        Err(last.unwrap_or_else(|| LlmError::BadResponse {
            backend: self.name.clone(),
            detail: "no attempt was made".to_owned(),
        }))
    }

    fn attempt(&self, payload: &str, key: Option<&str>) -> Result<String, Attempt> {
        let mut request = self.agent.post(&self.url).header("content-type", "application/json");
        if let Some(key) = key {
            request = request.header("authorization", &format!("Bearer {key}"));
        }

        let mut response = request.send(payload).map_err(|e| {
            // Connection-level failures are worth another go; a malformed URL
            // or a TLS refusal is not.
            let err = if matches!(e, ureq::Error::Timeout(_)) {
                LlmError::Timeout {
                    backend: self.name.clone(),
                    timeout_secs: self.timeout.as_secs(),
                }
            } else {
                LlmError::Unreachable { backend: self.name.clone(), source: Box::new(e) }
            };
            if matches!(&err, LlmError::Timeout { .. })
                || matches!(
                    &err,
                    LlmError::Unreachable { source, .. }
                        if is_transient(source.as_ref())
                )
            {
                Attempt::Retryable(err)
            } else {
                Attempt::Fatal(err)
            }
        })?;

        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .with_config()
            .limit(MAX_RESPONSE_BYTES)
            .read_to_string()
            .unwrap_or_default();

        if status != 200 {
            let detail = format!("HTTP {status}: {}", redact(&truncate(&body, 400), key));
            let err = LlmError::BadResponse { backend: self.name.clone(), detail };
            // 429 and 5xx are the server asking us to come back; 4xx is the
            // server telling us the request is wrong, and repeating it will
            // not improve matters.
            return Err(if status == 429 || status >= 500 {
                Attempt::Retryable(err)
            } else {
                Attempt::Fatal(err)
            });
        }

        content_of(&body).ok_or_else(|| {
            Attempt::Fatal(LlmError::BadResponse {
                backend: self.name.clone(),
                detail: format!(
                    "no choices[0].message.content in the reply: {}",
                    redact(&truncate(&body, 400), key)
                ),
            })
        })
    }
}

enum Attempt {
    Retryable(LlmError),
    Fatal(LlmError),
}

impl LlmBackend for OpenAiBackend {
    fn name(&self) -> &str {
        &self.name
    }

    fn classify(&self, ctx: &BatchContext) -> Result<BatchResponse, LlmError> {
        if ctx.files.is_empty() {
            return Ok(BatchResponse::default());
        }

        let key = self.api_key()?;
        let expected: BTreeSet<FileId> = ctx.files.iter().map(|f| f.file_id.clone()).collect();

        let mut messages = vec![
            json!({ "role": "system", "content": prompt::system_prompt(ctx) }),
            json!({ "role": "user", "content": prompt::user_payload(ctx) }),
        ];

        tracing::debug!(
            backend = %self.name,
            model = %self.model,
            files = ctx.files.len(),
            "classifying batch"
        );

        let first_reply = self.send(&messages, key.as_deref())?;
        let parsed = parse_reply(&first_reply, &expected);

        // A whole-reply failure makes every file a problem; a per-item failure
        // makes only some.
        let problems: Vec<_> = match &parsed {
            Ok(batch) if !batch.needs_reformat(&expected) => Vec::new(),
            Ok(batch) => batch.unresolved(&expected),
            Err(e) => expected.iter().map(|id| (id.clone(), e.to_string())).collect(),
        };
        // Kept so a reply that was unusable *as a whole* still reaches review
        // with its diagnosis, instead of looking like a file the model simply
        // never mentioned.
        let mut unusable_reply = parsed.as_ref().err().map(ToString::to_string);
        let first = parsed.ok();

        if problems.is_empty() {
            return Ok(finish(first.unwrap_or_default(), &expected, None));
        }

        // Exactly one reformat retry. Its HTTP call gets its own transport
        // budget; what it never gets is a second reformat.
        tracing::debug!(backend = %self.name, unusable = problems.len(), "asking for a reformat");
        messages.push(json!({ "role": "assistant", "content": first_reply }));
        messages.push(json!({ "role": "user", "content": prompt::reformat_complaint(&problems) }));

        let second = match self.send(&messages, key.as_deref()) {
            Ok(reply) => match parse_reply(&reply, &expected) {
                Ok(batch) => {
                    unusable_reply = None;
                    Some(batch)
                }
                Err(e) => {
                    unusable_reply = Some(e.to_string());
                    None
                }
            },
            // The reformat failing outright is not fatal: whatever the first
            // attempt did produce still stands, and the rest goes to review.
            Err(e) => {
                tracing::debug!(backend = %self.name, "reformat attempt failed: {e}");
                None
            }
        };

        Ok(finish(merge(first, second, &expected), &expected, unusable_reply.as_deref()))
    }
}

/// Prefers a usable answer from the retry, keeps the first attempt's otherwise.
///
/// Trust only moves downward: a retry cannot turn a file that was answered
/// properly into a malformed one.
fn merge(
    first: Option<ParsedBatch>,
    second: Option<ParsedBatch>,
    expected: &BTreeSet<FileId>,
) -> ParsedBatch {
    let mut out = ParsedBatch::default();
    for id in expected {
        let from_first = first.as_ref().and_then(|b| b.outcomes.get(id));
        let from_second = second.as_ref().and_then(|b| b.outcomes.get(id));

        // Preference order: a usable answer from either attempt (the retry
        // first), then the most informative failure (the retry's, again first).
        let candidates = [from_second, from_first];
        let chosen = candidates
            .into_iter()
            .flatten()
            .find(|o| matches!(o, ProposalOutcome::Ok(_)))
            .or_else(|| candidates.into_iter().flatten().next());

        if let Some(outcome) = chosen {
            out.outcomes.insert(id.clone(), outcome.clone());
        }
    }
    out.unattributable =
        first.map_or(0, |b| b.unattributable) + second.map_or(0, |b| b.unattributable);
    out
}

/// Turns a parsed batch into the port's response type.
///
/// A file the model simply never mentioned is left absent, which
/// `BatchResponse` reports as `Missing`. A file left unanswered because the
/// *whole* reply was unusable gets that reason attached instead, so the review
/// queue can say what went wrong rather than just that nothing came back.
fn finish(
    batch: ParsedBatch,
    expected: &BTreeSet<FileId>,
    unusable_reply: Option<&str>,
) -> BatchResponse {
    if batch.unattributable > 0 {
        tracing::warn!(
            count = batch.unattributable,
            "discarded proposals naming file ids that were not in the request"
        );
    }

    let mut outcomes = batch.outcomes;
    outcomes.retain(|id, _| expected.contains(id));

    if let Some(detail) = unusable_reply {
        for id in expected {
            outcomes
                .entry(id.clone())
                .or_insert_with(|| ProposalOutcome::Malformed { detail: detail.to_owned() });
        }
    }
    BatchResponse { outcomes }
}

/// `{endpoint}/chat/completions`, tolerating a trailing slash.
fn chat_completions_url(endpoint: &str) -> String {
    format!("{}/chat/completions", endpoint.trim_end_matches('/'))
}

/// Digs `choices[0].message.content` out of a chat-completions reply.
fn content_of(body: &str) -> Option<String> {
    let value: Value = serde_json::from_str(body).ok()?;
    value
        .get("choices")?
        .as_array()?
        .first()?
        .get("message")?
        .get("content")?
        .as_str()
        .map(str::to_owned)
}

/// Whether a transport failure is worth repeating.
fn is_transient(source: &(dyn std::error::Error + 'static)) -> bool {
    // Not a ureq error at all means something unrecognised; one more try beats
    // failing a whole profile on it.
    let Some(err) = source.downcast_ref::<ureq::Error>() else { return true };
    matches!(err, ureq::Error::Io(_) | ureq::Error::Timeout(_) | ureq::Error::ConnectionFailed)
}

/// Removes the API key from text that is about to become an error message.
///
/// Nothing should put it there in the first place; this is the backstop for a
/// proxy or a chatty server that echoes request headers back to us.
fn redact(text: &str, key: Option<&str>) -> String {
    match key {
        Some(k) if !k.is_empty() && text.contains(k) => text.replace(k, "[redacted]"),
        _ => text.to_owned(),
    }
}

fn truncate(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", s.get(..i).unwrap_or(s)),
        None => s.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_endpoint_gains_the_chat_completions_path_once() {
        assert_eq!(
            chat_completions_url("http://localhost:8080/v1"),
            "http://localhost:8080/v1/chat/completions"
        );
        assert_eq!(
            chat_completions_url("http://localhost:8080/v1/"),
            "http://localhost:8080/v1/chat/completions"
        );
    }

    #[test]
    fn content_is_dug_out_of_a_chat_completions_reply() {
        let body = r#"{"choices":[{"message":{"role":"assistant","content":"hello"}}]}"#;
        assert_eq!(content_of(body).as_deref(), Some("hello"));
        assert_eq!(content_of(r#"{"choices":[]}"#), None);
        assert_eq!(content_of("not json"), None);
    }

    #[test]
    fn an_api_key_is_scrubbed_from_anything_headed_for_an_error() {
        let out = redact("upstream said Bearer sk-secret-123 is invalid", Some("sk-secret-123"));
        assert!(!out.contains("sk-secret-123"), "{out}");
        assert!(out.contains("[redacted]"), "{out}");
    }

    #[test]
    fn truncation_keeps_error_messages_bounded() {
        let out = truncate(&"x".repeat(10_000), 40);
        assert!(out.chars().count() <= 41, "{}", out.len());
    }
}
