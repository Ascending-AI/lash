//! Typed evidence lines for the judged `process-operations` runbook (FIG-3156).
//!
//! The runbook tells its judge to inspect a named artifact per phase and read a
//! typed outcome out of it — a discard reason, a claim shape, a retention
//! decision. `scripts/process-operations-e2e.sh` produces those artifacts by
//! `tee`ing a `cargo test --nocapture` run into them, so the artifact contains
//! whatever the fixture wrote to stdout and nothing else. For five phases the
//! fixtures wrote nothing, and the runbook's own preamble forbids crediting the
//! script's prose instead ("do not treat a green script as the judgment
//! itself") — so those phases were unjudgeable.
//!
//! These lines close that gap. They are **not** debug output and must not be
//! removed as such: they are the judge's only view of the values the assertions
//! beside them already checked. Each line is emitted from the same scope as the
//! assertion it evidences, so the recorded value and the asserted value cannot
//! drift apart, and each carries a `checkpoint` discriminator in the shape
//! scenarios 2, 5 and 8 already use (`{"checkpoint":"…", …}`).
//!
//! Emission is `println!`, so it is captured and invisible under a normal
//! `cargo test` or `cargo nextest` run and appears only where a caller asked
//! for `--nocapture`.

/// Writes one runbook evidence line as compact JSON on stdout.
///
/// Takes an already-built [`serde_json::Value`] rather than formatting fields
/// here: the caller is the scope that holds the asserted values, and a helper
/// that re-derived them would be evidencing itself.
#[expect(
    clippy::expect_used,
    reason = "evidence lines are built from serde_json::json! literals, which always serialize"
)]
pub fn checkpoint(value: serde_json::Value) {
    println!(
        "{}",
        serde_json::to_string(&value).expect("serialize runbook evidence checkpoint")
    );
}
