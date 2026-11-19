//! `bower undo` -- remove the links a link-mode run made.
//!
//! Only links. A move is not undone here: see `bower_core::undo` for why.

use anyhow::Result;
use bower_config::LinkMode;
use bower_core::state::Store;
use bower_core::undo::{self, Candidate, Undone};

use crate::cli::UndoArgs;
use crate::triage::{confirm, now_secs, parse_duration};

pub(crate) fn run(store: &Store, args: &UndoArgs) -> Result<u8> {
    let since = match &args.since {
        Some(text) => Some(now_secs().saturating_sub(parse_duration(text)?.as_secs())),
        None => None,
    };
    let candidates = undo::plan(store, args.profile.as_deref(), since)?;

    // Drop what is already gone before asking anyone anything: the prompt
    // should count work, not history.
    let live: Vec<&Candidate> = candidates
        .iter()
        .filter(|c| {
            bower_core::exec::link_state(&c.dest, &c.source, c.via)
                != bower_core::exec::LinkState::Gone
        })
        .collect();

    if live.is_empty() {
        println!("no links to undo.");
        return Ok(crate::exit::OK);
    }

    if !args.dry_run
        && !confirm(
            args.yes,
            &format!("remove {} link(s)? Source files are not touched.", live.len()),
        )?
    {
        println!("nothing removed.");
        return Ok(crate::exit::OK);
    }

    let (mut removed, mut left, mut failed) = (0usize, 0usize, 0usize);
    for c in live {
        let name = c.dest.display();
        let kind = if c.via == LinkMode::Symlink { "symlink" } else { "hardlink" };
        match undo::undo_one(store, c, args.dry_run)? {
            Undone::Removed => {
                removed += 1;
                println!("UNLINK  {name}  ({kind})");
            }
            Undone::WouldRemove => {
                removed += 1;
                println!("would unlink  {name}  ({kind})");
            }
            Undone::Gone => {}
            Undone::Changed => {
                left += 1;
                println!("left alone  {name}: no longer the {kind} that was made");
            }
            Undone::LastCopy => {
                left += 1;
                println!("left alone  {name}: its source is gone, so this is the only copy");
            }
            Undone::Failed(e) => {
                failed += 1;
                println!("FAILED  {name}: {e}");
            }
        }
    }

    let verb = if args.dry_run { "would remove" } else { "removed" };
    println!("\n{verb} {removed} link(s); {left} left alone; {failed} failed.");
    if removed > 0 && !args.dry_run {
        println!("A re-run files these sources again unless the profile or its link_mode changes.");
    }
    Ok(if failed > 0 {
        crate::exit::ERROR
    } else if left > 0 {
        crate::exit::ATTENTION
    } else {
        crate::exit::OK
    })
}
