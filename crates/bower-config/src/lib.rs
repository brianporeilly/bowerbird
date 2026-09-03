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
//! Bowerbird configuration: schema, defaulting, and validation.
//!
//! Loading is a two-step process. [`raw`] deserializes the TOML file verbatim,
//! rejecting unknown keys. Validation then resolves every optional or inherited
//! value into a concrete one and returns the types in this module, so that
//! downstream crates never re-implement a defaulting rule or handle a
//! half-specified profile.

mod error;
mod raw;

pub use error::{ConfigError, Problem};

use serde::Deserialize;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The config schema version this build understands.
pub const SUPPORTED_CONFIG_VERSION: u32 = 1;

/// A fully validated configuration. Every value is concrete: profile-level
/// overrides have been folded in and `destination_root` has been defaulted.
#[derive(Debug, Clone)]
pub struct Config {
    pub general: General,
    pub backends: Vec<Backend>,
    pub profiles: Vec<Profile>,
}

#[derive(Debug, Clone)]
pub struct General {
    pub dry_run: bool,
    /// SQLite file holding *both* the append-only journal and the mutable
    /// review queue.
    pub state_path: PathBuf,
    /// Locking is per profile: `lock_file_dir/<profile>.lock`.
    pub lock_file_dir: PathBuf,
    pub log_level: String,
    pub review_placement: ReviewPlacement,
    pub quarantine_dir: Option<PathBuf>,
    pub recycle_dir: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct Backend {
    pub name: String,
    pub provider: Provider,
    pub endpoint: String,
    /// Name of the environment variable holding the API key. Secrets are never
    /// stored in the config file itself. `None` for unauthenticated endpoints.
    pub api_key_env: Option<String>,
    pub model: String,
    pub timeout: Duration,
    pub max_retries: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    OpenaiCompatible,
    AnthropicCompatible,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnConflict {
    /// Leave the source file where it is and record the collision.
    Skip,
    /// Append a numeric suffix to the destination filename.
    Suffix,
    /// Park the file for a human decision.
    #[default]
    Quarantine,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewPlacement {
    /// Pending items stay untouched at their original path.
    #[default]
    InPlace,
    /// Pending items are physically moved to a holding folder so non-CLI users
    /// can browse them directly.
    Quarantine,
}

/// Renaming is either off, or on *with* a template. "Enabled but no template"
/// is not representable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rename {
    Disabled,
    Enabled { template: String },
}

impl Rename {
    #[must_use]
    pub fn template(&self) -> Option<&str> {
        match self {
            Self::Disabled => None,
            Self::Enabled { template } => Some(template),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Metadata {
    pub detect_mime: bool,
    pub extract_exif: bool,
    pub extract_audio_tags: bool,
    pub extract_pdf_metadata: bool,
    /// Bytes of file content to include in the LLM prompt. `0` disables
    /// content sniffing entirely.
    pub content_sniff_bytes: usize,
}

#[derive(Debug, Clone)]
pub struct Profile {
    pub name: String,
    /// Absolute path of the directory to scan.
    pub path: PathBuf,
    /// Free text handed to the LLM describing what this directory is for.
    pub description: String,
    pub enabled: bool,
    /// Name of the [`Backend`] this profile routes to.
    pub llm_backend: String,
    /// Root under which every destination path is built. Defaults to
    /// [`Profile::path`] when omitted from the file, giving in-place
    /// organization into category subdirectories; pointing it elsewhere is
    /// equally supported and often preferable.
    pub destination_root: PathBuf,
    pub categories: Vec<String>,
    pub allow_dynamic_categories: bool,
    pub allow_delete_suggestions: bool,
    pub batch_size: usize,
    pub confidence_threshold: f32,
    pub on_conflict: OnConflict,
    /// How long a file must have been untouched before it is considered
    /// settled enough to act on.
    pub stability_wait: Duration,
    pub exclude_patterns: Vec<String>,
    pub include_subdirs: bool,
    pub rename: Rename,
    pub metadata: Metadata,
}

impl Profile {
    /// True when this profile organizes files in place, i.e. its destination
    /// root is the directory it scans. In that case the scanner must skip the
    /// category subdirectories it manages rather than skipping the root.
    #[must_use]
    pub fn is_in_place(&self) -> bool {
        self.destination_root == self.path
    }
}

impl Config {
    /// Reads and validates a config file.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)
            .map_err(|source| ConfigError::Read { path: path.to_path_buf(), source })?;
        Self::parse(&text, path)
    }

    /// Validates already-read config text. `origin` is used only for error
    /// messages.
    pub fn parse(text: &str, origin: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let raw: raw::RawConfig = toml::from_str(text)
            .map_err(|source| ConfigError::Parse { path: origin.as_ref().to_path_buf(), source })?;
        validate(raw)
    }

    #[must_use]
    pub fn profile(&self, name: &str) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.name == name)
    }

    #[must_use]
    pub fn backend(&self, name: &str) -> Option<&Backend> {
        self.backends.iter().find(|b| b.name == name)
    }

    /// The backend a profile routes to. Always `Some` for a validated config.
    #[must_use]
    pub fn backend_for(&self, profile: &Profile) -> Option<&Backend> {
        self.backend(&profile.llm_backend)
    }

    pub fn enabled_profiles(&self) -> impl Iterator<Item = &Profile> {
        self.profiles.iter().filter(|p| p.enabled)
    }
}

/// Characters that are unsafe in a path component on the filesystems Bowerbird
/// targets. `/` and `\` are separators; the rest are reserved on Windows and
/// merely inconvenient elsewhere.
const FORBIDDEN_IN_COMPONENT: &[char] = &['/', '\\', ':', '*', '?', '"', '<', '>', '|'];

/// Whether `s` is safe to use verbatim as exactly one *directory* component.
///
/// This is the single authority on the question: config validation checks
/// user-declared categories against it, and the policy engine checks
/// LLM-proposed ones against it, so the two can never disagree.
///
/// Stricter than [`is_safe_filename`] in one respect: a leading dot is
/// rejected. A category is a directory the tool creates and reuses, and a
/// model-proposed `.git` or `.ssh` is not a category anyone asked for.
#[must_use]
pub fn is_safe_component(s: &str) -> bool {
    is_safe_filename(s) && !s.starts_with('.')
}

/// Whether `s` is safe to use verbatim as exactly one *file* name.
///
/// Unlike [`is_safe_component`] this permits a leading dot, because a dotfile
/// cannot be used to escape a directory and refusing to file `.bashrc` under
/// its own name would be a surprise, not a safeguard. `.` and `..` are still
/// rejected -- those are traversal, not names.
#[must_use]
pub fn is_safe_filename(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 255
        && s != "."
        && s != ".."
        && s.trim() == s
        && !s.ends_with('.')
        && !s.chars().any(|c| c.is_control() || FORBIDDEN_IN_COMPONENT.contains(&c))
}

/// Whether `s` is usable as a profile name. Profile names become lock file
/// names, so they are held to a stricter standard than path components.
#[must_use]
pub fn is_safe_profile_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[allow(clippy::too_many_lines)]
fn validate(raw: raw::RawConfig) -> Result<Config, ConfigError> {
    let mut problems = Vec::new();

    if raw.config_version != SUPPORTED_CONFIG_VERSION {
        problems.push(Problem::new(
            "config_version",
            format!(
                "unsupported version {}; this build understands {SUPPORTED_CONFIG_VERSION}",
                raw.config_version
            ),
        ));
    }

    let g = raw.general;
    if !(0.0..=1.0).contains(&g.default_confidence_threshold) {
        problems.push(Problem::new(
            "general.default_confidence_threshold",
            "must be between 0.0 and 1.0",
        ));
    }
    if g.default_batch_size == 0 {
        problems.push(Problem::new("general.default_batch_size", "must be at least 1"));
    }

    // ---- backends ----
    let mut backends = Vec::with_capacity(raw.llm_backends.len());
    let mut seen_backends = BTreeSet::new();
    for (i, b) in raw.llm_backends.into_iter().enumerate() {
        let at = format!("llm_backends[{i}]");
        if b.name.trim().is_empty() {
            problems.push(Problem::new(format!("{at}.name"), "must not be empty"));
        } else if !seen_backends.insert(b.name.clone()) {
            problems.push(Problem::new(
                format!("{at}.name"),
                format!("duplicate backend name `{}`", b.name),
            ));
        }
        if b.endpoint.trim().is_empty() {
            problems.push(Problem::new(format!("{at}.endpoint"), "must not be empty"));
        }
        if b.model.trim().is_empty() {
            problems.push(Problem::new(format!("{at}.model"), "must not be empty"));
        }
        if b.timeout_secs == 0 {
            problems.push(Problem::new(format!("{at}.timeout_secs"), "must be at least 1"));
        }
        backends.push(Backend {
            name: b.name,
            provider: b.provider,
            api_key_env: (!b.api_key_env.trim().is_empty()).then_some(b.api_key_env),
            endpoint: b.endpoint,
            model: b.model,
            timeout: Duration::from_secs(b.timeout_secs),
            max_retries: b.max_retries,
        });
    }

    // ---- profiles ----
    let mut profiles = Vec::with_capacity(raw.profiles.len());
    let mut seen_profiles = BTreeSet::new();
    let mut needs_quarantine_dir = g.review_placement == ReviewPlacement::Quarantine;
    let mut needs_recycle_dir = false;

    for (i, p) in raw.profiles.into_iter().enumerate() {
        let at = if is_safe_profile_name(&p.name) {
            format!("profiles[{}]", p.name)
        } else {
            format!("profiles[{i}]")
        };

        if !is_safe_profile_name(&p.name) {
            problems.push(Problem::new(
                format!("{at}.name"),
                "must be 1-64 characters of [A-Za-z0-9_-]; it is used as a lock file name",
            ));
        } else if !seen_profiles.insert(p.name.clone()) {
            problems.push(Problem::new(
                format!("{at}.name"),
                format!("duplicate profile name `{}`", p.name),
            ));
        }

        if !p.path.is_absolute() {
            problems.push(Problem::new(format!("{at}.path"), "must be an absolute path"));
        }
        if let Some(root) = &p.destination_root
            && !root.is_absolute()
        {
            problems
                .push(Problem::new(format!("{at}.destination_root"), "must be an absolute path"));
        }

        if !seen_backends.contains(&p.llm_backend) {
            problems.push(Problem::new(
                format!("{at}.llm_backend"),
                format!("no [[llm_backends]] entry named `{}`", p.llm_backend),
            ));
        }

        for (ci, c) in p.categories.iter().enumerate() {
            if !is_safe_component(c) {
                problems.push(Problem::new(
                    format!("{at}.categories[{ci}]"),
                    format!("`{c}` is not usable as a single directory name"),
                ));
            }
        }
        if p.categories.is_empty() && !p.allow_dynamic_categories {
            problems.push(Problem::new(
                format!("{at}.categories"),
                "must not be empty unless allow_dynamic_categories = true",
            ));
        }

        let confidence_threshold = p.confidence_threshold.unwrap_or(g.default_confidence_threshold);
        if !(0.0..=1.0).contains(&confidence_threshold) {
            problems.push(Problem::new(
                format!("{at}.confidence_threshold"),
                "must be between 0.0 and 1.0",
            ));
        }
        let batch_size = p.batch_size.unwrap_or(g.default_batch_size);
        if batch_size == 0 {
            problems.push(Problem::new(format!("{at}.batch_size"), "must be at least 1"));
        }

        let rename = match (p.rename.enabled, p.rename.template) {
            (false, _) => Rename::Disabled,
            (true, Some(t)) if !t.trim().is_empty() => Rename::Enabled { template: t },
            (true, _) => {
                problems.push(Problem::new(
                    format!("{at}.rename.template"),
                    "is required when rename.enabled = true",
                ));
                Rename::Disabled
            }
        };

        if p.on_conflict == OnConflict::Quarantine {
            needs_quarantine_dir = true;
        }
        if p.allow_delete_suggestions {
            needs_recycle_dir = true;
        }

        let destination_root = p.destination_root.unwrap_or_else(|| p.path.clone());
        profiles.push(Profile {
            name: p.name,
            path: p.path,
            description: p.description,
            enabled: p.enabled,
            llm_backend: p.llm_backend,
            destination_root,
            categories: p.categories,
            allow_dynamic_categories: p.allow_dynamic_categories,
            allow_delete_suggestions: p.allow_delete_suggestions,
            batch_size,
            confidence_threshold,
            on_conflict: p.on_conflict,
            stability_wait: Duration::from_secs(p.stability_wait_minutes * 60),
            exclude_patterns: p.exclude_patterns,
            include_subdirs: p.include_subdirs,
            rename,
            metadata: Metadata {
                detect_mime: p.metadata.detect_mime,
                extract_exif: p.metadata.extract_exif,
                extract_audio_tags: p.metadata.extract_audio_tags,
                extract_pdf_metadata: p.metadata.extract_pdf_metadata,
                content_sniff_bytes: p.metadata.content_sniff_bytes,
            },
        });
    }

    if profiles.is_empty() {
        problems.push(Problem::new("profiles", "at least one [[profiles]] entry is required"));
    }
    if needs_quarantine_dir && g.quarantine_dir.is_none() {
        problems.push(Problem::new(
            "general.quarantine_dir",
            "is required when review_placement or any profile's on_conflict is \"quarantine\"",
        ));
    }
    if needs_recycle_dir && g.recycle_dir.is_none() {
        problems.push(Problem::new(
            "general.recycle_dir",
            "is required when any profile sets allow_delete_suggestions = true",
        ));
    }

    if problems.is_empty() {
        Ok(Config {
            general: General {
                dry_run: g.dry_run,
                state_path: g.state_path,
                lock_file_dir: g.lock_file_dir,
                log_level: g.log_level,
                review_placement: g.review_placement,
                quarantine_dir: g.quarantine_dir,
                recycle_dir: g.recycle_dir,
            },
            backends,
            profiles,
        })
    } else {
        Err(ConfigError::Invalid { problems })
    }
}
