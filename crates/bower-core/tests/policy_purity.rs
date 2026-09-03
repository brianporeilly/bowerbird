#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! The policy engine must not touch the filesystem.
//!
//! Within `bower-core` that is a convention -- the scanner and executor
//! legitimately do I/O -- so this test makes the convention mechanical. It
//! lives outside `src/policy/` because it necessarily reads files itself and
//! would otherwise flag its own source.
//!
//! A grep, not a type-level proof. Its job is to make an accidental violation
//! impossible to merge quietly, not to defeat a determined one.

const FORBIDDEN: &[&str] =
    &["std::fs", "fs::", "File::", "std::process", "std::net", "OpenOptions", "tokio::"];

#[test]
fn policy_module_performs_no_io() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/policy");
    let mut offences = Vec::new();
    let mut checked = 0usize;

    let entries = std::fs::read_dir(&dir).expect("policy source directory should exist");
    for entry in entries {
        let path = entry.expect("readable dir entry").path();
        if path.extension().is_none_or(|e| e != "rs") {
            continue;
        }
        checked += 1;
        let text = std::fs::read_to_string(&path).expect("readable source file");
        for (n, line) in text.lines().enumerate() {
            if line.trim_start().starts_with("//") || line.trim_start().starts_with('*') {
                continue;
            }
            for needle in FORBIDDEN {
                if line.contains(needle) {
                    offences.push(format!("{}:{}: `{needle}`", path.display(), n + 1));
                }
            }
        }
    }

    assert!(checked > 0, "found no policy sources to check at {}", dir.display());
    assert!(
        offences.is_empty(),
        "the policy engine must stay pure, but found filesystem/process/network use:\n{}",
        offences.join("\n")
    );
}
