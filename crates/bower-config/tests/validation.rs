#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::duration_suboptimal_units
)]
//! Config validation. A file that governs where documents end up should fail
//! loudly and completely, so these lean on two properties: unknown keys are
//! errors, and every problem is reported in one pass.

use bower_config::{Config, ConfigError, OnConflict, Rename, ReviewPlacement, StructuredOutput};

const MINIMAL: &str = r#"
config_version = 1

[general]
quarantine_dir = "/data/_review"

[[llm_backends]]
name = "local"
provider = "openai_compatible"
endpoint = "http://localhost:8080/v1"
model = "llama-3.1-8b-instruct"

[[profiles]]
name = "downloads"
path = "/data/downloads"
llm_backend = "local"
categories = ["Documents"]
"#;

fn parse(text: &str) -> Result<Config, ConfigError> {
    Config::parse(text, "test.toml")
}

fn problems(text: &str) -> Vec<String> {
    match parse(text) {
        Err(ConfigError::Invalid { problems }) => {
            problems.iter().map(ToString::to_string).collect()
        }
        Err(other) => panic!("expected validation problems, got {other}"),
        Ok(_) => panic!("expected validation to fail"),
    }
}

#[test]
fn minimal_config_gets_sensible_defaults() {
    let c = parse(MINIMAL).expect("should be valid");
    let p = c.profile("downloads").unwrap();

    assert!(c.general.dry_run, "dry_run must default to true; shipping unsafe is not a default");
    assert_eq!(c.general.review_placement, ReviewPlacement::InPlace);
    assert_eq!(p.batch_size, 25);
    assert!((p.confidence_threshold - 0.75).abs() < f32::EPSILON);
    assert_eq!(p.on_conflict, OnConflict::Quarantine);
    assert_eq!(p.rename, Rename::Disabled);
    assert!(p.enabled);
    assert!(!p.allow_delete_suggestions, "delete suggestions must be opt-in");
}

#[test]
fn destination_root_defaults_to_the_scanned_directory() {
    let c = parse(MINIMAL).unwrap();
    let p = c.profile("downloads").unwrap();
    assert_eq!(p.destination_root, p.path);
    assert!(p.is_in_place());
}

#[test]
fn an_explicit_destination_root_is_kept() {
    let text = MINIMAL.replace(
        "categories = [\"Documents\"]",
        "categories = [\"Documents\"]\ndestination_root = \"/data/organized\"",
    );
    let c = parse(&text).unwrap();
    let p = c.profile("downloads").unwrap();
    assert_eq!(p.destination_root, std::path::Path::new("/data/organized"));
    assert!(!p.is_in_place());
}

#[test]
fn profile_overrides_beat_general_defaults() {
    let text = MINIMAL.replace(
        "categories = [\"Documents\"]",
        "categories = [\"Documents\"]\nbatch_size = 5\nconfidence_threshold = 0.9",
    );
    let p = parse(&text).unwrap().profile("downloads").unwrap().clone();
    assert_eq!(p.batch_size, 5);
    assert!((p.confidence_threshold - 0.9).abs() < f32::EPSILON);
}

#[test]
fn an_unknown_key_is_an_error_not_a_shrug() {
    let text = MINIMAL.replace("[general]", "[general]\ndry_runn = false");
    let err = parse(&text).unwrap_err();
    assert!(matches!(err, ConfigError::Parse { .. }), "got {err}");
    assert!(err.to_string().contains("parse"));
}

#[test]
fn a_misplaced_key_is_caught_rather_than_silently_ignored() {
    // `content_sniff_bytes` belongs under [profiles.metadata]. Accepting it at
    // profile level would silently disable content sniffing.
    let text = MINIMAL.replace(
        "categories = [\"Documents\"]",
        "categories = [\"Documents\"]\ncontent_sniff_bytes = 4000",
    );
    assert!(matches!(parse(&text), Err(ConfigError::Parse { .. })));
}

#[test]
fn every_problem_is_reported_in_one_pass() {
    let text = r#"
config_version = 1

[[profiles]]
name = "bad name!"
path = "relative/path"
llm_backend = "nonexistent"
categories = ["../escape"]
confidence_threshold = 5.0
"#;
    let found = problems(text);
    let joined = found.join("\n");

    for expected in [
        "name",                 // illegal profile name
        "path",                 // not absolute
        "llm_backend",          // dangling reference
        "categories[0]",        // unsafe category
        "confidence_threshold", // out of range
    ] {
        assert!(joined.contains(expected), "expected a problem about `{expected}` in:\n{joined}");
    }
    assert!(found.len() >= 5, "expected at least 5 problems, got {}:\n{joined}", found.len());
}

#[test]
fn a_dangling_backend_reference_names_the_backend() {
    let text = MINIMAL.replace("llm_backend = \"local\"", "llm_backend = \"typo\"");
    let joined = problems(&text).join("\n");
    assert!(joined.contains("typo"), "{joined}");
}

#[test]
fn duplicate_names_are_rejected() {
    let text = format!(
        "{MINIMAL}\n[[profiles]]\nname = \"downloads\"\npath = \"/data/other\"\n\
         llm_backend = \"local\"\ncategories = [\"Documents\"]\n"
    );
    let joined = problems(&text).join("\n");
    assert!(joined.contains("duplicate"), "{joined}");
}

#[test]
fn duplicate_backend_names_are_rejected() {
    let text = format!(
        "{MINIMAL}\n[[llm_backends]]\nname = \"local\"\n\
         provider = \"anthropic_compatible\"\nendpoint = \"https://api.anthropic.com\"\n\
         model = \"claude-haiku-4-5\"\n"
    );
    let joined = problems(&text).join("\n");
    assert!(joined.contains("duplicate"), "{joined}");
}

#[test]
fn rename_enabled_without_a_template_is_rejected() {
    let text = format!("{MINIMAL}\n[profiles.rename]\nenabled = true\n");
    let joined = problems(&text).join("\n");
    assert!(joined.contains("rename.template"), "{joined}");
}

#[test]
fn rename_enabled_with_a_template_becomes_a_single_state() {
    let text =
        format!("{MINIMAL}\n[profiles.rename]\nenabled = true\ntemplate = \"{{date}}{{ext}}\"\n");
    let p = parse(&text).unwrap().profile("downloads").unwrap().clone();
    assert_eq!(p.rename.template(), Some("{date}{ext}"));
}

#[test]
fn a_fixed_taxonomy_may_not_be_empty() {
    let text = MINIMAL.replace("categories = [\"Documents\"]", "categories = []");
    let joined = problems(&text).join("\n");
    assert!(joined.contains("categories"), "{joined}");
}

#[test]
fn an_empty_taxonomy_is_fine_when_categories_are_dynamic() {
    let text = MINIMAL.replace(
        "categories = [\"Documents\"]",
        "categories = []\nallow_dynamic_categories = true",
    );
    assert!(parse(&text).is_ok());
}

#[test]
fn quarantine_dir_is_required_when_something_would_use_it() {
    let text = MINIMAL.replace("quarantine_dir = \"/data/_review\"", "");
    let joined = problems(&text).join("\n");
    assert!(joined.contains("quarantine_dir"), "{joined}");
}

#[test]
fn recycle_dir_is_required_when_deletion_is_enabled() {
    let text = MINIMAL.replace(
        "categories = [\"Documents\"]",
        "categories = [\"Documents\"]\nallow_delete_suggestions = true",
    );
    let joined = problems(&text).join("\n");
    assert!(joined.contains("recycle_dir"), "{joined}");
}

#[test]
fn api_keys_come_from_the_environment_only() {
    let c = parse(MINIMAL).unwrap();
    let backend = c.backend("local").unwrap();
    // An empty api_key_env means "unauthenticated", not "empty key".
    assert_eq!(backend.api_key_env, None);
}

#[test]
fn structured_output_defaults_to_the_mode_that_works_anywhere() {
    let backend = parse(MINIMAL).unwrap().backend("local").unwrap().clone();
    assert_eq!(
        backend.structured_output,
        StructuredOutput::Prompt,
        "an unknown endpoint must not be sent a response_format it may reject"
    );
    assert!(!backend.structured_output.sends_response_format());
}

#[test]
fn structured_output_can_be_opted_up_per_backend() {
    for (value, expected) in [
        ("json_object", StructuredOutput::JsonObject),
        ("json_schema", StructuredOutput::JsonSchema),
        ("prompt", StructuredOutput::Prompt),
    ] {
        let text = MINIMAL.replace(
            "model = \"llama-3.1-8b-instruct\"",
            &format!("model = \"llama-3.1-8b-instruct\"\nstructured_output = \"{value}\""),
        );
        let backend = parse(&text).unwrap().backend("local").unwrap().clone();
        assert_eq!(backend.structured_output, expected, "for {value}");
    }
}

#[test]
fn an_unknown_structured_output_mode_is_rejected() {
    let text = MINIMAL.replace(
        "model = \"llama-3.1-8b-instruct\"",
        "model = \"llama-3.1-8b-instruct\"\nstructured_output = \"telepathy\"",
    );
    assert!(matches!(parse(&text), Err(ConfigError::Parse { .. })));
}

#[test]
fn an_unsupported_config_version_is_rejected() {
    let text = MINIMAL.replace("config_version = 1", "config_version = 99");
    let joined = problems(&text).join("\n");
    assert!(joined.contains("config_version"), "{joined}");
}

#[test]
fn the_shipped_example_config_is_valid() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../bowerbird.example.toml");
    let text = std::fs::read_to_string(path).expect("example config should exist");
    let config = Config::parse(&text, path).expect("the example config must always be valid");
    assert_eq!(config.profiles.len(), 2);
    assert!(config.general.dry_run, "the example must ship safe");
}
