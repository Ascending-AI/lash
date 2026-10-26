use lash::{EmbedError, LashCore, Result, async_trait};
use lash_core::ToolProvider;
use std::sync::Arc;

use lash_core::facade_support::ProviderHandle;

use lash_protocol_types::sync::MutexExt;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use lash_core::LlmOutputPart;
use lash_core::llm::types::{LlmContentBlock, LlmResponse, LlmRole, LlmStreamEvent};

use lash_facade_tests::*;

#[cfg(test)]
#[path = "facade_streaming/tests/mod.rs"]
mod tests;
