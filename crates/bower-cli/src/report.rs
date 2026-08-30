//! Human-readable run output.

use bower_core::exec::{Executed, Pending};
use bower_core::run::RunReport;
use bower_core::scan::SkipReason;

/// Prints one profile's run.
pub(crate) fn print_run(report: &RunReport, dry_run: bool) {
    let mode = if dry_run { " (dry run -- nothing was written)" } else { "" };
    println!("\n{}{mode}", report.profile);
    println!("  scanned {} file(s), skipped {}", report.scanned, report.skipped.len());

    if report.outcomes.is_empty() {
        println!("  nothing to do");
    }

    for outcome in &report.outcomes {
        let name = outcome.file.relative.display().to_string();
        match (&outcome.error, &outcome.executed) {
            (Some(err), _) => println!("  {:<9} {name}\n              {err}", "ERROR"),
            (
                None,
                Some(Executed::Moved { to, renamed, .. } | Executed::WouldMove { to, renamed, .. }),
            ) => {
                let verb = if *renamed { "RENAME" } else { "MOVE" };
                println!("  {verb:<9} {name}\n              -> {}", to.display());
            }
            (None, Some(Executed::Nothing { reason })) => {
                println!("  {:<9} {name}\n              {reason}", "SKIP");
            }
            (None, Some(Executed::Deferred(pending))) => {
                let (label, detail) = match pending {
                    Pending::Quarantine { reason } => ("QUARANTINE", reason.clone()),
                    Pending::Recycle { reason, confidence } => {
                        ("RECYCLE?", format!("{reason} (confidence {confidence:.2})"))
                    }
                    Pending::Review { reason, .. } => ("REVIEW", reason.clone()),
                };
                println!("  {label:<9} {name}\n              {detail}");
            }
            (None, None) => println!("  {:<9} {name}", "?"),
        }
    }

    print_skips(report);
    print_summary(report, dry_run);
}

fn print_skips(report: &RunReport) {
    let mut settling = 0;
    let mut excluded = 0;
    let mut managed = 0;
    let mut symlinks = 0;
    let mut unreadable = Vec::new();
    for s in &report.skipped {
        match &s.reason {
            SkipReason::StillSettling => settling += 1,
            SkipReason::ExcludedByPattern => excluded += 1,
            SkipReason::ManagedOutput => managed += 1,
            SkipReason::Symlink => symlinks += 1,
            SkipReason::Unreadable(e) => unreadable.push(format!("{}: {e}", s.path.display())),
        }
    }
    let mut parts = Vec::new();
    if excluded > 0 {
        parts.push(format!("{excluded} excluded by pattern"));
    }
    if managed > 0 {
        parts.push(format!("{managed} in managed output directories"));
    }
    if settling > 0 {
        parts.push(format!("{settling} still settling"));
    }
    if symlinks > 0 {
        parts.push(format!("{symlinks} symlink(s)"));
    }
    if !parts.is_empty() {
        println!("  skipped: {}", parts.join(", "));
    }
    for u in unreadable {
        println!("  unreadable: {u}");
    }
}

fn print_summary(report: &RunReport, dry_run: bool) {
    let moved = if dry_run { report.would_move() } else { report.moved() };
    let verb = if dry_run { "would move" } else { "moved" };
    let attention = report.attention_count();
    let errors = report.errors();

    print!("  summary: {verb} {moved}");
    if attention > 0 {
        print!(", {attention} awaiting a human");
    }
    if errors > 0 {
        print!(", {errors} error(s)");
    }
    println!();
}
