use std::path::{Path, PathBuf};

/// Why a destination path could not be constructed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PathError {
    #[error("destination root `{0}` is not absolute")]
    RootNotAbsolute(PathBuf),
    #[error("category `{0}` is not usable as a single directory name")]
    UnsafeCategory(String),
    #[error("filename `{0}` is not usable as a single file name")]
    UnsafeFilename(String),
    #[error("constructed path `{path}` escaped destination root `{root}`")]
    Escaped { path: PathBuf, root: PathBuf },
}

/// A destination path proven to lie directly under a profile's
/// `destination_root`.
///
/// The inner path is private and [`DestPath::under`] is the only way to build
/// one, so possessing a `DestPath` *is* the proof that its components were
/// validated. The executor accepts nothing else, which is what keeps the
/// "no writes outside `destination_root`" guarantee from depending on any
/// caller remembering to check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DestPath {
    path: PathBuf,
    root: PathBuf,
    category: String,
    filename: String,
}

impl DestPath {
    /// Builds `root/category/filename`, rejecting anything that is not exactly
    /// one safe path component per level.
    pub fn under(root: &Path, category: &str, filename: &str) -> Result<Self, PathError> {
        if !root.is_absolute() {
            return Err(PathError::RootNotAbsolute(root.to_path_buf()));
        }
        if !bower_config::is_safe_component(category) {
            return Err(PathError::UnsafeCategory(category.to_owned()));
        }
        if !bower_config::is_safe_filename(filename) {
            return Err(PathError::UnsafeFilename(filename.to_owned()));
        }

        let path = root.join(category).join(filename);

        // Guaranteed by the component checks above -- both reject `..` and any
        // separator -- but re-asserted here so a future loosening of
        // `is_safe_component` fails closed rather than silently widening what
        // the executor will write to.
        if !path.starts_with(root) || path.components().any(|c| c.as_os_str() == "..") {
            return Err(PathError::Escaped { path, root: root.to_path_buf() });
        }

        Ok(Self {
            path,
            root: root.to_path_buf(),
            category: category.to_owned(),
            filename: filename.to_owned(),
        })
    }

    /// Rebuilds this path with `-<n>` inserted before the extension, for the
    /// `on_conflict = "suffix"` policy.
    pub fn with_suffix(&self, n: u32) -> Result<Self, PathError> {
        let (stem, ext) = split_extension(&self.filename);
        let suffixed = match ext {
            Some(ext) => format!("{stem}-{n}.{ext}"),
            None => format!("{stem}-{n}"),
        };
        Self::under(&self.root, &self.category, &suffixed)
    }

    #[must_use]
    pub fn as_path(&self) -> &Path {
        &self.path
    }

    /// The directory the executor must ensure exists before moving into it.
    #[must_use]
    pub fn parent_dir(&self) -> PathBuf {
        self.root.join(&self.category)
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn category(&self) -> &str {
        &self.category
    }

    #[must_use]
    pub fn filename(&self) -> &str {
        &self.filename
    }
}

/// Splits a filename into stem and extension. Unlike [`Path::extension`], a
/// leading dot never starts an extension (`.bashrc` is all stem), which matches
/// how users read these names.
#[must_use]
pub fn split_extension(filename: &str) -> (&str, Option<&str>) {
    match filename.rfind('.') {
        Some(i) if i > 0 && i + 1 < filename.len() => {
            let (stem, rest) = filename.split_at(i);
            (stem, rest.strip_prefix('.'))
        }
        _ => (filename, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf {
        PathBuf::from("/data/organized")
    }

    #[test]
    fn builds_a_path_under_the_root() {
        let d = DestPath::under(&root(), "Invoices", "acme.pdf").unwrap();
        assert_eq!(d.as_path(), Path::new("/data/organized/Invoices/acme.pdf"));
        assert_eq!(d.parent_dir(), Path::new("/data/organized/Invoices"));
    }

    #[test]
    fn rejects_traversal_in_either_component() {
        for (cat, name) in [
            ("..", "x.pdf"),
            ("../etc", "x.pdf"),
            ("Invoices", ".."),
            ("Invoices", "../../etc/passwd"),
            ("Invoices/nested", "x.pdf"),
            ("Invoices", "a/b.pdf"),
        ] {
            assert!(
                DestPath::under(&root(), cat, name).is_err(),
                "expected {cat}/{name} to be rejected"
            );
        }
    }

    #[test]
    fn rejects_absolute_looking_components_and_relative_roots() {
        assert!(DestPath::under(&root(), "/etc", "passwd").is_err());
        assert!(DestPath::under(Path::new("relative/root"), "Invoices", "a.pdf").is_err());
    }

    #[test]
    fn suffix_preserves_the_extension() {
        let d = DestPath::under(&root(), "Invoices", "acme.pdf").unwrap();
        assert_eq!(d.with_suffix(2).unwrap().filename(), "acme-2.pdf");

        let no_ext = DestPath::under(&root(), "Invoices", "receipt").unwrap();
        assert_eq!(no_ext.with_suffix(1).unwrap().filename(), "receipt-1");

        let dotted = DestPath::under(&root(), "Archives", "backup.tar.gz").unwrap();
        assert_eq!(dotted.with_suffix(3).unwrap().filename(), "backup.tar-3.gz");
    }

    #[test]
    fn split_extension_ignores_leading_dot() {
        assert_eq!(split_extension(".bashrc"), (".bashrc", None));
        assert_eq!(split_extension("a.pdf"), ("a", Some("pdf")));
        assert_eq!(split_extension("trailing."), ("trailing.", None));
    }
}
