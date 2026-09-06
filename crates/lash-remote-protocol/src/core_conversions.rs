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

mod llm;
mod observations;
mod processes;
mod prompt;
mod queued_events;
mod tools;
mod triggers;
mod turn_control;
mod turn_input;
mod turn_result;

pub use observations::{RemoteTurnActivitySink, replay_collected_activities};
pub(crate) use triggers::{decode_remote_json, encode_remote_json};

#[cfg(test)]
#[path = "core_conversions_tests.rs"]
mod core_conversions_tests;

#[cfg(test)]
#[path = "google_dialect_tests.rs"]
mod google_dialect_tests;
