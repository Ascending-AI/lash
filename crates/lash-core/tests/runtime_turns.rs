//! Relocated `lash-core` runtime suites (see `tests/runtime/tests/`).
//!
//! These were compiled into the crate's unit-test target until FIG-3041 split
//! them into integration binaries; the `runtime::tests::*` module path is
//! reproduced verbatim so every test keeps its name.

#![expect(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "test target: clippy's allow-unwrap-in-tests only exempts #[test] functions, and the setup helpers around them in this target are test code too"
)]

#[allow(dead_code, unused_imports)]
mod runtime_support;

mod runtime {
    #[allow(unused_imports, dead_code)]
    pub(crate) mod tests {
        pub(crate) use lash_core::attachments::{
            AttachmentProducer, AttachmentSourcePolicy, OpenAttachmentSourcePolicy,
        };
        pub(crate) use lash_core::facade_support::*;
        pub(crate) use lash_core::llm::types::{
            LlmOutputPart, LlmProviderTraceSender, LlmRequest, LlmResponse, LlmStreamEvent,
        };
        pub(crate) use lash_core::plugin::{
            CheckpointHookContext, PrepareTurnRequest, SessionConfigChangedContext, SessionRelation,
        };
        pub(crate) use lash_core::plugin::{
            RuntimeServices, SessionObservedProcessOutcome, SessionObservedProcessReceipt,
            SessionObserverIntent,
        };
        pub(crate) use lash_core::runtime::*;
        pub(crate) use lash_core::sansio::{LlmCallError, Response};
        pub(crate) use lash_core::session_model::{
            Message, MessageRole, Part, RuntimeSessionPolicy, SessionPolicy, SessionStreamEvent,
            TokenUsage, make_error_event, reassign_part_ids, shared_parts,
        };
        pub(crate) use lash_core::testing::runtime_internals::*;
        pub(crate) use lash_core::*;
        pub(crate) use lash_sansio::sync::MutexExt;
        pub(crate) use std::collections::HashMap;
        pub(crate) use std::sync::Mutex as StdMutex;
        pub(crate) use tokio::sync::mpsc;

        pub(crate) mod effect {
            pub(crate) use crate::runtime_support::effect_controller_doubles::*;
            pub(crate) use crate::runtime_support::effect_recording_authority::*;
        }

        pub(crate) use lash_core::llm::transport::LlmTransportError;
        pub(crate) use lash_core::llm::types::{LlmProviderTraceEvent, LlmUsage};
        pub(crate) use lash_core::plugin::StaticPluginFactory;
        pub(crate) use lash_core::testing::TestProvider;
        pub(crate) use lash_core::testing::runtime_helpers as helpers;
        pub(crate) use lash_core::testing::runtime_helpers::*;
        pub(crate) use lash_core::testing::trace_capture;
        pub(crate) use serde_json::json;
        pub(crate) use std::path::PathBuf;
        pub(crate) use std::sync::atomic::{AtomicBool, Ordering};
        pub(crate) use std::sync::{Arc, Mutex};
        pub(crate) use tokio_util::sync::CancellationToken;

        mod turns;
    }
}
