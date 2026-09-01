// FIG-1979: the dialect has one carrier.
//
// It is resolved once at session materialization and the executor never reads a
// per-turn value, so the per-turn options type has no dialect field at all. A
// turn that named a dialect its cells would ignore is a compile error, not a
// silently discarded field.

use lash::rlm::{RlmDialect, RlmTermination, RlmTurnOptions};

fn a_turn_cannot_name_a_dialect() {
    let _ = RlmTurnOptions {
        dialect: Some(RlmDialect::Typescript),
        termination: Some(RlmTermination::Natural),
        final_answer_format: None,
    };
}

fn main() {
    let _ = a_turn_cannot_name_a_dialect;
}
