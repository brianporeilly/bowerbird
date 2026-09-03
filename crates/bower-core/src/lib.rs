#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::case_sensitive_file_extension_comparisons
    )
)]
pub mod context;
pub mod exec;
pub mod hash;
pub mod llm;
pub mod lock;
pub mod model;
pub mod policy;
pub mod review;
pub mod run;
pub mod scan;
pub mod state;

/// The machine's current offset from UTC, in seconds east.
///
/// This is the one place the timezone is read. Callers resolve it once per run
/// and pass it into [`policy::PlanInput`], which keeps the policy engine free
/// of ambient environment input -- the same reason [`policy::Occupancy`] is
/// handed in rather than looked up.
///
/// **Falls back to UTC (`0`) when the offset cannot be determined soundly.**
/// `time` refuses to guess in a process that has other threads running, because
/// reading the timezone is not thread-safe against a concurrent `setenv`. A
/// wrong-by-a-day filename is a far better outcome than a data race, and the
/// fallback is the behaviour this had before local time was supported at all.
#[must_use]
pub fn local_utc_offset_secs() -> i64 {
    time::UtcOffset::current_local_offset().map_or(0, |offset| i64::from(offset.whole_seconds()))
}
