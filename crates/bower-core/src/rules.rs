//! The rule-based fast path.
//!
//! A rule is a decision the profile's owner made in advance: files like this go
//! there. A file that matches one never reaches a model, which is both cheaper
//! and, for the cases a person can state outright (`*.nzb`, `*.torrent`),
//! simply more reliable than asking.
//!
//! A rule does not get a side door around the safety layers. It produces the
//! same [`Proposal`] a model would and goes through the same policy engine:
//! category resolution, remembered rejections, filename rendering, containment,
//! collision handling. The only things it skips are the model and the
//! confidence gate, and the second of those only because its confidence is
//! `1.0` by construction -- there is no estimate to be unsure about.
//!
//! What a rule does **not** do is rename. It supplies no filename tokens, so on
//! a profile with `rename` enabled a template that needs a model-supplied token
//! sends the file to review rather than filing it under a name with a hole in
//! it. That is the existing behaviour for a model that omits a token, applied
//! consistently.
//!
//! There are no built-in rules. See [`bower_config::Rule`].

use bower_config::Rule;
use globset::{GlobBuilder, GlobMatcher};
use std::collections::BTreeMap;

use crate::model::{FileRecord, Proposal, ProposalOutcome, RawProposal};

#[derive(Debug, thiserror::Error)]
pub enum RuleError {
    /// Config validation rejects these first; this is for a profile built by
    /// hand, where nothing has validated it.
    #[error("rule `{rule}` has an invalid name_glob `{glob}`: {source}")]
    BadGlob {
        rule: String,
        glob: String,
        #[source]
        source: globset::Error,
    },
}

#[derive(Debug)]
struct Compiled<'a> {
    rule: &'a Rule,
    glob: Option<GlobMatcher>,
}

/// A profile's rules, compiled once per run so a glob is not rebuilt per file.
#[derive(Debug)]
pub struct RuleSet<'a> {
    rules: Vec<Compiled<'a>>,
}

impl<'a> RuleSet<'a> {
    pub fn compile(rules: &'a [Rule]) -> Result<Self, RuleError> {
        let mut compiled = Vec::with_capacity(rules.len());
        for rule in rules {
            let glob = match &rule.name_glob {
                None => None,
                Some(g) => Some(
                    GlobBuilder::new(g)
                        .case_insensitive(true)
                        .build()
                        .map_err(|source| RuleError::BadGlob {
                            rule: rule.name.clone(),
                            glob: g.clone(),
                            source,
                        })?
                        .compile_matcher(),
                ),
            };
            compiled.push(Compiled { rule, glob });
        }
        Ok(Self { rules: compiled })
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// The first rule that matches, in the order the rules were written.
    #[must_use]
    pub fn first_match(&self, file: &FileRecord) -> Option<&'a Rule> {
        self.rules.iter().find(|c| matches(c, file)).map(|c| c.rule)
    }
}

/// Every criterion the rule sets must hold. A rule with none set cannot exist
/// (config validation refuses it), but would match nothing here rather than
/// everything, so a hand-built one fails safe.
fn matches(c: &Compiled<'_>, file: &FileRecord) -> bool {
    let rule = c.rule;
    if rule.extensions.is_empty() && c.glob.is_none() && rule.mime.is_none() {
        return false;
    }

    if !rule.extensions.is_empty() {
        let Some(ext) = file.extension.as_deref() else { return false };
        if !rule.extensions.iter().any(|e| e == ext) {
            return false;
        }
    }

    if let Some(glob) = &c.glob
        && !glob.is_match(file.file_name())
    {
        return false;
    }

    if let Some(want) = &rule.mime {
        let Some(have) = file.mime.as_deref() else { return false };
        let ok = match want.strip_suffix("/*") {
            Some(family) => {
                have.split_once('/').is_some_and(|(t, _)| t.eq_ignore_ascii_case(family))
            }
            None => have.eq_ignore_ascii_case(want),
        };
        if !ok {
            return false;
        }
    }

    true
}

/// The proposal a matching rule stands in for.
#[must_use]
pub fn proposal(rule: &Rule, file: &FileRecord) -> ProposalOutcome {
    ProposalOutcome::Ok(Proposal::Categorize(RawProposal {
        file_id: file.id.clone(),
        category: rule.category.clone(),
        is_new_category: false,
        name_tokens: BTreeMap::new(),
        confidence: 1.0,
        reasoning: format!("matched rule `{}`", rule.name),
    }))
}
