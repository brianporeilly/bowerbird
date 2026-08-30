#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
//! Where the codebase is allowed to delete anything.
//!
//! ADR-0001's central promise is that deletion is reversible by construction:
//! approving a deletion moves a file into the recycle store, and only an
//! explicit `bower recycle purge` destroys it. That promise lives or dies on a
//! small, auditable set of call sites, so this test pins the set down.
//!
//! A grep, like `policy_purity`. Its job is to make an accidental new deletion
//! path impossible to merge quietly, not to defeat a determined one.

use std::path::Path;

/// Every file permitted to remove something, and why.
const ALLOWED: &[(&str, &str)] = &[
    ("exec.rs", "unlinks the *source* after a move; the bytes survive at the destination"),
    ("exec.rs", "removes a partially copied destination after a failed cross-device move"),
    ("lock.rs", "removes its own lock file on drop"),
    ("review.rs", "`purge` -- the one deliberate, user-invoked destruction"),
];

#[test]
fn nothing_outside_the_audited_set_deletes_anything() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let permitted: Vec<&str> = ALLOWED.iter().map(|(f, _)| *f).collect();
    let mut offences = Vec::new();

    visit(&src, &mut |path, text| {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        if permitted.contains(&name) {
            return;
        }
        for (n, line) in text.lines().enumerate() {
            let code = line.trim_start();
            if code.starts_with("//") || code.starts_with('*') {
                continue;
            }
            if code.contains("remove_file") || code.contains("remove_dir") {
                offences.push(format!("{}:{}: {}", path.display(), n + 1, code.trim()));
            }
        }
    });

    assert!(
        offences.is_empty(),
        "deletion must stay confined to the audited call sites:\n{}\n\nPermitted:\n{}",
        offences.join("\n"),
        ALLOWED.iter().map(|(f, why)| format!("  {f}: {why}")).collect::<Vec<_>>().join("\n"),
    );
}

#[test]
fn the_only_unconditional_delete_is_in_purge() {
    let review = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/review.rs");
    let text = std::fs::read_to_string(&review).unwrap();

    let deletes: Vec<_> = text
        .lines()
        .enumerate()
        .filter(|(_, l)| {
            let c = l.trim_start();
            !c.starts_with("//") && (c.contains("remove_file") || c.contains("remove_dir"))
        })
        .collect();

    assert_eq!(deletes.len(), 1, "expected exactly one delete in review.rs, found {deletes:?}");

    // It must sit inside `purge`, not in approve, reject, or restore.
    let (line_no, _) = deletes[0];
    let preceding = text.lines().take(line_no).collect::<Vec<_>>().join("\n");
    let enclosing = preceding.rfind("\npub fn ").map(|i| &preceding[i + 8..]);
    let enclosing = enclosing.and_then(|s| s.split('(').next()).unwrap_or("<none>");
    assert_eq!(
        enclosing, "purge",
        "the delete in review.rs must live in `purge`, but is inside `{enclosing}`"
    );
}

fn visit(dir: &Path, f: &mut impl FnMut(&Path, &str)) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            visit(&path, f);
        } else if path.extension().is_some_and(|e| e == "rs")
            && let Ok(text) = std::fs::read_to_string(&path)
        {
            f(&path, &text);
        }
    }
}
