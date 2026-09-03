//! `bower journal` -- what was actually done.
//!
//! The review queue answers "what is waiting on me"; this answers "what
//! happened". They are different questions, and until now only the first had a
//! command: the journal was reachable from tests and nowhere else.
//!
//! Every operation is two rows -- an `intent` before the filesystem is touched
//! and a `committed` or `failed` after -- so an intent with no result is the
//! visible signature of a crash partway through. `--failed` surfaces exactly
//! those, which is the reason they are recorded that way.

use anyhow::Result;
use bower_core::state::{JournalRow, Store};
use std::collections::BTreeMap;

use crate::cli::JournalArgs;
use crate::triage::{age, now_secs, parse_duration};

/// Rows to pull before filtering. A filter that matched only within the
/// display limit would report "no failures" on a journal full of them, so the
/// filters read a wider window and `--limit` applies to what survives.
const FILTER_POOL: usize = 2_000;

pub(crate) fn show(store: &Store, args: &JournalArgs) -> Result<u8> {
    let filtering = args.failed || args.since.is_some();
    let pool = if filtering { FILTER_POOL.max(args.limit) } else { args.limit };
    let mut rows = store.journal_recent(args.profile.as_deref(), pool)?;

    if let Some(since) = &args.since {
        let window = parse_duration(since)?;
        let cutoff = now_secs().saturating_sub(window.as_secs());
        rows.retain(|r| u64::try_from(r.at).unwrap_or(0) >= cutoff);
    }
    if args.failed {
        let unresolved = unresolved_ops(&rows);
        rows.retain(|r| r.phase == "failed" || unresolved.contains(&r.op_id));
    }
    rows.truncate(args.limit);

    if rows.is_empty() {
        println!("{}", nothing_to_show(args));
        return Ok(crate::exit::OK);
    }

    println!(
        "{:<10}  {:<10}  {:<16}  {:<7} {:<6} {:>5}  file",
        "when", "phase", "action", "origin", "by", "conf"
    );
    for r in &rows {
        println!(
            "{:<10}  {:<10}  {:<16}  {:<7} {:<6} {:>5}  {}",
            age(u64::try_from(r.at).unwrap_or(0)),
            r.phase,
            r.action,
            r.provenance.origin.as_str(),
            r.provenance.decided_by.as_str(),
            r.provenance.confidence.map_or_else(|| "-".to_owned(), |c| format!("{c:.2}")),
            describe(r),
        );
    }

    println!("\n{} row(s).", rows.len());
    if !args.failed {
        let unresolved = unresolved_ops(&rows);
        if !unresolved.is_empty() {
            println!(
                "{} operation(s) recorded an intent with no result: a crash partway through, \
                 or a run still going. `bower journal --failed` lists them.",
                unresolved.len()
            );
        }
    }
    Ok(crate::exit::OK)
}

/// Op ids with an `intent` row and no `committed` or `failed` row.
///
/// Only meaningful over the rows actually fetched: an operation whose result
/// landed outside the window looks unresolved here. The message says "recorded
/// an intent with no result" rather than asserting a crash, for that reason.
fn unresolved_ops(rows: &[JournalRow]) -> Vec<String> {
    let mut seen: BTreeMap<&str, (bool, bool)> = BTreeMap::new();
    for r in rows {
        let e = seen.entry(&r.op_id).or_insert((false, false));
        match r.phase.as_str() {
            "intent" => e.0 = true,
            _ => e.1 = true,
        }
    }
    seen.into_iter()
        .filter(|(_, (intent, resolved))| *intent && !*resolved)
        .map(|(id, _)| id.to_owned())
        .collect()
}

/// `name -> category/name`.
///
/// Destinations are absolute in the journal, which is what makes it an audit
/// record, and unreadable in a table -- every row would repeat the same long
/// prefix. The last two components are what actually differ between rows;
/// `bower journal --limit 1` is not the tool for reading a full path, the
/// database is.
fn describe(r: &JournalRow) -> String {
    let source = leaf(&r.source);
    match &r.dest {
        Some(dest) => {
            let parent = dest.parent().and_then(std::path::Path::file_name);
            let shown = parent
                .map_or_else(|| leaf(dest), |p| format!("{}/{}", p.to_string_lossy(), leaf(dest)));
            format!("{source} -> {shown}")
        }
        None => source,
    }
}

fn leaf(p: &std::path::Path) -> String {
    p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned())
}

fn nothing_to_show(args: &JournalArgs) -> String {
    let what =
        if args.failed { "no failed or unresolved operations" } else { "nothing in the journal" };
    match (&args.profile, &args.since) {
        (Some(p), Some(s)) => format!("{what} for profile `{p}` in the last {s}"),
        (Some(p), None) => format!("{what} for profile `{p}`"),
        (None, Some(s)) => format!("{what} in the last {s}"),
        (None, None) => what.to_owned(),
    }
}
