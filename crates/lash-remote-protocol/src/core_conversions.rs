//! Drift guard convention: conversions exhaustively destructure their source
//! (struct patterns without `..`, enum matches without catch-all `_` arms) so
//! a new field on either side fails compilation here instead of silently
//! dropping off the wire.

use std::collections::HashMap;
use std::io::Write;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use lash_core::ToolDefinition;
use lash_core::llm::types as core_llm;

use super::*;

include!("core_conversions/triggers.rs");
include!("core_conversions/processes.rs");
include!("core_conversions/turn_input.rs");
include!("core_conversions/turn_control.rs");
include!("core_conversions/llm.rs");
include!("core_conversions/turn_result.rs");
include!("core_conversions/observations.rs");
include!("core_conversions/queued_events.rs");
include!("core_conversions/prompt.rs");
include!("core_conversions/tools.rs");

#[cfg(test)]
#[path = "core_conversions_tests.rs"]
mod core_conversions_tests;

#[cfg(test)]
#[path = "google_dialect_tests.rs"]
mod google_dialect_tests;
