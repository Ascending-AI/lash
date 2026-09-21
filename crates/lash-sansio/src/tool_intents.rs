//! The recorded tool-intent kind vocabulary.
//!
//! This file is deliberately tiny and dedicated: it is the single source of
//! truth for [`ToolIntentKind`](crate::ToolIntentKind) here and `ToolIntent`
//! in lash-core — each generates itself by invoking [`tool_intent_variants!`]
//! with its own generator, so neither can carry a variant the other lacks —
//! and the durable version guards pin the file whole. A `rust_items`
//! projection cannot see inside `macro_rules!`, so a vocabulary that lived in
//! a shared file would be invisible to the settlement's guard: adding a
//! declaration means adding one line here, and the guards force the version
//! question at review time.
#[macro_export]
macro_rules! tool_intent_variants {
    ($generator:ident) => {
        $generator! {
            StartProcess "start_process",
            SignalProcess "signal_process",
            CancelProcess "cancel_process",
            EmitProcessEvent "emit_process_event",
            EmitTrigger "emit_trigger_mutated",
            RegisterProcessDefinition "register_process_definition",
            RegisterTrigger "register_trigger",
        }
    };
}
