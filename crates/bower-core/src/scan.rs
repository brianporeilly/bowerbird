//! Directory scanning: turning a profile's source directory into
//! [`FileRecord`]s.

use bower_config::Profile;
use globset::{Glob, GlobSet, GlobSetBuilder};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::model::{FileFacts, FileId, FileRecord};

/// Bytes read from the head of a file for magic-byte MIME detection.
const SNIFF_WINDOW: usize = 8192;

#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    #[error("profile `{profile}` has an invalid exclude pattern `{pattern}`")]
    BadPattern {
        profile: String,
        pattern: String,
        #[source]
        source: globset::Error,
    },
    #[error("could not read source directory {path}")]
    Unreadable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Why a candidate file was not turned into a [`FileRecord`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    ExcludedByPattern,
    /// Inside a directory this profile manages as output.
    ManagedOutput,
    /// Modified too recently to be considered settled.
    StillSettling,
    /// Symlinks are never followed or moved.
    Symlink,
    Unreadable(String),
}

#[derive(Debug, Clone)]
pub struct Skipped {
    pub path: PathBuf,
    pub reason: SkipReason,
}

#[derive(Debug, Default)]
pub struct ScanReport {
    pub files: Vec<FileRecord>,
    pub skipped: Vec<Skipped>,
}

/// Extra directories to leave alone, on top of the ones derived from the
/// profile. The caller supplies the global quarantine and recycle directories
/// here, since the profile does not know about them.
#[derive(Debug, Default, Clone)]
pub struct ScanOptions {
    pub extra_excluded_roots: Vec<PathBuf>,
}

/// Walks a profile's source directory.
///
/// # Not re-scanning its own output
///
/// The tool must never treat a file it filed yesterday as unorganized input
/// today. Which directories to skip depends on how the profile is set up:
///
/// * **Routed elsewhere** (`destination_root` != `path`): skip
///   `destination_root` outright, which matters only when it happens to sit
///   inside `path`.
/// * **In place** (`destination_root` == `path`, the default): skipping the
///   destination root would skip the entire scan, so instead the *category
///   subdirectories* under it are skipped.
///
/// One gap remains: with `allow_dynamic_categories = true` and in-place
/// organization, a category the model invented on an earlier run is not in
/// `profile.categories` and so is not skipped. Closing that needs the journal
/// to report which directories this profile has created, which arrives with the
/// journal itself. Until then, in-place profiles with dynamic categories should
/// keep `include_subdirs = false` (the default), which sidesteps it entirely.
pub fn scan(profile: &Profile, options: &ScanOptions) -> Result<ScanReport, ScanError> {
    let excludes = build_globset(profile)?;
    let managed = managed_roots(profile, options);

    if !profile.path.is_dir() {
        return Err(ScanError::Unreadable {
            path: profile.path.clone(),
            source: std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "source directory does not exist or is not a directory",
            ),
        });
    }

    let mut report = ScanReport::default();
    let now = SystemTime::now();

    let walker = walkdir::WalkDir::new(&profile.path)
        .min_depth(1)
        .max_depth(if profile.include_subdirs { usize::MAX } else { 1 })
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| !(e.file_type().is_dir() && is_under_any(e.path(), &managed)));

    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                let path = e.path().unwrap_or(&profile.path).to_path_buf();
                report
                    .skipped
                    .push(Skipped { path, reason: SkipReason::Unreadable(e.to_string()) });
                continue;
            }
        };

        let path = entry.path();
        if entry.file_type().is_dir() {
            continue;
        }
        if entry.file_type().is_symlink() {
            report.skipped.push(Skipped { path: path.to_path_buf(), reason: SkipReason::Symlink });
            continue;
        }

        // Defence in depth: `filter_entry` already prunes managed directories,
        // but a managed root could name a file rather than a directory.
        if is_under_any(path, &managed) {
            report
                .skipped
                .push(Skipped { path: path.to_path_buf(), reason: SkipReason::ManagedOutput });
            continue;
        }

        let relative = path.strip_prefix(&profile.path).unwrap_or(path).to_path_buf();
        if excludes.is_match(&relative) || excludes.is_match(Path::new(entry.file_name())) {
            report
                .skipped
                .push(Skipped { path: path.to_path_buf(), reason: SkipReason::ExcludedByPattern });
            continue;
        }

        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(e) => {
                report.skipped.push(Skipped {
                    path: path.to_path_buf(),
                    reason: SkipReason::Unreadable(e.to_string()),
                });
                continue;
            }
        };
        let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);

        // A file still being written to is not a file we know how to classify.
        if !profile.stability_wait.is_zero() {
            let settled = now.duration_since(mtime).is_ok_and(|age| age >= profile.stability_wait);
            if !settled {
                report
                    .skipped
                    .push(Skipped { path: path.to_path_buf(), reason: SkipReason::StillSettling });
                continue;
            }
        }

        let head = read_head(path, &profile.metadata);
        report.files.push(FileRecord {
            id: FileId::for_path(path),
            path: path.to_path_buf(),
            relative,
            facts: FileFacts { size: meta.len(), mtime },
            extension: path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase),
            mime: profile
                .metadata
                .detect_mime
                .then(|| infer::get(&head).map(|t| t.mime_type().to_owned()))
                .flatten(),
            content_snippet: snippet(&head, profile.metadata.content_sniff_bytes),
        });
    }

    report.files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(report)
}

/// Directories this profile treats as its own output, which must never appear
/// as input. See [`scan`] for why the in-place case is handled differently.
fn managed_roots(profile: &Profile, options: &ScanOptions) -> Vec<PathBuf> {
    let mut roots = options.extra_excluded_roots.clone();
    if profile.is_in_place() {
        roots.extend(profile.categories.iter().map(|c| profile.destination_root.join(c)));
    } else {
        roots.push(profile.destination_root.clone());
    }
    roots
}

fn is_under_any(path: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|r| path.starts_with(r))
}

fn build_globset(profile: &Profile) -> Result<GlobSet, ScanError> {
    let mut builder = GlobSetBuilder::new();
    for pattern in &profile.exclude_patterns {
        let glob = Glob::new(pattern).map_err(|source| ScanError::BadPattern {
            profile: profile.name.clone(),
            pattern: pattern.clone(),
            source,
        })?;
        builder.add(glob);
    }
    builder.build().map_err(|source| ScanError::BadPattern {
        profile: profile.name.clone(),
        pattern: profile.exclude_patterns.join(", "),
        source,
    })
}

/// Reads enough of the file's head to satisfy both MIME sniffing and the
/// configured content snippet, in a single open.
fn read_head(path: &Path, meta: &bower_config::Metadata) -> Vec<u8> {
    let want = SNIFF_WINDOW.max(meta.content_sniff_bytes);
    if !meta.detect_mime && meta.content_sniff_bytes == 0 {
        return Vec::new();
    }
    let Ok(mut file) = File::open(path) else {
        return Vec::new();
    };
    let mut buf = vec![0u8; want];
    match file.read(&mut buf) {
        Ok(n) => {
            buf.truncate(n);
            buf
        }
        Err(_) => Vec::new(),
    }
}

fn snippet(head: &[u8], want: usize) -> Option<String> {
    if want == 0 || head.is_empty() {
        return None;
    }
    let slice = head.get(..want.min(head.len()))?;
    let text = String::from_utf8_lossy(slice).trim().to_owned();
    (!text.is_empty()).then_some(text)
}
