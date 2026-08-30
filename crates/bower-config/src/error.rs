use std::fmt;
use std::path::PathBuf;

/// A single problem found while validating a config file.
///
/// Validation collects every problem rather than stopping at the first, because
/// a config file that governs filesystem mutation is one a user wants to fix in
/// a single pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    /// Dotted path to the offending key, e.g. `profiles[downloads].confidence_threshold`.
    pub location: String,
    pub message: String,
}

impl Problem {
    pub(crate) fn new(location: impl Into<String>, message: impl Into<String>) -> Self {
        Self { location: location.into(), message: message.into() }
    }
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.location, self.message)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("could not read config file {path}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("could not parse config file {path}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    #[error("config is invalid ({} problem(s))", .problems.len())]
    Invalid { problems: Vec<Problem> },
}

impl ConfigError {
    /// Renders every problem as an indented list, for CLI output.
    #[must_use]
    pub fn problem_report(&self) -> Option<String> {
        match self {
            Self::Invalid { problems } => {
                Some(problems.iter().map(|p| format!("  - {p}")).collect::<Vec<_>>().join("\n"))
            }
            _ => None,
        }
    }
}
