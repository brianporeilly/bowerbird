//! Filename template rendering.
//!
//! **Provisional.** ADR-0001 lists template syntax as an open question -- token
//! format, escaping, and conditional/fallback tokens are all undecided. What is
//! implemented here is the smallest thing that satisfies the ADR's own example,
//! `"{date}-{doc_type}-{vendor}{ext}"`, so that the rest of the pipeline can be
//! built and tested:
//!
//! * `{ext}` and `{date}` are *engine tokens*: filled from the source file
//!   itself, never from model output. `{ext}` is the extension including the
//!   dot, or nothing when the file has no extension; `{date}` is the file's
//!   mtime as `YYYY-MM-DD`.
//! * Every other token, such as `{name}`, is replaced by the sanitized value
//!   the model supplied under that name.
//! * `{{` and `}}` are literal braces.
//! * A token the model did not supply is an error, not an empty string. There
//!   is deliberately no fallback syntax yet; a file whose template cannot be
//!   filled goes to manual review rather than being filed under a name with a
//!   hole in it.

use std::collections::BTreeMap;

use super::sanitize;

/// Token bound to the source file's extension rather than to model output.
const EXT_TOKEN: &str = "ext";

/// Token bound to the source file's mtime rather than to model output.
///
/// A date is a fact the scanner already holds, and the model is never told it
/// (see `context::FileContext`), so asking for one would be asking it to
/// invent a value that lands in a filename looking exactly as authoritative as
/// a true one. The engine fills it instead, and `template_tokens` does not ask
/// the model for it.
const DATE_TOKEN: &str = "date";

/// Whether `name` is filled by the engine from the file rather than by the
/// model. Engine tokens are excluded from what the model is asked to supply,
/// and a model that volunteers one anyway is ignored.
fn is_engine_token(name: &str) -> bool {
    name == EXT_TOKEN || name == DATE_TOKEN
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TemplateError {
    #[error("unclosed `{{` in template")]
    UnclosedBrace,
    #[error("stray `}}` in template; write `}}}}` for a literal brace")]
    StrayBrace,
    #[error("empty `{{}}` in template")]
    EmptyToken,
    #[error("template has no tokens, so every file would render the same name")]
    NoTokens,
    #[error("model did not supply a value for token `{0}`")]
    MissingToken(String),
    #[error("rendered filename `{0}` is not a usable file name")]
    UnusableResult(String),
}

/// Checks a template for problems that do not depend on any particular file, so
/// a broken template is reported once at startup instead of once per file.
pub fn validate_template(template: &str) -> Result<(), TemplateError> {
    let mut token_count = 0usize;
    for piece in parse(template)? {
        // `{date}` counts: it varies per file, so a template of `{date}{ext}`
        // is not the constant name this check exists to reject.
        if let Piece::Token(name) = piece
            && name != EXT_TOKEN
        {
            token_count += 1;
        }
    }
    if token_count == 0 {
        return Err(TemplateError::NoTokens);
    }
    Ok(())
}

/// The token names a template requires, in template order and deduplicated.
///
/// Engine tokens are excluded: they are bound to the source file, not to
/// anything the model supplies. The context builder uses this to tell the model
/// which tokens are actually wanted, rather than leaving it to guess.
pub fn template_tokens(template: &str) -> Result<Vec<String>, TemplateError> {
    let mut names: Vec<String> = Vec::new();
    for piece in parse(template)? {
        if let Piece::Token(name) = piece
            && !is_engine_token(name)
            && !names.iter().any(|n| n == name)
        {
            names.push(name.to_owned());
        }
    }
    Ok(names)
}

/// Renders `template` against already-sanitized `tokens`.
///
/// `extension` and `date` are the engine's, taken from the source file. They
/// take precedence over anything the model supplied under the same name, so a
/// model cannot put its own date into a filename by proposing a `date` token.
pub(crate) fn render(
    template: &str,
    tokens: &BTreeMap<String, String>,
    extension: Option<&str>,
    date: &str,
) -> Result<String, TemplateError> {
    let mut out = String::new();
    for piece in parse(template)? {
        match piece {
            Piece::Literal(s) => out.push_str(s),
            Piece::Token(EXT_TOKEN) => {
                if let Some(ext) = extension {
                    out.push('.');
                    out.push_str(ext);
                }
            }
            Piece::Token(DATE_TOKEN) => out.push_str(date),
            Piece::Token(name) => {
                let value =
                    tokens.get(name).ok_or_else(|| TemplateError::MissingToken(name.to_owned()))?;
                out.push_str(value);
            }
        }
    }

    let clamped = sanitize::clamp_filename(&out);
    if bower_config::is_safe_filename(&clamped) {
        Ok(clamped)
    } else {
        Err(TemplateError::UnusableResult(clamped))
    }
}

enum Piece<'a> {
    Literal(&'a str),
    Token(&'a str),
}

fn parse(template: &str) -> Result<Vec<Piece<'_>>, TemplateError> {
    let mut pieces = Vec::new();
    let mut rest = template;

    while !rest.is_empty() {
        let Some(open) = rest.find(['{', '}']) else {
            pieces.push(Piece::Literal(rest));
            break;
        };
        let (before, tail) = rest.split_at(open);
        if !before.is_empty() {
            pieces.push(Piece::Literal(before));
        }

        if let Some(after) = tail.strip_prefix("{{") {
            pieces.push(Piece::Literal("{"));
            rest = after;
            continue;
        }
        if let Some(after) = tail.strip_prefix("}}") {
            pieces.push(Piece::Literal("}"));
            rest = after;
            continue;
        }
        if tail.starts_with('}') {
            return Err(TemplateError::StrayBrace);
        }

        let body = tail.get(1..).unwrap_or_default();
        let close = body.find('}').ok_or(TemplateError::UnclosedBrace)?;
        let name = body.get(..close).unwrap_or_default().trim();
        if name.is_empty() {
            return Err(TemplateError::EmptyToken);
        }
        pieces.push(Piece::Token(name));
        rest = body.get(close + 1..).unwrap_or_default();
    }

    Ok(pieces)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()
    }

    const DATE: &str = "2024-03-15";

    #[test]
    fn renders_the_adr_example() {
        let t = tokens(&[("doc_type", "invoice"), ("vendor", "Acme")]);
        let out = render("{date}-{doc_type}-{vendor}{ext}", &t, Some("pdf"), DATE).unwrap();
        assert_eq!(out, "2024-03-15-invoice-Acme.pdf");
    }

    /// The engine's date wins. A model that proposes a `date` token -- whether
    /// it read one out of the file or invented it -- cannot put it in the name.
    #[test]
    fn a_model_supplied_date_is_ignored() {
        let t = tokens(&[("date", "1999-12-31"), ("vendor", "Acme")]);
        let out = render("{date}-{vendor}", &t, None, DATE).unwrap();
        assert_eq!(out, "2024-03-15-Acme");
    }

    #[test]
    fn the_date_token_needs_nothing_from_the_model() {
        let out = render("{date}{ext}", &tokens(&[]), Some("pdf"), DATE).unwrap();
        assert_eq!(out, "2024-03-15.pdf");
    }

    #[test]
    fn ext_token_vanishes_for_extensionless_files() {
        let t = tokens(&[("vendor", "Acme")]);
        assert_eq!(render("{vendor}{ext}", &t, None, DATE).unwrap(), "Acme");
    }

    #[test]
    fn a_missing_token_is_an_error_not_a_hole() {
        let t = tokens(&[]);
        let err = render("{date}-{vendor}", &t, None, DATE).unwrap_err();
        assert_eq!(err, TemplateError::MissingToken("vendor".to_owned()));
    }

    #[test]
    fn a_template_that_renders_an_unsafe_name_is_rejected() {
        let t = tokens(&[("a", "x"), ("b", "y")]);
        assert!(matches!(render("{a}/{b}", &t, None, DATE), Err(TemplateError::UnusableResult(_))));
    }

    #[test]
    fn braces_can_be_escaped() {
        let t = tokens(&[("v", "x")]);
        assert_eq!(render("{{{v}}}", &t, None, DATE).unwrap(), "{x}");
    }

    #[test]
    fn token_names_are_extracted_in_order_without_engine_tokens() {
        assert_eq!(
            template_tokens("{date}-{doc_type}-{vendor}{ext}").unwrap(),
            ["doc_type", "vendor"]
        );
        // Repeats collapse; `{ext}` never appears.
        assert_eq!(template_tokens("{a}-{a}{ext}").unwrap(), ["a"]);
        assert_eq!(template_tokens("{ext}"), Ok(vec![]));
        assert!(template_tokens("{unclosed").is_err());
    }

    #[test]
    fn validate_catches_static_problems() {
        assert_eq!(validate_template("{date}{ext}"), Ok(()));
        assert_eq!(validate_template("{unclosed"), Err(TemplateError::UnclosedBrace));
        assert_eq!(validate_template("stray}"), Err(TemplateError::StrayBrace));
        assert_eq!(validate_template("{}"), Err(TemplateError::EmptyToken));
        // Constant templates would collide on every single file.
        assert_eq!(validate_template("archive{ext}"), Err(TemplateError::NoTokens));
    }
}
