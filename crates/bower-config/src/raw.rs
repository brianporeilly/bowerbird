//! Verbatim deserialization of the TOML file.
//!
//! These types mirror the file's shape exactly and apply no defaulting beyond
//! serde's. Every table sets `deny_unknown_fields` so a typo'd or misplaced key
//! is a hard error rather than a silently ignored setting -- important for a
//! file whose keys gate filesystem mutation.

use serde::Deserialize;
use std::path::PathBuf;

use crate::{OnConflict, Provider, ReviewPlacement};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawConfig {
    pub config_version: u32,
    #[serde(default)]
    pub general: RawGeneral,
    #[serde(default)]
    pub llm_backends: Vec<RawBackend>,
    #[serde(default)]
    pub profiles: Vec<RawProfile>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawGeneral {
    #[serde(default = "default_true")]
    pub dry_run: bool,
    #[serde(default = "default_state_path")]
    pub state_path: PathBuf,
    #[serde(default = "default_lock_dir")]
    pub lock_file_dir: PathBuf,
    #[serde(default = "default_log_level")]
    pub log_level: String,
    #[serde(default = "default_batch_size")]
    pub default_batch_size: usize,
    #[serde(default = "default_threshold")]
    pub default_confidence_threshold: f32,
    #[serde(default)]
    pub review_placement: ReviewPlacement,
    pub quarantine_dir: Option<PathBuf>,
    pub recycle_dir: Option<PathBuf>,
}

impl Default for RawGeneral {
    fn default() -> Self {
        Self {
            dry_run: true,
            state_path: default_state_path(),
            lock_file_dir: default_lock_dir(),
            log_level: default_log_level(),
            default_batch_size: default_batch_size(),
            default_confidence_threshold: default_threshold(),
            review_placement: ReviewPlacement::default(),
            quarantine_dir: None,
            recycle_dir: None,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawBackend {
    pub name: String,
    pub provider: Provider,
    pub endpoint: String,
    #[serde(default)]
    pub api_key_env: String,
    pub model: String,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    #[serde(default = "default_retries")]
    pub max_retries: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawProfile {
    pub name: String,
    pub path: PathBuf,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub llm_backend: String,
    /// Omitted => defaults to `path`, i.e. in-place organization into category
    /// subdirectories. See `Profile::destination_root`.
    pub destination_root: Option<PathBuf>,
    #[serde(default)]
    pub categories: Vec<String>,
    #[serde(default)]
    pub allow_dynamic_categories: bool,
    #[serde(default)]
    pub allow_delete_suggestions: bool,
    pub batch_size: Option<usize>,
    pub confidence_threshold: Option<f32>,
    #[serde(default)]
    pub on_conflict: OnConflict,
    #[serde(default)]
    pub stability_wait_minutes: u64,
    #[serde(default)]
    pub exclude_patterns: Vec<String>,
    #[serde(default)]
    pub include_subdirs: bool,
    #[serde(default)]
    pub rename: RawRename,
    #[serde(default)]
    pub metadata: RawMetadata,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawRename {
    #[serde(default)]
    pub enabled: bool,
    pub template: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawMetadata {
    #[serde(default = "default_true")]
    pub detect_mime: bool,
    #[serde(default)]
    pub extract_exif: bool,
    #[serde(default)]
    pub extract_audio_tags: bool,
    #[serde(default)]
    pub extract_pdf_metadata: bool,
    #[serde(default)]
    pub content_sniff_bytes: usize,
}

impl Default for RawMetadata {
    fn default() -> Self {
        Self {
            detect_mime: true,
            extract_exif: false,
            extract_audio_tags: false,
            extract_pdf_metadata: false,
            content_sniff_bytes: 0,
        }
    }
}

fn default_true() -> bool {
    true
}
fn default_state_path() -> PathBuf {
    PathBuf::from("/var/lib/bowerbird/state.db")
}
fn default_lock_dir() -> PathBuf {
    PathBuf::from("/var/lib/bowerbird/locks")
}
fn default_log_level() -> String {
    "info".to_owned()
}
fn default_batch_size() -> usize {
    25
}
fn default_threshold() -> f32 {
    0.75
}
fn default_timeout() -> u64 {
    30
}
fn default_retries() -> u32 {
    2
}
