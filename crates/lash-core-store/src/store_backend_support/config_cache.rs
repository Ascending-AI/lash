//! Validated configuration reuse for read-only metadata loads (FIG-6645).
//!
//! A cache belongs to one storage instance. Its keys are immutable graph
//! roots, never mutable session/head pointers. A miss must read and validate
//! the graph in the caller's database snapshot. Write transactions do not
//! admit entries: a rolled-back graph must never become reusable material.
//! A write transaction may reuse a committed entry once it established the
//! root is visible in its own snapshot ([`ConfigCache::validates`]).

use std::collections::VecDeque;
use std::sync::Mutex;

use lash_protocol_types::sync::MutexExt;
use serde_json::Value;

use crate::checkpoint_graph::NodeId;
use crate::store::{FleetFormat, SESSION_HEAD_META_SCHEMA_VERSION, SessionHeadPayload, StoreError};
use lash_protocol_types::SessionId;

const BUDGET_BYTES: usize = 32 << 20;
const MAX_ENTRIES: usize = 128;

struct Entry {
    root: NodeId,
    payload: SessionHeadPayload,
    bytes: usize,
}

/// Bounded reuse of successfully decoded configuration by immutable root.
/// Head pointers, session identity, liveness and fences remain the reader's
/// responsibility on every read. Entries hold no durable retention leases.
///
/// Integrator class (ADR 0051): **store and durable-substrate implementors**.
#[derive(Default)]
pub struct ConfigCache {
    entries: Mutex<VecDeque<Entry>>,
}

impl ConfigCache {
    /// Reuse validated material while admitting its stamp under today's fleet.
    pub fn get(
        &self,
        root: NodeId,
        fleet: FleetFormat,
    ) -> Result<Option<SessionHeadPayload>, StoreError> {
        let mut entries = self.entries.lock_recover();
        let Some(index) = entries.iter().position(|entry| entry.root == root) else {
            return Ok(None);
        };
        crate::store::ensure_supported_schema_version_for_fleet(
            "SessionHeadMeta",
            entries[index].payload.schema_version,
            crate::surface_format!(SESSION_HEAD_META_SCHEMA_VERSION),
            fleet,
        )?;
        let Some(entry) = entries.remove(index) else {
            return Ok(None);
        };
        let payload = entry.payload.clone();
        entries.push_back(entry);
        Ok(Some(payload))
    }

    /// Whether a committed read validated `root` as session `session_id`'s
    /// configuration, admitted under today's fleet: a commit keeps the root
    /// as its validated identity without lending its payload (FIG-7049).
    /// That the root is visible in the caller's snapshot stays the caller's
    /// to establish; a miss validates the graph there.
    pub fn validates(
        &self,
        root: NodeId,
        session_id: &SessionId,
        fleet: FleetFormat,
    ) -> Result<bool, StoreError> {
        let mut entries = self.entries.lock_recover();
        let Some(index) = entries.iter().position(|entry| entry.root == root) else {
            return Ok(false);
        };
        crate::store::ensure_supported_schema_version_for_fleet(
            "SessionHeadMeta",
            entries[index].payload.schema_version,
            crate::surface_format!(SESSION_HEAD_META_SCHEMA_VERSION),
            fleet,
        )?;
        if entries[index].payload.session_id != *session_id {
            return Ok(false);
        }
        if let Some(entry) = entries.remove(index) {
            entries.push_back(entry);
        }
        Ok(true)
    }

    /// Decode graph material directly, admitting only successful validations.
    /// Large configurations still decode normally without occupying the cache.
    pub fn decode(
        &self,
        root: NodeId,
        value: Value,
        fleet: FleetFormat,
    ) -> Result<SessionHeadPayload, StoreError> {
        let bytes = value_bytes(&value);
        let payload = SessionHeadPayload::decode_value_for_fleet(value, fleet)?;
        if bytes <= BUDGET_BYTES {
            let mut entries = self.entries.lock_recover();
            if !entries.iter().any(|entry| entry.root == root) {
                let mut total: usize = entries.iter().map(|entry| entry.bytes).sum();
                while entries.len() >= MAX_ENTRIES || total + bytes > BUDGET_BYTES {
                    if let Some(entry) = entries.pop_front() {
                        total -= entry.bytes;
                    }
                }
                entries.push_back(Entry {
                    root,
                    payload: payload.clone(),
                    bytes,
                });
            }
        }
        Ok(payload)
    }
}

// Charge the expanded value, including repeated graph references, rather
// than the compact graph's bytes. Container slots and string capacities are
// counted before serde transfers the allocations to the typed payload.
fn value_bytes(value: &Value) -> usize {
    std::mem::size_of::<Value>()
        + match value {
            Value::String(text) => text.capacity(),
            Value::Array(items) => {
                items.capacity() * std::mem::size_of::<Value>()
                    + items.iter().map(value_bytes).sum::<usize>()
            }
            Value::Object(fields) => fields
                .iter()
                .map(|(key, value)| {
                    key.capacity() + 3 * std::mem::size_of::<usize>() + value_bytes(value)
                })
                .sum(),
            _ => 0,
        }
}
