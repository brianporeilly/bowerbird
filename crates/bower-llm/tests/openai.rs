#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::duration_suboptimal_units
)]
//! The OpenAI-compatible adapter, against a scripted local server.
//!
//! No network, no model, no API key: every exchange here is a loopback socket
//! playing a backend that behaves in a specific way.

mod support;

use bower_config::{Backend, Metadata, OnConflict, Profile, Provider, Rename, StructuredOutput};
use bower_core::llm::{BatchRequest, LlmBackend};
use bower_core::model::{FileFacts, FileId, FileRecord, Proposal, ProposalOutcome};
use bower_llm::OpenAiBackend;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use support::{MockServer, Reply};

const SRC: &str = "/data/downloads";

fn backend(endpoint: String) -> Backend {
    Backend {
        name: "local".to_owned(),
        provider: Provider::OpenaiCompatible,
        endpoint,
        api_key_env: None,
        model: "test-model".to_owned(),
        timeout: Duration::from_secs(5),
        max_retries: 0,
        structured_output: StructuredOutput::Prompt,
    }
}

fn profile() -> Profile {
    Profile {
        name: "downloads".to_owned(),
        path: PathBuf::from(SRC),
        description: "General downloads folder.".to_owned(),
        enabled: true,
        llm_backend: "local".to_owned(),
        destination_root: PathBuf::from("/data/organized"),
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

fn record(name: &str) -> FileRecord {
    let path = Path::new(SRC).join(name);
    FileRecord {
        id: FileId::for_path(&path),
        relative: PathBuf::from(name),
        facts: FileFacts { size: 10, mtime: SystemTime::UNIX_EPOCH },
        extension: Some("pdf".to_owned()),
        mime: Some("application/pdf".to_owned()),
        content_snippet: None,
        path,
    }
}

/// A well-formed proposal entry for `file`.
fn good(file: &FileRecord, category: &str) -> String {
    format!(
        r#"{{"file_id":"{}","action":"categorize","category":"{category}",
            "is_new_category":false,"name_tokens":{{}},"confidence":0.9,"reasoning":"r"}}"#,
        file.id
    )
}

fn payload(entries: &[String]) -> String {
    format!(r#"{{"proposals":[{}]}}"#, entries.join(","))
}

fn adapter(server: &MockServer) -> OpenAiBackend {
    OpenAiBackend::new(&backend(server.endpoint())).with_backoff(Duration::ZERO)
}

fn classify<'a>(
    adapter: &OpenAiBackend,
    profile: &'a Profile,
    files: &'a [FileRecord],
) -> Result<bower_core::llm::BatchResponse, bower_core::llm::LlmError> {
    adapter.classify(&bower_core::context::build(BatchRequest { profile, files }))
}

fn is_ok(outcome: &ProposalOutcome) -> bool {
    matches!(outcome, ProposalOutcome::Ok(_))
}

// --- the happy path ---------------------------------------------------------

#[test]
fn a_well_formed_reply_classifies_every_file_in_one_request() {
    let files = [record("a.pdf"), record("b.pdf")];
    let server = MockServer::new(vec![Reply::assistant(&payload(&[
        good(&files[0], "Documents"),
        good(&files[1], "Images"),
    ]))]);

    let response = classify(&adapter(&server), &profile(), &files).unwrap();

    assert!(is_ok(&response.outcome_for(&files[0].id)));
    assert!(is_ok(&response.outcome_for(&files[1].id)));
    assert_eq!(server.request_count(), 1, "a good reply needs no reformat");
}

#[test]
fn an_empty_batch_never_reaches_the_network() {
    let server = MockServer::new(vec![Reply::assistant("{}")]);
    let response = classify(&adapter(&server), &profile(), &[]).unwrap();

    assert!(response.outcomes.is_empty());
    assert_eq!(server.request_count(), 0, "there is nothing to ask about");
}

// --- per-item validation ----------------------------------------------------

#[test]
fn one_bad_entry_costs_one_reformat_and_its_neighbours_survive() {
    let files = [record("a.pdf"), record("b.pdf")];
    let broken = format!(r#"{{"file_id":"{}","action":"categorize"}}"#, files[1].id);

    let server = MockServer::new(vec![
        Reply::assistant(&payload(&[good(&files[0], "Documents"), broken])),
        Reply::assistant(&payload(&[good(&files[0], "Documents"), good(&files[1], "Images")])),
    ]);

    let response = classify(&adapter(&server), &profile(), &files).unwrap();

    assert!(is_ok(&response.outcome_for(&files[0].id)));
    assert!(is_ok(&response.outcome_for(&files[1].id)), "the reformat recovered it");
    assert_eq!(server.request_count(), 2);
}

#[test]
fn there_is_never_more_than_one_reformat() {
    let files = [record("a.pdf")];
    let broken = payload(&[format!(r#"{{"file_id":"{}","action":"categorize"}}"#, files[0].id)]);

    // The server never improves. The adapter must give up, not loop.
    let server = MockServer::new(vec![Reply::assistant(&broken)]);
    let response = classify(&adapter(&server), &profile(), &files).unwrap();

    assert!(matches!(response.outcome_for(&files[0].id), ProposalOutcome::Malformed { .. }));
    assert_eq!(server.request_count(), 2, "one attempt plus exactly one reformat");
}

#[test]
fn a_reformat_cannot_downgrade_a_file_that_was_already_answered() {
    let files = [record("a.pdf"), record("b.pdf")];
    let broken = format!(r#"{{"file_id":"{}","action":"categorize"}}"#, files[1].id);

    let server = MockServer::new(vec![
        Reply::assistant(&payload(&[good(&files[0], "Documents"), broken.clone()])),
        // The retry answers neither file usefully.
        Reply::assistant(&payload(&[broken])),
    ]);

    let response = classify(&adapter(&server), &profile(), &files).unwrap();
    assert!(
        is_ok(&response.outcome_for(&files[0].id)),
        "trust only moves downward; a good first answer stands"
    );
}

#[test]
fn a_reply_naming_a_file_we_never_sent_is_discarded() {
    let files = [record("a.pdf")];
    let stranger = record("someone-elses.pdf");

    let server = MockServer::new(vec![Reply::assistant(&payload(&[
        good(&files[0], "Documents"),
        good(&stranger, "Documents"),
    ]))]);

    let response = classify(&adapter(&server), &profile(), &files).unwrap();
    assert_eq!(response.outcomes.len(), 1);
    assert!(!response.outcomes.contains_key(&stranger.id));
}

#[test]
fn prose_and_code_fences_around_the_json_are_tolerated() {
    let files = [record("a.pdf")];
    let fenced =
        format!("Certainly!\n```json\n{}\n```\n", payload(&[good(&files[0], "Documents")]));

    let server = MockServer::new(vec![Reply::assistant(&fenced)]);
    let response = classify(&adapter(&server), &profile(), &files).unwrap();

    assert!(is_ok(&response.outcome_for(&files[0].id)));
    assert_eq!(server.request_count(), 1, "a recoverable reply needs no reformat");
}

// --- transport retries, kept separate ---------------------------------------

#[test]
fn transport_retries_do_not_consume_the_reformat() {
    let files = [record("a.pdf")];
    let mut config = backend(String::new());
    config.max_retries = 2;

    let server = MockServer::new(vec![
        Reply::status(500, "boom"),
        Reply::status(500, "boom"),
        Reply::assistant(&payload(&[good(&files[0], "Documents")])),
    ]);
    config.endpoint = server.endpoint();

    let adapter = OpenAiBackend::new(&config).with_backoff(Duration::ZERO);
    let response = adapter
        .classify(&bower_core::context::build(BatchRequest { profile: &profile(), files: &files }))
        .unwrap();

    assert!(is_ok(&response.outcome_for(&files[0].id)));
    assert_eq!(server.request_count(), 3, "two transport retries, zero reformats");
}

#[test]
fn a_429_is_retried_but_a_400_is_not() {
    let files = [record("a.pdf")];

    let mut config = backend(String::new());
    config.max_retries = 1;
    let server = MockServer::new(vec![
        Reply::status(429, "slow down"),
        Reply::assistant(&payload(&[good(&files[0], "Documents")])),
    ]);
    config.endpoint = server.endpoint();
    let adapter = OpenAiBackend::new(&config).with_backoff(Duration::ZERO);
    assert!(
        adapter
            .classify(&bower_core::context::build(BatchRequest {
                profile: &profile(),
                files: &files
            }))
            .is_ok()
    );
    assert_eq!(server.request_count(), 2, "429 means come back");

    let mut config = backend(String::new());
    config.max_retries = 3;
    let server = MockServer::new(vec![Reply::status(400, r#"{"error":"bad model"}"#)]);
    config.endpoint = server.endpoint();
    let adapter = OpenAiBackend::new(&config).with_backoff(Duration::ZERO);
    let err = adapter
        .classify(&bower_core::context::build(BatchRequest { profile: &profile(), files: &files }))
        .expect_err("a 400 is fatal");

    assert_eq!(server.request_count(), 1, "repeating a rejected request will not help");
    assert!(err.to_string().contains("local"));
}

// --- secrets ----------------------------------------------------------------

#[test]
fn a_missing_api_key_fails_before_anything_is_sent() {
    let files = [record("a.pdf")];
    let server = MockServer::new(vec![Reply::assistant("{}")]);

    let mut config = backend(server.endpoint());
    config.api_key_env = Some("BOWER_TEST_KEY_DEFINITELY_UNSET".to_owned());
    let adapter = OpenAiBackend::new(&config);

    let err = adapter
        .classify(&bower_core::context::build(BatchRequest { profile: &profile(), files: &files }))
        .expect_err("no key, no request");

    assert!(err.to_string().contains("BOWER_TEST_KEY_DEFINITELY_UNSET"));
    assert_eq!(server.request_count(), 0, "the batch must not leave without credentials");
}

#[test]
fn the_key_is_sent_as_a_bearer_token_and_never_appears_in_an_error() {
    const SECRET: &str = "sk-do-not-leak-me-0123456789";
    let files = [record("a.pdf")];

    // A server that echoes the request back in its error body, which is the
    // realistic way a credential ends up somewhere it should not be.
    let server = MockServer::new(vec![Reply::status(
        401,
        &format!(r#"{{"error":"invalid key: Bearer {SECRET}"}}"#),
    )]);

    let mut config = backend(server.endpoint());
    config.api_key_env = Some("BOWER_TEST_KEY".to_owned());

    let adapter = OpenAiBackend::new(&config)
        .with_backoff(Duration::ZERO)
        .with_key_source(|_| Some(SECRET.to_owned()));

    let err = adapter
        .classify(&bower_core::context::build(BatchRequest { profile: &profile(), files: &files }))
        .expect_err("401 is fatal");
    let rendered = format!("{err} / {err:?}");

    assert_eq!(
        server.requests()[0].header("authorization"),
        Some(format!("Bearer {SECRET}").as_str()),
        "the key must actually be sent"
    );
    assert!(!rendered.contains(SECRET), "the key leaked into an error: {rendered}");
    assert!(rendered.contains("[redacted]"), "{rendered}");
}

#[test]
fn a_blank_credential_is_treated_as_missing() {
    let files = [record("a.pdf")];
    let server = MockServer::new(vec![Reply::assistant("{}")]);

    let mut config = backend(server.endpoint());
    config.api_key_env = Some("BOWER_TEST_KEY".to_owned());

    let adapter = OpenAiBackend::new(&config).with_key_source(|_| Some("   ".to_owned()));
    assert!(
        adapter
            .classify(&bower_core::context::build(BatchRequest {
                profile: &profile(),
                files: &files
            }))
            .is_err()
    );
    assert_eq!(server.request_count(), 0);
}

// --- the capability flag ----------------------------------------------------

#[test]
fn structured_output_decides_whether_response_format_is_sent() {
    let files = [record("a.pdf")];
    let reply = payload(&[good(&files[0], "Documents")]);

    for (mode, expected) in [
        (StructuredOutput::Prompt, None),
        (StructuredOutput::JsonObject, Some("json_object")),
        (StructuredOutput::JsonSchema, Some("json_schema")),
    ] {
        let server = MockServer::new(vec![Reply::assistant(&reply)]);
        let mut config = backend(server.endpoint());
        config.structured_output = mode;

        let adapter = OpenAiBackend::new(&config).with_backoff(Duration::ZERO);
        adapter
            .classify(&bower_core::context::build(BatchRequest {
                profile: &profile(),
                files: &files,
            }))
            .unwrap();

        let sent = server.requests()[0].json();
        match expected {
            None => assert!(
                sent.get("response_format").is_none(),
                "an unknown endpoint must not be sent a field it may reject: {sent}"
            ),
            Some(kind) => assert_eq!(sent["response_format"]["type"], kind, "for {mode:?}"),
        }
    }
}

#[test]
fn the_json_schema_mode_sends_a_strict_schema() {
    let files = [record("a.pdf")];
    let server = MockServer::new(vec![Reply::assistant(&payload(&[good(&files[0], "Documents")]))]);

    let mut config = backend(server.endpoint());
    config.structured_output = StructuredOutput::JsonSchema;
    OpenAiBackend::new(&config)
        .classify(&bower_core::context::build(BatchRequest { profile: &profile(), files: &files }))
        .unwrap();

    let sent = server.requests()[0].json();
    assert_eq!(sent["response_format"]["json_schema"]["strict"], serde_json::json!(true));
    assert_eq!(
        sent["response_format"]["json_schema"]["schema"]["additionalProperties"],
        serde_json::json!(false)
    );
}

// --- what goes on the wire --------------------------------------------------

#[test]
fn no_absolute_path_is_ever_sent_to_the_model() {
    let files = [record("a.pdf"), record("nested/b.pdf")];
    let server = MockServer::new(vec![Reply::assistant(&payload(&[
        good(&files[0], "Documents"),
        good(&files[1], "Documents"),
    ]))]);

    let mut p = profile();
    p.metadata.content_sniff_bytes = 1000;
    classify(&adapter(&server), &p, &files).unwrap();

    let sent = server.requests()[0].body.clone();
    assert!(!sent.contains(SRC), "the scan root reached the model: {sent}");
    assert!(!sent.contains("/data/organized"), "the destination root reached the model");
}

#[test]
fn the_taxonomy_and_the_files_reach_the_model() {
    let files = [record("a.pdf")];
    let server = MockServer::new(vec![Reply::assistant(&payload(&[good(&files[0], "Documents")]))]);

    classify(&adapter(&server), &profile(), &files).unwrap();

    let text = server.requests()[0].prompt_text();
    assert!(text.contains("Documents") && text.contains("Images"), "taxonomy missing");
    assert!(text.contains(files[0].id.as_str()), "the file id must be sent verbatim");
    assert!(text.contains("General downloads folder."), "the profile description is context");
}

#[test]
fn a_reply_that_is_not_json_at_all_costs_one_reformat_then_goes_to_review() {
    let files = [record("a.pdf")];
    let server = MockServer::new(vec![Reply::assistant("I'm afraid I can't do that.")]);

    let response = classify(&adapter(&server), &profile(), &files).unwrap();

    assert!(matches!(response.outcome_for(&files[0].id), ProposalOutcome::Malformed { .. }));
    assert_eq!(server.request_count(), 2);
}

#[test]
fn a_deletion_suggestion_survives_the_wire_intact() {
    let files = [record("a.pdf")];
    let mut p = profile();
    p.allow_delete_suggestions = true;

    let reply = payload(&[format!(
        r#"{{"file_id":"{}","action":"suggest_delete","reason":"empty file","confidence":0.95}}"#,
        files[0].id
    )]);
    let server = MockServer::new(vec![Reply::assistant(&reply)]);

    let response = classify(&adapter(&server), &p, &files).unwrap();
    match response.outcome_for(&files[0].id) {
        ProposalOutcome::Ok(Proposal::SuggestDelete(d)) => assert_eq!(d.reason, "empty file"),
        other => panic!("expected a deletion suggestion, got {other:?}"),
    }
}

/// A server that is up and merely slow must not be reported as unreachable.
/// The obvious reading of "unreachable" is that the endpoint is wrong or the
/// process is down; for a local model the real cause is usually a batch too
/// large for the timeout, and the error should say so.
#[test]
fn a_slow_backend_reports_a_timeout_with_the_remedy_not_unreachable() {
    let server = MockServer::new(vec![
        Reply::assistant(r#"{"proposals":[]}"#).after(Duration::from_millis(600)),
    ]);
    let mut cfg = backend(server.endpoint());
    cfg.timeout = Duration::from_millis(100);
    cfg.max_retries = 0;
    let adapter = OpenAiBackend::new(&cfg);

    let files = vec![record("a.pdf")];
    let err = adapter
        .classify(&bower_core::context::build(BatchRequest { profile: &profile(), files: &files }))
        .unwrap_err();

    assert!(
        matches!(err, bower_core::llm::LlmError::Timeout { .. }),
        "expected a timeout, got {err:?}"
    );

    let text = err.to_string();
    assert!(text.contains("did not answer"), "{text}");
    assert!(text.contains("batch_size"), "the error should name the first thing to change: {text}");
}
