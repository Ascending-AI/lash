//! The logical-run family (FIG-3600 S7): a session's logical runs with
//! their terminal evidence, the bindings of accepted inputs to the runs that
//! shift them, and the control intents an operator's verbs and a session's
//! close record.
//!
//! Every statement here is issued verbatim by both backends: the upserts are
//! spelled `ON CONFLICT ... DO NOTHING`, which SQLite and PostgreSQL both
//! read, and no statement carries a boolean literal.

pub mod control_intents;
pub mod run_inputs;
pub mod runs;
