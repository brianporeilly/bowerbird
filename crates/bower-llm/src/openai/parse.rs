//! Per-item validation of a model's reply.
//!
//! ADR-0001 §3 requires that one malformed entry in a batch does not sink the
//! rest, so nothing here deserializes the whole array at once. Each entry is
//! resolved independently and a bad one becomes
//! [`ProposalOutcome::Malformed`] for that file alone.
//!
//! Pure: no I/O, no network. The adapter does the talking.

use bower_core::model::{FileId, Proposal, ProposalOutcome};
use serde_json::{Map, Value};
use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};

/// A failure that affects the entire reply rather than one entry, and is
/// therefore worth one reformat retry.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReplyError {
    #[error("no JSON object or array found in the reply")]
    NotJson,
    #[error("reply JSON is not an array of proposals and has no `proposals` array")]
    NoProposals,
}

#[derive(Debug, Default, Clone)]
pub struct ParsedBatch {
    pub outcomes: BTreeMap<FileId, ProposalOutcome>,
    /// Entries naming a file that was not in the request. Counted rather than
    /// kept: an id we did not send is either a hallucination or an attempt to
    /// reach a file this batch is not about, and neither is worth acting on.
    pub unattributable: usize,
}

impl ParsedBatch {
    /// Whether anything about this reply warrants asking the model again.
    #[must_use]
    pub fn needs_reformat(&self, expected: &BTreeSet<FileId>) -> bool {
        expected.iter().any(|id| !matches!(self.outcomes.get(id), Some(ProposalOutcome::Ok(_))))
    }

    /// Ids the model failed to answer usefully, for the retry complaint.
    #[must_use]
    pub fn unresolved(&self, expected: &BTreeSet<FileId>) -> Vec<(FileId, String)> {
        expected
            .iter()
            .filter_map(|id| match self.outcomes.get(id) {
                Some(ProposalOutcome::Ok(_)) => None,
                Some(ProposalOutcome::Malformed { detail }) => Some((id.clone(), detail.clone())),
                _ => Some((id.clone(), "no entry for this file_id".to_owned())),
            })
            .collect()
    }
}

/// Parses one reply into per-file outcomes.
///
/// `expected` is the set of ids actually sent. An entry naming anything else is
/// discarded rather than trusted.
pub fn parse_reply(content: &str, expected: &BTreeSet<FileId>) -> Result<ParsedBatch, ReplyError> {
    let json = extract_json(content).ok_or(ReplyError::NotJson)?;
    let value: Value = serde_json::from_str(json).map_err(|_| ReplyError::NotJson)?;
    let entries = proposals_array(value).ok_or(ReplyError::NoProposals)?;

    let mut batch = ParsedBatch::default();
    for entry in entries {
        let Some(id) = attribute(&entry, expected) else {
            batch.unattributable += 1;
            continue;
        };

        let outcome = match serde_json::from_value::<Proposal>(default_action(entry)) {
            Ok(proposal) => ProposalOutcome::Ok(proposal),
            Err(e) => ProposalOutcome::Malformed { detail: e.to_string() },
        };

        match batch.outcomes.entry(id) {
            // A second proposal for the same file is a contradiction, and the
            // pipeline may only ever lower trust, so both are discarded in
            // favour of a human looking at it.
            Entry::Occupied(mut slot) => {
                slot.insert(ProposalOutcome::Malformed {
                    detail: "the model returned more than one proposal for this file".to_owned(),
                });
            }
            Entry::Vacant(slot) => {
                slot.insert(outcome);
            }
        }
    }
    Ok(batch)
}

/// Matches an entry's `file_id` against the ids actually requested.
fn attribute(entry: &Value, expected: &BTreeSet<FileId>) -> Option<FileId> {
    let id = entry.get("file_id")?.as_str()?;
    expected.iter().find(|e| e.as_str() == id).cloned()
}

/// Supplies `action: "categorize"` when the model omitted the tag.
///
/// Models routinely emit the categorization shape without the discriminant.
/// Defaulting is safe in exactly one direction: the destructive variant must be
/// named explicitly, so an untagged entry can never become a deletion
/// suggestion. It can only ever become the harmless one.
fn default_action(entry: Value) -> Value {
    match entry {
        Value::Object(mut map) if !map.contains_key("action") => {
            map.insert("action".to_owned(), Value::String("categorize".to_owned()));
            Value::Object(map)
        }
        other => other,
    }
}

/// Finds the proposals, tolerating the handful of shapes models actually emit.
fn proposals_array(value: Value) -> Option<Vec<Value>> {
    match value {
        Value::Array(items) => Some(items),
        Value::Object(map) => named_array(&map),
        _ => None,
    }
}

fn named_array(map: &Map<String, Value>) -> Option<Vec<Value>> {
    ["proposals", "results", "files", "classifications"]
        .iter()
        .find_map(|k| map.get(*k))
        .and_then(Value::as_array)
        .cloned()
}

/// Pulls the JSON out of a reply that may be fenced or prefaced with prose.
///
/// Needed for `structured_output = "prompt"`, where nothing constrains the
/// model to reply with bare JSON. Harmless in the structured modes.
fn extract_json(reply: &str) -> Option<&str> {
    let text = reply.trim();

    if let Some(after) = text.strip_prefix("```") {
        // Drop an optional language tag on the fence line.
        let body = after.split_once('\n').map_or(after, |(_, rest)| rest);
        if let Some(end) = body.rfind("```") {
            let inner = body.get(..end)?.trim();
            if !inner.is_empty() {
                return Some(inner);
            }
        }
    }

    if text.starts_with('{') || text.starts_with('[') {
        return Some(text);
    }

    // Prose around the payload: take the outermost bracketed span. Both
    // delimiters are ASCII, so these byte offsets are char boundaries.
    let start = text.find(['{', '['])?;
    let end = text.rfind(['}', ']'])?;
    (end > start).then(|| text.get(start..=end)).flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn ids(names: &[&str]) -> BTreeSet<FileId> {
        names.iter().map(|n| FileId::for_path(Path::new(n))).collect()
    }

    fn id(name: &str) -> FileId {
        FileId::for_path(Path::new(name))
    }

    fn entry(file_id: &FileId, category: &str) -> String {
        format!(
            r#"{{"file_id":"{file_id}","action":"categorize","category":"{category}",
               "confidence":0.9,"reasoning":"because"}}"#
        )
    }

    #[test]
    fn one_bad_entry_does_not_sink_the_batch() {
        let expected = ids(&["a", "b", "c"]);
        let (a, b, c) = (id("a"), id("b"), id("c"));
        let reply = format!(
            r#"{{"proposals":[{}, {{"file_id":"{b}","action":"categorize"}}, {}]}}"#,
            entry(&a, "Documents"),
            entry(&c, "Images"),
        );

        let parsed = parse_reply(&reply, &expected).unwrap();

        assert!(matches!(parsed.outcomes.get(&a), Some(ProposalOutcome::Ok(_))));
        assert!(matches!(parsed.outcomes.get(&c), Some(ProposalOutcome::Ok(_))));
        assert!(
            matches!(parsed.outcomes.get(&b), Some(ProposalOutcome::Malformed { .. })),
            "the bad entry is malformed on its own, not fatal to its neighbours"
        );
    }

    #[test]
    fn an_id_that_was_not_requested_is_discarded_not_trusted() {
        let expected = ids(&["a"]);
        let reply = format!(
            r#"{{"proposals":[{}, {}]}}"#,
            entry(&id("a"), "Documents"),
            entry(&id("somebody-elses-file"), "Documents"),
        );

        let parsed = parse_reply(&reply, &expected).unwrap();
        assert_eq!(parsed.outcomes.len(), 1);
        assert_eq!(parsed.unattributable, 1);
    }

    #[test]
    fn a_file_the_model_ignored_is_simply_absent() {
        let expected = ids(&["a", "b"]);
        let reply = format!(r#"{{"proposals":[{}]}}"#, entry(&id("a"), "Documents"));

        let parsed = parse_reply(&reply, &expected).unwrap();
        assert!(!parsed.outcomes.contains_key(&id("b")));
        assert!(parsed.needs_reformat(&expected));
    }

    #[test]
    fn contradictory_duplicates_are_downgraded_to_malformed() {
        let expected = ids(&["a"]);
        let a = id("a");
        let reply =
            format!(r#"{{"proposals":[{}, {}]}}"#, entry(&a, "Documents"), entry(&a, "Images"));

        let parsed = parse_reply(&reply, &expected).unwrap();
        assert!(
            matches!(parsed.outcomes.get(&a), Some(ProposalOutcome::Malformed { .. })),
            "two answers for one file is not an answer"
        );
    }

    #[test]
    fn a_missing_action_tag_becomes_categorize_never_a_deletion() {
        let expected = ids(&["a"]);
        let a = id("a");
        let reply = format!(
            r#"[{{"file_id":"{a}","category":"Documents","confidence":0.9,"reasoning":"r"}}]"#
        );

        match parse_reply(&reply, &expected).unwrap().outcomes.get(&a) {
            Some(ProposalOutcome::Ok(Proposal::Categorize(_))) => {}
            other => panic!("expected a categorization, got {other:?}"),
        }
    }

    #[test]
    fn a_deletion_must_be_named_explicitly() {
        let expected = ids(&["a"]);
        let a = id("a");
        let reply = format!(
            r#"[{{"file_id":"{a}","action":"suggest_delete","reason":"dup","confidence":0.9}}]"#
        );

        match parse_reply(&reply, &expected).unwrap().outcomes.get(&a) {
            Some(ProposalOutcome::Ok(Proposal::SuggestDelete(_))) => {}
            other => panic!("expected a deletion suggestion, got {other:?}"),
        }
    }

    #[test]
    fn json_is_recovered_from_fences_and_prose() {
        let expected = ids(&["a"]);
        let a = id("a");
        let payload = format!(r#"{{"proposals":[{}]}}"#, entry(&a, "Documents"));

        for reply in [
            format!("```json\n{payload}\n```"),
            format!("```\n{payload}\n```"),
            format!("Sure! Here are the classifications:\n{payload}\nHope that helps."),
            payload.clone(),
        ] {
            let parsed = parse_reply(&reply, &expected).unwrap();
            assert!(
                matches!(parsed.outcomes.get(&a), Some(ProposalOutcome::Ok(_))),
                "failed to recover JSON from: {reply}"
            );
        }
    }

    #[test]
    fn a_bare_array_is_accepted() {
        let expected = ids(&["a"]);
        let reply = format!("[{}]", entry(&id("a"), "Documents"));
        assert!(parse_reply(&reply, &expected).is_ok());
    }

    #[test]
    fn a_reply_with_no_json_is_a_whole_batch_failure() {
        let expected = ids(&["a"]);
        assert_eq!(
            parse_reply("I'm sorry, I can't help with that.", &expected).unwrap_err(),
            ReplyError::NotJson
        );
        assert_eq!(parse_reply("", &expected).unwrap_err(), ReplyError::NotJson);
    }

    #[test]
    fn json_without_proposals_is_a_whole_batch_failure() {
        let expected = ids(&["a"]);
        assert_eq!(
            parse_reply(r#"{"status":"ok"}"#, &expected).unwrap_err(),
            ReplyError::NoProposals
        );
    }

    #[test]
    fn a_fully_answered_batch_needs_no_retry() {
        let expected = ids(&["a", "b"]);
        let reply = format!(
            r#"{{"proposals":[{}, {}]}}"#,
            entry(&id("a"), "Documents"),
            entry(&id("b"), "Images"),
        );
        let parsed = parse_reply(&reply, &expected).unwrap();
        assert!(!parsed.needs_reformat(&expected));
        assert!(parsed.unresolved(&expected).is_empty());
    }
}
