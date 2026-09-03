#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::duration_suboptimal_units
)]
//! Scanner behaviour, against real temporary directories.

use bower_config::{Metadata, OnConflict, Profile, Rename};
use bower_core::scan::{ScanOptions, ScanReport, SkipReason, scan};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

fn profile(path: &Path) -> Profile {
    Profile {
        name: "test".to_owned(),
        path: path.to_path_buf(),
        description: String::new(),
        enabled: true,
        llm_backend: "local".to_owned(),
        destination_root: path.to_path_buf(),
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

fn touch(path: &Path, body: &[u8]) {
    if let Some(p) = path.parent() {
        fs::create_dir_all(p).unwrap();
    }
    fs::write(path, body).unwrap();
}

fn names(report: &ScanReport) -> Vec<String> {
    let mut n: Vec<_> = report.files.iter().map(|f| f.relative.display().to_string()).collect();
    n.sort();
    n
}

fn skipped_for(report: &ScanReport, reason: &SkipReason) -> Vec<PathBuf> {
    report.skipped.iter().filter(|s| &s.reason == reason).map(|s| s.path.clone()).collect()
}

#[test]
fn finds_files_and_ignores_directories() {
    let dir = tempfile::tempdir().unwrap();
    touch(&dir.path().join("a.pdf"), b"a");
    touch(&dir.path().join("b.png"), b"b");
    fs::create_dir_all(dir.path().join("empty")).unwrap();

    let report = scan(&profile(dir.path()), &ScanOptions::default()).unwrap();
    assert_eq!(names(&report), ["a.pdf", "b.png"]);
}

#[test]
fn top_level_only_unless_subdirs_are_requested() {
    let dir = tempfile::tempdir().unwrap();
    touch(&dir.path().join("top.pdf"), b"a");
    touch(&dir.path().join("nested/deep.pdf"), b"b");

    let mut p = profile(dir.path());
    assert_eq!(names(&scan(&p, &ScanOptions::default()).unwrap()), ["top.pdf"]);

    p.include_subdirs = true;
    assert_eq!(names(&scan(&p, &ScanOptions::default()).unwrap()), ["nested/deep.pdf", "top.pdf"]);
}

/// The regression test for ADR-0001's contradiction: it says to exclude
/// `destination_root`, and that `destination_root` defaults to `path`. Applied
/// literally to an in-place profile, that excludes the entire scan.
#[test]
fn an_in_place_profile_scans_its_own_root() {
    let dir = tempfile::tempdir().unwrap();
    touch(&dir.path().join("a.pdf"), b"a");

    let p = profile(dir.path());
    assert!(p.is_in_place());

    let report = scan(&p, &ScanOptions::default()).unwrap();
    assert_eq!(names(&report), ["a.pdf"], "an in-place profile must not exclude itself");
}

#[test]
fn an_in_place_profile_does_not_re_ingest_its_category_directories() {
    let dir = tempfile::tempdir().unwrap();
    touch(&dir.path().join("fresh.pdf"), b"a");
    touch(&dir.path().join("Documents/filed.pdf"), b"b");
    touch(&dir.path().join("Images/filed.png"), b"c");
    touch(&dir.path().join("Other/unmanaged.txt"), b"d");

    let mut p = profile(dir.path());
    p.include_subdirs = true;

    let report = scan(&p, &ScanOptions::default()).unwrap();
    assert_eq!(
        names(&report),
        ["Other/unmanaged.txt", "fresh.pdf"],
        "managed category directories must be skipped; unmanaged ones must not be"
    );
}

#[test]
fn a_routed_profile_skips_its_destination_root() {
    let dir = tempfile::tempdir().unwrap();
    touch(&dir.path().join("fresh.pdf"), b"a");
    touch(&dir.path().join("organized/Documents/filed.pdf"), b"b");

    let mut p = profile(dir.path());
    p.destination_root = dir.path().join("organized");
    p.include_subdirs = true;
    assert!(!p.is_in_place());

    assert_eq!(names(&scan(&p, &ScanOptions::default()).unwrap()), ["fresh.pdf"]);
}

#[test]
fn the_quarantine_and_recycle_stores_are_never_re_ingested() {
    let dir = tempfile::tempdir().unwrap();
    touch(&dir.path().join("fresh.pdf"), b"a");
    touch(&dir.path().join("_review/held.pdf"), b"b");
    touch(&dir.path().join("_recycled/gone.pdf"), b"c");

    let mut p = profile(dir.path());
    p.include_subdirs = true;

    let options = ScanOptions {
        extra_excluded_roots: vec![dir.path().join("_review"), dir.path().join("_recycled")],
    };
    assert_eq!(names(&scan(&p, &options).unwrap()), ["fresh.pdf"]);
}

#[test]
fn exclude_patterns_match_names_and_relative_paths() {
    let dir = tempfile::tempdir().unwrap();
    touch(&dir.path().join("keep.pdf"), b"a");
    touch(&dir.path().join("big.part"), b"b");
    touch(&dir.path().join(".DS_Store"), b"c");
    touch(&dir.path().join("nested/skip.tmp"), b"d");

    let mut p = profile(dir.path());
    p.include_subdirs = true;
    p.exclude_patterns =
        vec!["*.part".to_owned(), ".DS_Store".to_owned(), "nested/*.tmp".to_owned()];

    let report = scan(&p, &ScanOptions::default()).unwrap();
    assert_eq!(names(&report), ["keep.pdf"]);
    assert_eq!(skipped_for(&report, &SkipReason::ExcludedByPattern).len(), 3);
}

#[test]
fn a_file_still_being_written_is_left_to_settle() {
    let dir = tempfile::tempdir().unwrap();
    touch(&dir.path().join("downloading.pdf"), b"partial");

    let mut p = profile(dir.path());
    p.stability_wait = Duration::from_secs(3600);

    let report = scan(&p, &ScanOptions::default()).unwrap();
    assert!(report.files.is_empty());
    assert_eq!(skipped_for(&report, &SkipReason::StillSettling).len(), 1);
}

#[test]
#[cfg(unix)]
fn symlinks_are_never_followed_or_moved() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    touch(&outside.path().join("secret.txt"), b"secret");
    touch(&dir.path().join("real.pdf"), b"a");
    std::os::unix::fs::symlink(outside.path().join("secret.txt"), dir.path().join("link.txt"))
        .unwrap();
    std::os::unix::fs::symlink(outside.path(), dir.path().join("linkdir")).unwrap();

    let mut p = profile(dir.path());
    p.include_subdirs = true;

    let report = scan(&p, &ScanOptions::default()).unwrap();
    assert_eq!(names(&report), ["real.pdf"], "a symlink must never become an input");
    assert!(!skipped_for(&report, &SkipReason::Symlink).is_empty());
}

#[test]
fn records_carry_the_facts_the_policy_engine_needs() {
    let dir = tempfile::tempdir().unwrap();
    // A real PNG header, so magic-byte detection has something to find.
    touch(&dir.path().join("image.PNG"), b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR");

    let report = scan(&profile(dir.path()), &ScanOptions::default()).unwrap();
    let f = report.files.first().expect("one file");

    assert_eq!(f.extension.as_deref(), Some("png"), "extension is normalized to lowercase");
    assert_eq!(f.mime.as_deref(), Some("image/png"), "MIME comes from magic bytes, not extension");
    assert_eq!(f.facts.size, 16);
    assert_eq!(f.file_name(), "image.PNG");
    assert!(f.path.is_absolute());
}

#[test]
fn content_snippets_appear_only_when_asked_for() {
    let dir = tempfile::tempdir().unwrap();
    touch(&dir.path().join("note.txt"), b"Invoice 12345 from Acme Corp");

    let mut p = profile(dir.path());
    assert!(scan(&p, &ScanOptions::default()).unwrap().files[0].content_snippet.is_none());

    p.metadata.content_sniff_bytes = 4000;
    let snippet = scan(&p, &ScanOptions::default()).unwrap().files[0]
        .content_snippet
        .clone()
        .expect("snippet requested");
    assert!(snippet.contains("Acme"), "{snippet}");
}

#[test]
fn a_missing_source_directory_is_an_error_not_an_empty_scan() {
    let dir = tempfile::tempdir().unwrap();
    let p = profile(&dir.path().join("does-not-exist"));
    assert!(scan(&p, &ScanOptions::default()).is_err());
}

#[test]
fn an_invalid_exclude_pattern_names_the_pattern() {
    let dir = tempfile::tempdir().unwrap();
    let mut p = profile(dir.path());
    p.exclude_patterns = vec!["[unclosed".to_owned()];

    let err = scan(&p, &ScanOptions::default()).unwrap_err().to_string();
    assert!(err.contains("[unclosed"), "{err}");
}

#[test]
fn file_ids_are_stable_across_scans_and_distinct_between_files() {
    let dir = tempfile::tempdir().unwrap();
    touch(&dir.path().join("a.pdf"), b"a");
    touch(&dir.path().join("b.pdf"), b"b");

    let first = scan(&profile(dir.path()), &ScanOptions::default()).unwrap();
    let second = scan(&profile(dir.path()), &ScanOptions::default()).unwrap();

    let ids: Vec<_> = first.files.iter().map(|f| f.id.clone()).collect();
    let again: Vec<_> = second.files.iter().map(|f| f.id.clone()).collect();
    assert_eq!(ids, again, "ids must survive between runs so rejections stay matched");
    assert_ne!(ids[0], ids[1]);
}
