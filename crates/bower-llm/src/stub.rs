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

use bower_core::llm::{BatchRequest, BatchResponse, LlmBackend, LlmError};
use bower_core::model::{FileRecord, Proposal, ProposalOutcome, RawProposal};
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

    fn classify(&self, request: BatchRequest<'_>) -> Result<BatchResponse, LlmError> {
        let mut outcomes = BTreeMap::new();
        for file in request.files {
            let guess = guess_category(file);
            let category = resolve_against(request.profile, &guess);
            let is_new_category =
                !request.profile.categories.iter().any(|c| c.eq_ignore_ascii_case(&category));

            let proposal = RawProposal {
                file_id: file.id.clone(),
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
            outcomes.insert(file.id.clone(), ProposalOutcome::Ok(Proposal::Categorize(proposal)));
        }
        Ok(BatchResponse { outcomes })
    }
}

fn guess_category(file: &FileRecord) -> String {
    let ext = file.extension.as_deref().unwrap_or_default();
    GUESSES
        .iter()
        .find(|(_, exts)| exts.contains(&ext))
        .map_or(FALLBACK, |(name, _)| *name)
        .to_owned()
}

/// Prefers a category the profile actually declares, so the stub does not
/// invent categories on profiles that forbid them.
fn resolve_against(profile: &bower_config::Profile, guess: &str) -> String {
    if let Some(declared) = profile.categories.iter().find(|c| c.eq_ignore_ascii_case(guess)) {
        return declared.clone();
    }
    if profile.allow_dynamic_categories {
        return guess.to_owned();
    }
    profile.categories.first().cloned().unwrap_or_else(|| guess.to_owned())
}

fn tokens(file: &FileRecord) -> BTreeMap<String, String> {
    let mut tokens = BTreeMap::new();
    let (stem, _) = bower_core::model::split_extension(file.file_name());
    tokens.insert("name".to_owned(), stem.to_owned());
    tokens.insert("vendor".to_owned(), first_word(stem).to_owned());
    tokens.insert("doc_type".to_owned(), guess_category(file).to_lowercase());
    tokens.insert("date".to_owned(), date_of(file));
    tokens
}

fn first_word(stem: &str) -> &str {
    stem.split(['-', '_', ' ', '.']).find(|s| !s.is_empty()).unwrap_or(stem)
}

/// The file's mtime as `YYYY-MM-DD`, computed from the Unix epoch by civil-date
/// arithmetic so the stub needs no date crate.
fn date_of(file: &FileRecord) -> String {
    let secs = file.facts.mtime.duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let (y, m, d) = civil_from_days(i64::try_from(secs / 86_400).unwrap_or(0));
    format!("{y:04}-{m:02}-{d:02}")
}

/// Howard Hinnant's `civil_from_days`, the standard branch-free conversion from
/// a days-since-epoch count to a proleptic Gregorian date.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, u32::try_from(m).unwrap_or(1), u32::try_from(d).unwrap_or(1))
}

/// Deterministic pseudo-confidence in `0.60..=0.99`, derived from the file id so
/// it is stable across runs but varied across files.
///
/// The band deliberately straddles a typical threshold: a demo run should show
/// files being filed *and* files being held back, since exercising the
/// manual-review path is most of the point of having a stub at all.
fn confidence(file: &FileRecord) -> f32 {
    let sum: u32 = file.id.as_str().bytes().map(u32::from).sum();
    let bucket = sum % 40;
    #[allow(clippy::cast_precision_loss)]
    {
        0.60 + (bucket as f32) / 100.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_and_a_known_date_convert_correctly() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        // 2024-03-15 is 19797 days after the epoch.
        assert_eq!(civil_from_days(19_797), (2024, 3, 15));
    }

    #[test]
    fn confidence_stays_in_band() {
        for id in ["f_00000000", "f_ffffffff", "f_0af3c1aa"] {
            let file = FileRecord {
                id: bower_core::model::FileId::for_path(std::path::Path::new(id)),
                path: std::path::PathBuf::from(id),
                relative: std::path::PathBuf::from(id),
                facts: bower_core::model::FileFacts { size: 0, mtime: std::time::UNIX_EPOCH },
                extension: None,
                mime: None,
                content_snippet: None,
            };
            let c = confidence(&file);
            assert!((0.60..=0.99).contains(&c), "{id} -> {c}");
        }
    }
}
