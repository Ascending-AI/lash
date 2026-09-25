//! The logical-root family (FIG-3600 S7): a session's logical roots with
//! their terminal evidence, the bindings of accepted inputs to the roots that
//! drive them, and the control intents an operator's verbs and a session's
//! close record.
//!
//! Every statement here is issued verbatim by both backends: the upserts are
//! spelled `ON CONFLICT ... DO NOTHING`, which SQLite and PostgreSQL both
//! read, and no statement carries a boolean literal.

pub mod control_intents;
pub mod root_inputs;
pub mod roots;
