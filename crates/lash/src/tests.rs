use crate::{EmbedError, LashCore, Result, TurnInput, async_trait};
use lash_core::facade_support::PluginFactory;
use lash_core::facade_support::ProviderHandle;
use lash_core::plugin::StaticPluginFactory;
use lash_core::{ToolProvider, TurnActivity};
use lash_vocabulary::identity::SessionId;
use std::sync::Arc;

use lash_protocol_types::sync::MutexExt;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use lash_core::llm::types::{LlmContentBlock, LlmRequest, LlmResponse};
use lash_core::{LlmOutputPart, StoreError};

// Unit laws use this crate's facade identity. Compile the one shared fixture
// source against it; external domain binaries link the fixture package.
#[path = "../../lash-facade-tests/src/lib.rs"]
pub(crate) mod host;
use crate::tests::host::fixtures::{self};
pub(crate) use host::*;
mod absent_session_delete;
mod assistant_hook_faults;
mod core_session_builder;
mod crashed_create_drain;
mod deleted_session_run_replay;
mod facade_construction;
mod plugin_operations;
mod recorded_execution_controls;
mod round_cancel_state;
mod send_handle;
mod tool_intent_ingress;
#[cfg(feature = "rlm")]
mod turn_retirement;
mod writer_fence;

mod cross_core_process_changes;

mod turn_streaming;

#[cfg(feature = "rlm")]
mod recorded_carriers;

#[cfg(feature = "rlm")]
mod adr_claims;

mod plugin_diagnostics;

#[cfg(feature = "rlm")]
mod admin_reads_without_runtime;

mod facade_turn;

mod durable_session;

mod history_hydration;
