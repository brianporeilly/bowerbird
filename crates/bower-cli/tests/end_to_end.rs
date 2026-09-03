#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::duration_suboptimal_units
)]
//! End-to-end runs of the `bower` binary against throwaway directory trees,
//! driven by the offline classifier so they need no model or network.

use assert_cmd::Command;
use predicates::str::contains;
use std::fs;
use std::path::Path;

/// Exit codes, per ADR-0001: 0 clean, 1 hard error, 2 needs a human.
const OK: i32 = 0;
const ERROR: i32 = 1;
const ATTENTION: i32 = 2;

struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("downloads")).unwrap();
        // The stub's confidence is derived from the file id, which is derived
        // from the absolute path, so each fixture gets its own spread. The
        // threshold below is set to 0 where a test needs every file to move.
        fs::write(root.join("downloads/acme-invoice.pdf"), "invoice").unwrap();
        fs::write(root.join("downloads/holiday.png"), "png").unwrap();
        fs::write(root.join("downloads/partial.part"), "partial").unwrap();
        Self { dir }
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    /// Writes a config with the given confidence threshold.
    fn config(&self, threshold: f32) -> std::path::PathBuf {
        let root = self.root().display();
        let text = format!(
            r#"
config_version = 1

[general]
dry_run = true
state_path = "{root}/state.db"
lock_file_dir = "{root}/locks"
quarantine_dir = "{root}/_review"

[[llm_backends]]
name = "local"
provider = "openai_compatible"
endpoint = "http://localhost:8080/v1"
model = "test"

[[profiles]]
name = "downloads"
path = "{root}/downloads"
description = "test fixture"
llm_backend = "local"
categories = ["Documents", "Images"]
allow_dynamic_categories = true
confidence_threshold = {threshold}
on_conflict = "quarantine"
exclude_patterns = ["*.part"]
include_subdirs = true
"#
        );
        let path = self.root().join("bowerbird.toml");
        fs::write(&path, text).unwrap();
        path
    }

    fn bower(&self, threshold: f32) -> Command {
        let mut cmd = Command::cargo_bin("bower").unwrap();
        cmd.arg("--config").arg(self.config(threshold));
        cmd
    }
}

#[test]
fn config_check_validates_without_running_anything() {
    let f = Fixture::new();
    f.bower(0.0)
        .arg("config")
        .arg("check")
        .assert()
        .code(OK)
        .stdout(contains("is valid"))
        .stdout(contains("downloads"));

    assert!(f.root().join("downloads/acme-invoice.pdf").exists(), "check must not move anything");
}

#[test]
fn a_dry_run_prints_a_plan_and_writes_nothing() {
    let f = Fixture::new();
    f.bower(0.0)
        .args(["run", "--profile", "downloads", "--stub-llm"])
        .assert()
        .code(OK)
        .stdout(contains("dry run"))
        .stdout(contains("MOVE"));

    assert!(f.root().join("downloads/acme-invoice.pdf").exists());
    assert!(!f.root().join("downloads/Documents").exists(), "not even a directory");
}

#[test]
fn execute_moves_files_into_category_directories() {
    let f = Fixture::new();
    f.bower(0.0)
        .args(["run", "--profile", "downloads", "--stub-llm", "--execute"])
        .assert()
        .code(OK);

    assert!(f.root().join("downloads/Documents/acme-invoice.pdf").exists());
    assert!(f.root().join("downloads/Images/holiday.png").exists());
    assert!(!f.root().join("downloads/acme-invoice.pdf").exists());
    assert!(f.root().join("downloads/partial.part").exists(), "excluded files are untouched");
}

#[test]
fn a_second_run_does_not_re_ingest_the_first_runs_output() {
    let f = Fixture::new();
    let config = f.config(0.0);

    for _ in 0..2 {
        Command::cargo_bin("bower")
            .unwrap()
            .arg("--config")
            .arg(&config)
            .args(["run", "--profile", "downloads", "--stub-llm", "--execute"])
            .assert()
            .code(OK);
    }

    // Exactly one copy of each file, still one level deep.
    assert!(f.root().join("downloads/Documents/acme-invoice.pdf").exists());
    assert!(!f.root().join("downloads/Documents/Documents").exists());
}

#[test]
fn nothing_is_ever_overwritten() {
    let f = Fixture::new();
    let config = f.config(0.0);
    let filed = f.root().join("downloads/Documents/acme-invoice.pdf");

    Command::cargo_bin("bower")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .args(["run", "-p", "downloads", "--stub-llm", "--execute"])
        .assert()
        .code(OK);
    assert_eq!(fs::read_to_string(&filed).unwrap(), "invoice");

    // A different file arrives under the same name.
    fs::write(f.root().join("downloads/acme-invoice.pdf"), "SOMETHING ELSE").unwrap();
    Command::cargo_bin("bower")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .args(["run", "-p", "downloads", "--stub-llm", "--execute"])
        .assert()
        .code(ATTENTION)
        .stdout(contains("QUARANTINE"));

    assert_eq!(fs::read_to_string(&filed).unwrap(), "invoice", "the filed original must survive");
    assert!(f.root().join("downloads/acme-invoice.pdf").exists(), "the newcomer must survive too");
}

#[test]
fn low_confidence_items_are_held_back_and_reported_via_the_exit_code() {
    let f = Fixture::new();
    f.bower(1.0) // nothing can clear this
        .args(["run", "-p", "downloads", "--stub-llm", "--execute"])
        .assert()
        .code(ATTENTION)
        .stdout(contains("REVIEW"));

    assert!(f.root().join("downloads/acme-invoice.pdf").exists(), "nothing should have moved");
}

#[test]
fn a_named_profile_that_does_not_exist_fails_loudly_and_lists_the_real_ones() {
    let f = Fixture::new();
    f.bower(0.0)
        .args(["run", "--profile", "nope", "--stub-llm"])
        .assert()
        .code(ERROR)
        .stderr(contains("no profile named `nope`"))
        .stderr(contains("downloads"));
}

#[test]
fn a_broken_config_reports_every_problem_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("bad.toml");
    fs::write(
        &config,
        "config_version = 1\n\n[[profiles]]\nname = \"bad name!\"\npath = \"relative\"\n\
         llm_backend = \"missing\"\ncategories = [\"../escape\"]\n",
    )
    .unwrap();

    Command::cargo_bin("bower")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .args(["config", "check"])
        .assert()
        .code(ERROR)
        .stderr(contains("name"))
        .stderr(contains("path"))
        .stderr(contains("llm_backend"))
        .stderr(contains("categories[0]"));
}

#[test]
fn a_missing_config_says_where_it_looked() {
    Command::cargo_bin("bower")
        .unwrap()
        .args(["--config", "/nonexistent/bowerbird.toml", "config", "check"])
        .assert()
        .code(ERROR)
        .stderr(contains("/nonexistent/bowerbird.toml"));
}

#[test]
fn an_empty_queue_and_recycle_store_say_so() {
    let f = Fixture::new();
    f.bower(0.0).args(["review", "list"]).assert().code(OK).stdout(contains("nothing is waiting"));
    f.bower(0.0)
        .args(["recycle", "list"])
        .assert()
        .code(OK)
        .stdout(contains("recycle store is empty"));
}

/// Queues everything by setting a threshold nothing can clear.
fn with_pending() -> (Fixture, std::path::PathBuf) {
    let f = Fixture::new();
    let config = f.config(1.0);
    Command::cargo_bin("bower")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .args(["run", "-p", "downloads", "--stub-llm", "--execute"])
        .assert()
        .code(ATTENTION);
    (f, config)
}

fn bower_at(config: &Path) -> Command {
    let mut cmd = Command::cargo_bin("bower").unwrap();
    cmd.arg("--config").arg(config);
    cmd
}

#[test]
fn pending_items_are_listed_and_shown_in_detail() {
    let (_f, config) = with_pending();

    bower_at(&config)
        .args(["review", "list"])
        .assert()
        .code(ATTENTION)
        .stdout(contains("acme-invoice.pdf"))
        .stdout(contains("review"));

    bower_at(&config)
        .args(["review", "show", "1"])
        .assert()
        .code(OK)
        .stdout(contains("would file to"))
        .stdout(contains("held because"))
        .stdout(contains("hash"));
}

#[test]
fn approving_an_item_files_it_where_the_run_would_have() {
    let (f, config) = with_pending();
    assert!(f.root().join("downloads/acme-invoice.pdf").exists());

    bower_at(&config).args(["review", "approve", "1"]).assert().code(OK).stdout(contains("filed"));

    assert!(f.root().join("downloads/Documents/acme-invoice.pdf").exists());
    assert!(!f.root().join("downloads/acme-invoice.pdf").exists());
    bower_at(&config).args(["review", "show", "1"]).assert().code(ERROR);
}

#[test]
fn approving_a_dry_run_reports_without_writing() {
    let (f, config) = with_pending();
    bower_at(&config)
        .args(["review", "approve", "1", "--dry-run"])
        .assert()
        .code(OK)
        .stdout(contains("would be filed"));

    assert!(f.root().join("downloads/acme-invoice.pdf").exists(), "nothing moved");
    bower_at(&config).args(["review", "show", "1"]).assert().code(OK);
}

#[test]
fn approving_a_file_that_changed_since_it_was_queued_is_refused() {
    let (f, config) = with_pending();
    // The file the proposal was about no longer exists in that form.
    fs::write(f.root().join("downloads/acme-invoice.pdf"), "ENTIRELY DIFFERENT").unwrap();

    bower_at(&config)
        .args(["review", "approve", "1"])
        .assert()
        .code(ATTENTION)
        .stderr(contains("has changed"));

    assert!(
        f.root().join("downloads/acme-invoice.pdf").exists(),
        "the file must be left alone, not filed on a stale decision"
    );
    assert!(!f.root().join("downloads/Documents").exists());
}

#[test]
fn approving_a_file_that_vanished_since_it_was_queued_is_refused() {
    let (f, config) = with_pending();
    fs::remove_file(f.root().join("downloads/acme-invoice.pdf")).unwrap();

    bower_at(&config)
        .args(["review", "approve", "1"])
        .assert()
        .code(ATTENTION)
        .stderr(contains("no longer exists"));
}

#[test]
fn rejecting_an_item_stops_it_being_proposed_again() {
    let (f, config) = with_pending();

    bower_at(&config)
        .args(["review", "reject", "1", "--reason", "not an invoice"])
        .assert()
        .code(OK)
        .stdout(contains("will not be proposed again"));

    // A fresh run at a threshold everything clears must still leave it alone.
    let permissive = f.config(0.0);
    bower_at(&permissive)
        .args(["run", "-p", "downloads", "--stub-llm", "--execute"])
        .assert()
        .code(OK);

    assert!(
        f.root().join("downloads/acme-invoice.pdf").exists(),
        "a refused proposal must not be acted on by a later run"
    );
}

#[test]
fn bulk_approval_needs_confirmation_it_cannot_ask_for_without_a_terminal() {
    let (_f, config) = with_pending();
    bower_at(&config)
        .args(["review", "approve", "--all"])
        .assert()
        .code(ERROR)
        .stderr(contains("--yes"));
}

#[test]
fn bulk_approval_files_everything_pending_when_confirmed() {
    let (f, config) = with_pending();
    bower_at(&config).args(["review", "approve", "--all", "--yes"]).assert().code(OK);

    assert!(f.root().join("downloads/Documents/acme-invoice.pdf").exists());
    assert!(f.root().join("downloads/Images/holiday.png").exists());
    bower_at(&config).args(["review", "list"]).assert().code(OK).stdout(contains("nothing"));
}

#[test]
fn the_queue_can_be_filtered_by_profile_and_type() {
    let (_f, config) = with_pending();
    bower_at(&config)
        .args(["review", "list", "--profile", "downloads"])
        .assert()
        .code(ATTENTION)
        .stdout(contains("acme-invoice.pdf"));

    bower_at(&config)
        .args(["review", "list", "--type", "delete"])
        .assert()
        .code(OK)
        .stdout(contains("nothing is waiting"));
}

#[test]
fn purge_refuses_a_duration_it_cannot_read() {
    let (_f, config) = with_pending();
    bower_at(&config)
        .args(["recycle", "purge", "--older-than", "banana"])
        .assert()
        .code(ERROR)
        .stderr(contains("duration"));
}

#[test]
fn a_held_lock_stops_a_named_profile_but_only_skips_under_all() {
    let f = Fixture::new();
    let config = f.config(0.0);
    let locks = f.root().join("locks");
    fs::create_dir_all(&locks).unwrap();
    fs::write(locks.join("downloads.lock"), "99999\n").unwrap();

    Command::cargo_bin("bower")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .args(["run", "-p", "downloads", "--stub-llm", "--execute"])
        .assert()
        .code(ERROR)
        .stderr(contains("already running"));

    Command::cargo_bin("bower")
        .unwrap()
        .arg("--config")
        .arg(&config)
        .args(["run", "--all", "--stub-llm", "--execute"])
        .assert()
        .code(OK)
        .stderr(contains("skipping"));

    assert!(f.root().join("downloads/acme-invoice.pdf").exists(), "a locked profile must not run");
}

#[test]
fn a_dry_run_does_not_contend_for_the_lock() {
    let f = Fixture::new();
    let locks = f.root().join("locks");
    fs::create_dir_all(&locks).unwrap();
    fs::write(locks.join("downloads.lock"), "99999\n").unwrap();

    // Previewing what a scheduled run would do should not require waiting for it.
    f.bower(0.0).args(["run", "-p", "downloads", "--stub-llm"]).assert().code(OK);
}
