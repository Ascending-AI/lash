// FIG-1979 / ADR 0096: a turn cannot name a language.
//
// TypeScript is the sole RLM language, so nothing in the RLM option types
// carries one -- not the per-turn bag, and not the create contract. A turn that
// named a language its cells would ignore is a compile error, not a silently
// discarded field.

use lash::rlm::{TerminationMode, RlmTurnOptions};

fn a_turn_cannot_name_a_dialect() {
    let _ = RlmTurnOptions {
        dialect: Some("typescript"),
        termination: Some(TerminationMode::Natural),
        ..Default::default()
    };
}

fn main() {
    let _ = a_turn_cannot_name_a_dialect;
}
