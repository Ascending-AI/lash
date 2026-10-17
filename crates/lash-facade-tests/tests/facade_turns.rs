use lash::{EmbedError, LashCore, Result, TurnInput, async_trait};
use lash_core::ToolProvider;
use lash_core::facade_support::PluginFactory;
use lash_core::plugin::StaticPluginFactory;
use std::sync::Arc;

use lash_core::facade_support::ProviderHandle;
use lash_protocol_types::SessionId;
use lash_protocol_types::sync::MutexExt;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use lash_core::llm::types::{LlmContentBlock, LlmRequest, LlmResponse, LlmRole, LlmStreamEvent};
use lash_core::{LlmOutputPart, StoreError};

use lash_facade_tests::*;

#[cfg(test)]
#[path = "facade_turns/tests/mod.rs"]
mod tests;
