//! Turning untrusted model output into strings that are safe as path
//! components.
//!
//! Two different treatments, deliberately:
//!
//! * **Tokens** are freeform prose from the model destined for the middle of a
//!   filename. Mangling them into something safe is the right move.
//! * **Categories** are structural -- they name directories the user declared
//!   and that the tool will keep reusing. Silently rewriting one would let
//!   `Invoices/` and `Invoices!/` both exist, so an unsafe category is
//!   rejected for review instead of repaired.

/// Longest a single rendered token may be, so that a filename built from
/// several of them stays well clear of the 255-byte component limit.
const MAX_TOKEN_LEN: usize = 64;

/// Longest rendered filename, leaving room for a `-NN` conflict suffix.
pub(crate) const MAX_FILENAME_LEN: usize = 200;

/// Rewrites one model-supplied token into a safe filename fragment, or returns
/// `None` if nothing usable survives.
pub(crate) fn token(raw: &str) -> Option<String> {
    let mut out = String::with_capacity(raw.len());
    let mut pending_sep = false;

    for ch in raw.chars() {
        let keep = ch.is_alphanumeric() || matches!(ch, '_' | '.' | '(' | ')' | '&' | '+' | '\'');
        if keep && !ch.is_control() {
            if pending_sep && !out.is_empty() {
                out.push('-');
            }
            pending_sep = false;
            out.push(ch);
        } else {
            // Whitespace, separators, reserved characters and anything else all
            // collapse to a single '-'.
            pending_sep = true;
        }
    }

    let trimmed = out.trim_matches(|c: char| c == '-' || c == '.' || c == '_');
    let truncated = truncate_chars(trimmed, MAX_TOKEN_LEN);
    (!truncated.is_empty()).then(|| truncated.to_owned())
}

/// Normalizes whitespace in a model-supplied category without otherwise
/// altering it. Returns `None` when the result would not be a usable directory
/// name -- the caller routes that to manual review rather than guessing.
pub(crate) fn category(raw: &str) -> Option<String> {
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    bower_config::is_safe_component(&collapsed).then_some(collapsed)
}

/// Truncates to at most `max` characters, never splitting one.
fn truncate_chars(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((i, _)) => s.get(..i).unwrap_or(s),
        None => s,
    }
}

/// Shortens a rendered filename to [`MAX_FILENAME_LEN`] by trimming the stem,
/// preserving the extension.
pub(crate) fn clamp_filename(name: &str) -> String {
    if name.chars().count() <= MAX_FILENAME_LEN {
        return name.to_owned();
    }
    let (stem, ext) = crate::model::split_extension(name);
    match ext {
        Some(ext) => {
            let room = MAX_FILENAME_LEN.saturating_sub(ext.chars().count() + 1);
            let cut = truncate_chars(stem, room).trim_end_matches(['-', '.', '_']);
            format!("{cut}.{ext}")
        }
        None => truncate_chars(name, MAX_FILENAME_LEN).trim_end_matches(['-', '.', '_']).to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_collapse_unsafe_runs_to_single_dashes() {
        assert_eq!(token("Acme Corp").as_deref(), Some("Acme-Corp"));
        assert_eq!(token("  Acme // Corp  ").as_deref(), Some("Acme-Corp"));
        assert_eq!(token("a///b").as_deref(), Some("a-b"));
        assert_eq!(token("2024-03-15").as_deref(), Some("2024-03-15"));
    }

    #[test]
    fn tokens_that_sanitize_to_nothing_are_none() {
        assert_eq!(token(""), None);
        assert_eq!(token("   "), None);
        assert_eq!(token("///"), None);
        assert_eq!(token("..."), None);
    }

    #[test]
    fn tokens_cannot_smuggle_traversal_or_separators() {
        for hostile in ["../../etc/passwd", "..", "/etc/passwd", "a\0b", "C:\\Windows"] {
            let out = token(hostile).unwrap_or_default();
            assert!(!out.contains('/'), "{hostile} -> {out}");
            assert!(!out.contains('\\'), "{hostile} -> {out}");
            assert!(!out.contains('\0'), "{hostile} -> {out}");
            assert_ne!(out, "..", "{hostile}");
        }
    }

    #[test]
    fn categories_are_normalized_not_repaired() {
        assert_eq!(category("  Tax   Records ").as_deref(), Some("Tax Records"));
        assert_eq!(category("Invoices").as_deref(), Some("Invoices"));
        // Unsafe categories are rejected outright rather than mangled.
        assert_eq!(category("../etc"), None);
        assert_eq!(category("a/b"), None);
        assert_eq!(category(".hidden"), None);
        assert_eq!(category(""), None);
    }

    #[test]
    fn clamp_preserves_extension() {
        let long = format!("{}.pdf", "x".repeat(400));
        let out = clamp_filename(&long);
        assert!(out.chars().count() <= MAX_FILENAME_LEN);
        assert!(out.ends_with(".pdf"), "{out}");
    }
}
