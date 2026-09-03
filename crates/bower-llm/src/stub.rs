//! A deterministic, offline backend.
//!
//! Not a mock in the test-double sense -- it ships in the binary behind
//! `--stub-llm` so the whole pipeline can be exercised end to end without a
//! model, an API key, or a network. Its proposals are derived from extension
//! and filename with no cleverness whatsoever; its value is that it is
//! *reproducible*, so a dry run over the same directory prints the same plan
//! every time and the interesting behaviour under test is the policy engine's,
//! not a model's.
//!
//! Confidence is spread deterministically across a band that straddles typical
//! thresholds, so a realistic run exercises the manual-review path as well as
//! the happy one.

use bower_core::context::{BatchContext, FileContext};
use bower_core::llm::{BatchResponse, LlmBackend, LlmError};
use bower_core::model::{Proposal, ProposalOutcome, RawProposal};
use std::collections::BTreeMap;

/// Extension groups, mapped to the category names the ADR's example config
/// uses. Matched case-insensitively against the profile's declared categories.
const GUESSES: &[(&str, &[&str])] = &[
    ("Documents", &["pdf", "doc", "docx", "odt", "rtf", "txt", "md", "epub"]),
    ("Images", &["jpg", "jpeg", "png", "gif", "webp", "heic", "tiff", "svg"]),
    ("Installers", &["exe", "msi", "dmg", "pkg", "deb", "rpm", "appimage"]),
    ("Archives", &["zip", "tar", "gz", "bz2", "xz", "7z", "rar"]),
    ("Media", &["mp3", "flac", "wav", "mp4", "mkv", "mov", "avi", "webm"]),
    ("Spreadsheets", &["csv", "xls", "xlsx", "ods"]),
];

const FALLBACK: &str = "Documents";

#[derive(Debug, Default, Clone, Copy)]
pub struct StubBackend;

impl StubBackend {
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl LlmBackend for StubBackend {
    // The trait ties the lifetime to `&self` so backends can name themselves
    // from config; this one happens to be a literal.
    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "stub"
    }

    fn classify(&self, ctx: &BatchContext) -> Result<BatchResponse, LlmError> {
        let mut outcomes = BTreeMap::new();
        for file in &ctx.files {
            let guess = guess_category(file);
            let category = resolve_against(ctx, &guess);
            let is_new_category = !ctx.categories.iter().any(|c| c.eq_ignore_ascii_case(&category));

            let proposal = RawProposal {
                file_id: file.file_id.clone(),
                category,
                is_new_category,
                name_tokens: tokens(file),
                confidence: confidence(file),
                reasoning: format!(
                    "stub backend: extension `{}` and MIME `{}`",
                    file.extension.as_deref().unwrap_or("none"),
                    file.mime.as_deref().unwrap_or("unknown"),
                ),
            };
            outcomes
                .insert(file.file_id.clone(), ProposalOutcome::Ok(Proposal::Categorize(proposal)));
        }
        Ok(BatchResponse { outcomes })
    }
}

fn guess_category(file: &FileContext) -> String {
    let ext = file.extension.as_deref().unwrap_or_default();
    GUESSES
        .iter()
        .find(|(_, exts)| exts.contains(&ext))
        .map_or(FALLBACK, |(name, _)| *name)
        .to_owned()
}

/// Prefers a category the context actually declares, so the stub does not
/// invent categories on profiles that forbid them.
fn resolve_against(ctx: &BatchContext, guess: &str) -> String {
    if let Some(declared) = ctx.categories.iter().find(|c| c.eq_ignore_ascii_case(guess)) {
        return declared.clone();
    }
    if ctx.allow_new_categories {
        return guess.to_owned();
    }
    ctx.categories.first().cloned().unwrap_or_else(|| guess.to_owned())
}

/// The stub's whole token vocabulary: `name`, `vendor`, `doc_type`.
///
/// Deliberately fixed rather than derived from `ctx.filename_tokens`. A
/// template asking for something else gets a `MissingToken` and the file goes
/// to manual review -- which is exactly what a real model declining to supply a
/// token does, and worth exercising. Inventing a value for every name a
/// template happens to mention would hide that path.
///
/// `date` is absent because the engine fills `{date}` from the file's mtime; a
/// value proposed here would be ignored.
fn tokens(file: &FileContext) -> BTreeMap<String, String> {
    let mut tokens = BTreeMap::new();
    let (stem, _) = bower_core::model::split_extension(&file.file_name);
    tokens.insert("name".to_owned(), stem.to_owned());
    tokens.insert("vendor".to_owned(), first_word(stem).to_owned());
    tokens.insert("doc_type".to_owned(), guess_category(file).to_lowercase());
    tokens
}

fn first_word(stem: &str) -> &str {
    stem.split(['-', '_', ' ', '.']).find(|s| !s.is_empty()).unwrap_or(stem)
}

/// Deterministic pseudo-confidence in `0.60..=0.99`, derived from the file id so
/// it is stable across runs but varied across files.
///
/// The band deliberately straddles a typical threshold: a demo run should show
/// files being filed *and* files being held back, since exercising the
/// manual-review path is most of the point of having a stub at all.
fn confidence(file: &FileContext) -> f32 {
    let sum: u32 = file.file_id.as_str().bytes().map(u32::from).sum();
    let bucket = sum % 40;
    #[allow(clippy::cast_precision_loss)]
    {
        0.60 + (bucket as f32) / 100.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(id: &str) -> FileContext {
        FileContext {
            file_id: bower_core::model::FileId::for_path(std::path::Path::new(id)),
            file_name: id.to_owned(),
            relative_dir: None,
            extension: None,
            size_bytes: 0,
            mime: None,
            content_excerpt: None,
        }
    }

    #[test]
    fn confidence_stays_in_band() {
        for id in ["f_00000000", "f_ffffffff", "f_0af3c1aa"] {
            let c = confidence(&file(id));
            assert!((0.60..=0.99).contains(&c), "{id} -> {c}");
        }
    }

    /// The engine fills `{date}` from the file's mtime, which the stub is not
    /// told. Proposing one would be proposing a value that is ignored.
    #[test]
    fn the_stub_does_not_propose_a_date_token() {
        assert!(!tokens(&file("invoice.pdf")).contains_key("date"));
    }
}
