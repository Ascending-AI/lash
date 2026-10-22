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
use serde::{Deserialize, Serialize};

#[macro_export]
macro_rules! tool_intent_variants {
    ($generator:ident) => {
        $generator! {
            StartProcess "start_process",
            CancelProcess "cancel_process",
            GetDefinition "get_definition",
            PublishDefinition "publish_definition",
        }
    };
}

macro_rules! define_tool_intent_kind {
    ($($variant:ident $wire:literal,)*) => {
        /// Literal command kind of one recorded declaration.
        ///
        /// Generated from [`tool_intent_variants!`]; never hand-edited.
        #[derive(
            Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema,
        )]
        #[serde(rename_all = "snake_case")]
        pub enum ToolIntentKind {
            $($variant,)*
        }

        impl ToolIntentKind {
            /// Every kind the generated variant set declares, in declaration order.
            pub const ALL: &'static [Self] = &[$(Self::$variant,)*];

            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $wire,)*
                }
            }
        }
    };
}

crate::tool_intent_variants!(define_tool_intent_kind);
