//! The `review` and `recycle` command groups.

use anyhow::{Context, Result, bail};
use bower_config::Config;
use bower_core::exec::Mode;
use bower_core::review::{self, Approved, ResolveError, ResolveOptions};
use bower_core::state::{RecycleItem, ReviewItem, ReviewKind, Store};
use std::io::{IsTerminal, Write};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::cli::{ApproveArgs, ItemType, PurgeArgs, RejectArgs, ReviewListArgs};

/// Exit code signalling that items are still waiting on a human.
const ATTENTION: u8 = 2;
const OK: u8 = 0;

fn kind_of(t: ItemType) -> ReviewKind {
    match t {
        // "delete" is ADR-0001's spelling; internally a deletion suggestion is
        // a recycle, since that is what approving one actually does.
        ItemType::Delete => ReviewKind::Recycle,
        ItemType::Review => ReviewKind::Review,
        ItemType::Quarantine => ReviewKind::Quarantine,
    }
}

fn label(kind: ReviewKind) -> &'static str {
    match kind {
        ReviewKind::Review => "review",
        ReviewKind::Recycle => "delete?",
        ReviewKind::Quarantine => "conflict",
    }
}

fn options(config: &Config, dry_run: bool) -> ResolveOptions<'_> {
    ResolveOptions {
        mode: Mode::from_dry_run(dry_run),
        recycle_dir: config.general.recycle_dir.as_deref(),
    }
}

pub(crate) fn list(store: &Store, args: &ReviewListArgs) -> Result<u8> {
    let items = store.review_list(args.profile.as_deref(), args.kind.map(kind_of))?;
    if items.is_empty() {
        println!("nothing is waiting on a decision");
        return Ok(OK);
    }

    println!("{:>5}  {:<9} {:<14} FILE", "ID", "KIND", "PROFILE");
    for item in &items {
        println!(
            "{:>5}  {:<9} {:<14} {}",
            item.id,
            label(item.kind),
            item.profile,
            item.path.display()
        );
    }
    println!("\n{} item(s). `bower review show <id>` for detail.", items.len());
    Ok(ATTENTION)
}

pub(crate) fn show(store: &Store, id: i64) -> Result<u8> {
    let item = fetch(store, id)?;
    println!("item {}  [{}]  profile {}", item.id, label(item.kind), item.profile);
    println!("  file           {}", item.path.display());
    if item.path != item.original_path {
        println!("  originally     {}", item.original_path.display());
    }
    if let Some(dest) = &item.proposed_dest {
        println!("  would file to  {}", dest.display());
    }
    if !item.category.is_empty() {
        println!("  category       {}", item.category);
    }
    if let Some(c) = item.confidence {
        println!("  confidence     {c:.2}");
    }
    println!("  held because   {}", item.reason);
    if !item.reasoning.is_empty() {
        println!("  model said     {}", item.reasoning);
    }
    println!("  hash           {}", item.file_hash);
    Ok(OK)
}

pub(crate) fn approve(store: &Store, config: &Config, args: &ApproveArgs) -> Result<u8> {
    let opts = options(config, args.dry_run);

    let items = if args.all {
        let pending = store.review_list(args.profile.as_deref(), None)?;
        if pending.is_empty() {
            println!("nothing is waiting on a decision");
            return Ok(OK);
        }
        // A bulk approval is the one place a single keystroke moves many files,
        // so it says what it is about to do and waits to be told to go ahead.
        println!("about to approve {} item(s):", pending.len());
        for item in &pending {
            println!("  {:>5}  {:<9} {}", item.id, label(item.kind), item.path.display());
        }
        if !args.dry_run && !confirm(args.yes, "approve all of these?")? {
            println!("cancelled");
            return Ok(OK);
        }
        pending
    } else {
        let Some(id) = args.id else {
            bail!("give an item id, or --all to approve everything pending");
        };
        vec![fetch(store, id)?]
    };

    let mut failures = 0usize;
    for item in &items {
        match approve_one(store, config, item, &opts) {
            Ok(outcome) => println!("{}", describe(item, &outcome)),
            Err(e) => {
                failures += 1;
                eprintln!("item {}: {e}", item.id);
            }
        }
    }

    if failures > 0 {
        println!("\n{failures} of {} could not be approved", items.len());
        return Ok(ATTENTION);
    }
    Ok(OK)
}

fn approve_one(
    store: &Store,
    config: &Config,
    item: &ReviewItem,
    opts: &ResolveOptions<'_>,
) -> Result<Approved, ResolveError> {
    let profile = config
        .profile(&item.profile)
        .ok_or_else(|| ResolveError::NoSuchProfile(item.profile.clone()))?;
    review::approve(store, item, profile, opts)
}

fn describe(item: &ReviewItem, outcome: &Approved) -> String {
    match outcome {
        Approved::Filed { to } => format!("item {} filed to {}", item.id, to.display()),
        Approved::Recycled { to } => {
            format!("item {} recycled to {} (restorable)", item.id, to.display())
        }
        Approved::WouldFile { to } => {
            format!("item {} would be filed to {}", item.id, to.display())
        }
        Approved::WouldRecycle { to } => {
            format!("item {} would be recycled to {}", item.id, to.display())
        }
    }
}

pub(crate) fn reject(store: &Store, config: &Config, args: &RejectArgs) -> Result<u8> {
    let item = fetch(store, args.id)?;
    let outcome = review::reject(store, &item, args.reason.as_deref(), &options(config, false))?;

    println!("item {} rejected; it will not be proposed again unless the file changes", item.id);
    if let Some(to) = outcome.restored_to {
        println!("  moved back to {}", to.display());
    }
    if let Some(why) = outcome.restore_failed {
        println!("  left at {} ({why})", item.path.display());
    }
    Ok(OK)
}

// --- recycle ----------------------------------------------------------------

pub(crate) fn recycle_list(store: &Store) -> Result<u8> {
    let items = store.recycle_list()?;
    if items.is_empty() {
        println!("the recycle store is empty");
        return Ok(OK);
    }

    println!("{:>5}  {:<14} {:<12} ORIGINALLY", "ID", "PROFILE", "AGE");
    for item in &items {
        println!(
            "{:>5}  {:<14} {:<12} {}",
            item.id,
            item.profile,
            age(item.recycled_at),
            item.original_path.display()
        );
    }
    println!("\n{} item(s). Nothing here is deleted until `bower recycle purge`.", items.len());
    Ok(OK)
}

pub(crate) fn recycle_restore(store: &Store, config: &Config, id: i64) -> Result<u8> {
    let item = fetch_recycled(store, id)?;
    let to = review::restore(store, &item, &options(config, false))?;
    println!("restored to {}", to.display());
    Ok(OK)
}

pub(crate) fn recycle_purge(store: &Store, config: &Config, args: &PurgeArgs) -> Result<u8> {
    let window = parse_duration(&args.older_than)
        .with_context(|| format!("could not read `{}` as a duration", args.older_than))?;
    let cutoff = now_secs().saturating_sub(window.as_secs());

    let items = store.recycle_older_than(cutoff)?;
    if items.is_empty() {
        println!("nothing in the recycle store is older than {}", args.older_than);
        return Ok(OK);
    }

    println!("{} item(s) older than {}:", items.len(), args.older_than);
    for item in &items {
        println!("  {:>5}  {}", item.id, item.original_path.display());
    }

    if args.dry_run {
        println!("\ndry run: nothing was deleted");
        return Ok(OK);
    }
    // The only irreversible operation in the tool.
    if !confirm(args.yes, "permanently delete these? this cannot be undone")? {
        println!("cancelled");
        return Ok(OK);
    }

    let opts = options(config, false);
    let mut purged = 0usize;
    for item in &items {
        match review::purge(store, item, &opts) {
            Ok(()) => purged += 1,
            Err(e) => eprintln!("item {}: {e}", item.id),
        }
    }
    println!("permanently deleted {purged} item(s)");
    Ok(OK)
}

// --- helpers ----------------------------------------------------------------

fn fetch(store: &Store, id: i64) -> Result<ReviewItem> {
    store
        .review_get(id)?
        .with_context(|| format!("no pending item with id {id}; try `bower review list`"))
}

fn fetch_recycled(store: &Store, id: i64) -> Result<RecycleItem> {
    store
        .recycle_get(id)?
        .with_context(|| format!("no recycled item with id {id}; try `bower recycle list`"))
}

/// Asks before doing something bulk or irreversible.
///
/// Without a terminal there is nobody to ask, so `--yes` becomes mandatory
/// rather than the prompt being silently skipped.
fn confirm(assume_yes: bool, question: &str) -> Result<bool> {
    if assume_yes {
        return Ok(true);
    }
    if !std::io::stdin().is_terminal() {
        bail!("{question} -- not running on a terminal, so pass --yes to confirm");
    }
    print!("{question} [y/N] ");
    std::io::stdout().flush().ok();

    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes"))
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn age(at: u64) -> String {
    let secs = now_secs().saturating_sub(at);
    let days = secs / 86_400;
    if days > 0 {
        return format!("{days}d ago");
    }
    let hours = secs / 3_600;
    if hours > 0 {
        return format!("{hours}h ago");
    }
    format!("{}m ago", secs / 60)
}

/// Reads `30d`, `2w`, `12h`, `45m`, or a bare number of seconds.
fn parse_duration(text: &str) -> Result<Duration> {
    let text = text.trim();
    let (value, unit) = text.split_at(text.len().saturating_sub(1));
    let (value, multiplier) = match unit {
        "d" => (value, 86_400),
        "w" => (value, 604_800),
        "h" => (value, 3_600),
        "m" => (value, 60),
        "s" => (value, 1),
        _ => (text, 1),
    };
    let n: u64 = value.trim().parse().with_context(|| format!("`{text}` is not a duration"))?;
    Ok(Duration::from_secs(n.saturating_mul(multiplier)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_read_the_units_the_help_text_advertises() {
        assert_eq!(parse_duration("30d").unwrap(), Duration::from_secs(30 * 86_400));
        assert_eq!(parse_duration("2w").unwrap(), Duration::from_secs(2 * 604_800));
        assert_eq!(parse_duration("12h").unwrap(), Duration::from_secs(12 * 3_600));
        assert_eq!(parse_duration("45m").unwrap(), Duration::from_secs(45 * 60));
        assert_eq!(parse_duration("90s").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("120").unwrap(), Duration::from_secs(120));
    }

    #[test]
    fn a_duration_that_is_not_one_is_refused() {
        for bad in ["", "d", "abc", "-5d", "3.5d", "30x"] {
            assert!(parse_duration(bad).is_err(), "{bad} should not parse");
        }
    }
}
