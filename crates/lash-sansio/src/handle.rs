//! The one handle record, and the one pair that mints and parses its id.
//!
//! A handle is what a cell holds on to when the work it names has not finished
//! yet: a tool call it launched and will `await`, or a durable process it
//! started. Before ADR 0095 those were two records with two encodings, and four
//! readers each decided independently what a handle was — a loose
//! `is_process_handle` that accepted either key, an execution nonce carried in
//! its own field, and an incarnation back-filled from elsewhere.
//!
//! There is now one record,
//!
//! ```json
//! { "__handle__": "lash", "id": "<HandleId>" }
//! ```
//!
//! and one [`HandleId`] whose text carries everything the holder of the handle
//! is allowed to learn. The id is **opaque to the cell**: it is a string to be
//! carried and handed back, never parsed or constructed by guest code. Only the
//! runtime reads its parts, through [`HandleId::target`].
//!
//! This module is the single authority for that spelling. It lives in
//! `lash-sansio` because both sides of the seam need it and neither may depend
//! on the other: the language runtime mints handles for the tool calls a cell
//! launches, and core mints them for the processes it starts. Each crate keeps
//! its own value type and calls [`parse_handle`] with the two fields it read,
//! so the *format* has one owner even though the record has two builders.

use serde::{Deserialize, Serialize};

use crate::ProcessId;

/// The field naming a record as a handle.
pub const HANDLE_FIELD: &str = "__handle__";

/// The one handle kind. Every handle record carries this as [`HANDLE_FIELD`].
pub const HANDLE_KIND: &str = "lash";

/// An opaque handle id.
///
/// Ordering is over the id text, which makes a `BTreeMap` keyed by `HandleId`
/// serialize the same way twice for the same set of handles — what the VM's
/// pending-tool map needs to keep continuations byte-identical across two runs
/// of the same program from the same state.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HandleId(String);

/// What a handle id names, once the runtime has read its parts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HandleTarget {
    /// A pending tool call, identified by the execution that minted it and the
    /// request slot it holds in that execution.
    ///
    /// The execution nonce is folded into the id rather than carried beside it,
    /// so there is nothing to keep in step: a handle from an earlier cell, or
    /// one written by hand, simply does not name a live request.
    Tool { execution_nonce: u64, request: u32 },
    /// A durable process. Its minted id is never reused, so the id alone
    /// names one process for as long as anything can hold the handle.
    Process { process_id: ProcessId },
}

const TOOL_TAG: char = 't';
const PROCESS_TAG: char = 'p';
const SEPARATOR: char = '.';

impl HandleId {
    /// Mints the handle for request `request` of the execution identified by
    /// `execution_nonce`.
    pub fn tool(execution_nonce: u64, request: u32) -> Self {
        Self(format!(
            "{TOOL_TAG}{SEPARATOR}{execution_nonce:016x}{SEPARATOR}{request:x}"
        ))
    }

    /// Mints the handle for the process `process_id`.
    pub fn process(process_id: &ProcessId) -> Self {
        Self(format!("{PROCESS_TAG}{SEPARATOR}{process_id}"))
    }

    /// A `None` here is the whole refusal: a hand-written handle, a handle from
    /// a previous execution and a handle whose text was tampered with all fail
    /// to name live work, and the caller reports that in its own terms.
    ///
    /// A process handle is `p.<process id>` and nothing else: the retired
    /// `p.<incarnation>.<name>` spelling, whose name was host-chosen, names no
    /// process because no minted id contains the separator.
    pub fn target(&self) -> Option<HandleTarget> {
        let (tag, rest) = self.0.split_once(SEPARATOR)?;
        let tag = {
            let mut chars = tag.chars();
            let tag = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            tag
        };
        match tag {
            TOOL_TAG => {
                let (second, third) = rest.split_once(SEPARATOR)?;
                if second.len() != 16 {
                    return None;
                }
                Some(HandleTarget::Tool {
                    execution_nonce: u64::from_str_radix(second, 16).ok()?,
                    request: u32::from_str_radix(third, 16).ok()?,
                })
            }
            PROCESS_TAG => Some(HandleTarget::Process {
                process_id: ProcessId::parse(rest).ok()?,
            }),
            _ => None,
        }
    }

    /// The id as the record spells it.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Adopts an id read back off a handle record.
    ///
    /// Adoption does not validate: an id the runtime did not mint is a value
    /// that names no live work, which every caller already has to handle, and
    /// refusing here would only move the same refusal earlier.
    pub fn from_text(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

impl std::fmt::Display for HandleId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// `kind` is the record's [`HANDLE_FIELD`] and `id` its `id`, as the caller's own value type
/// spells them.
///
/// There is one kind and one place the parts live: a field spelled beside the
/// id is never consulted.
pub fn parse_handle(kind: &str, id: &str) -> Option<HandleId> {
    (kind == HANDLE_KIND).then(|| HandleId::from_text(id))
}

/// This is a question about *shape*, not about live work: the linker asks it
/// of a record literal whose values it cannot see, to decide whether awaiting
/// that record is visibly settled. Only [`parse_handle`] can say whether a
/// handle names anything, and only at runtime.
pub fn is_handle_shape<'a>(names: impl IntoIterator<Item = &'a str>) -> bool {
    names.into_iter().any(|name| name == HANDLE_FIELD)
}

pub fn parse_handle_json(value: &serde_json::Value) -> Option<HandleId> {
    let kind = value.get(HANDLE_FIELD)?.as_str()?;
    let id = value.get("id")?.as_str()?;
    parse_handle(kind, id)
}

pub fn handle_record_json(id: &HandleId) -> serde_json::Value {
    serde_json::json!({ HANDLE_FIELD: HANDLE_KIND, "id": id.as_str() })
}

/// The field of the internal result slot a process-start attempt answers with.
///
/// A start's process id is minted when the declared start is realized, after
/// the attempt sealed its output, so the attempt cannot answer a handle. It
/// answers `{"__start_slot__": <intent index>}` instead, and the realization
/// replaces the slot with the handle of the process that start registered
/// before the output reaches a model or a cell (ADR 0107). A slot is never a
/// handle and never survives to a holder.
pub const PROCESS_START_SLOT_FIELD: &str = "__start_slot__";

/// The unrealized slot for the declared start at `intent_index`.
pub fn process_start_slot_json(intent_index: u32) -> serde_json::Value {
    serde_json::json!({ PROCESS_START_SLOT_FIELD: intent_index })
}

/// The intent index a slot record names, if `value` is one.
pub fn process_start_slot(value: &serde_json::Value) -> Option<u32> {
    value
        .get(PROCESS_START_SLOT_FIELD)?
        .as_u64()
        .and_then(|index| u32::try_from(index).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_handles_round_trip_through_their_text() {
        for (nonce, request) in [(0u64, 0u32), (1, 2), (u64::MAX, u32::MAX), (0x9e37, 17)] {
            let id = HandleId::tool(nonce, request);
            assert_eq!(
                id.target(),
                Some(HandleTarget::Tool {
                    execution_nonce: nonce,
                    request,
                }),
                "tool handle {id} lost its parts"
            );
        }
    }

    /// A minted id carrying `n`'s bits under the UUIDv7 version and variant.
    fn process(n: u128) -> ProcessId {
        let version_and_variant = (0xf_u128 << 76) | (0b11_u128 << 62);
        ProcessId::from_minted((n & !version_and_variant) | (0x7_u128 << 76) | (0b10_u128 << 62))
    }

    #[test]
    fn process_handles_round_trip_through_their_text() {
        for n in [0, 1, u128::MAX, 0x0192_0000_0000_7000_8000_0000_0000_0001] {
            let id = HandleId::process(&process(n));
            assert_eq!(
                id.target(),
                Some(HandleTarget::Process {
                    process_id: process(n),
                }),
                "process handle {id} lost its parts"
            );
        }
    }

    #[test]
    fn a_tool_handle_and_a_process_handle_never_share_an_id() {
        assert_ne!(HandleId::tool(1, 2), HandleId::process(&process(1)));
    }

    #[test]
    fn distinct_executions_and_requests_get_distinct_handles() {
        assert_ne!(HandleId::tool(1, 0), HandleId::tool(2, 0));
        assert_ne!(HandleId::tool(1, 0), HandleId::tool(1, 1));
    }

    #[test]
    fn hand_written_ids_name_nothing() {
        for text in [
            "",
            "0",
            "t",
            "t.0",
            "tool.0.0",
            "t.0.0",                 // nonce must be the full 16 hex digits
            "t.00000000000000000.0", // and no more
            "x.0000000000000000.0",
            "t.zzzzzzzzzzzzzzzz.0",
            "t.0000000000000000.zz",
            "p.0.",
            "p.zz.a",
            "p.",
            "p.p-7",
            // The retired incarnation-bearing spelling, even around a minted id.
            "p.2.p-7",
            "p.2.p_00000000000070008000000000000007",
        ] {
            assert_eq!(
                HandleId::from_text(text).target(),
                None,
                "`{text}` was read as a live handle"
            );
        }
    }

    #[test]
    fn the_json_record_is_the_one_shape_and_parses_back() {
        let id = HandleId::tool(0x9e37, 1);
        let record = handle_record_json(&id);
        assert_eq!(
            record,
            serde_json::json!({ "__handle__": "lash", "id": id.as_str() })
        );
        assert_eq!(parse_handle_json(&record), Some(id));
    }

    #[test]
    fn the_handle_shape_is_the_marker_field_and_nothing_else() {
        assert!(is_handle_shape(["__handle__", "id"]));
        assert!(is_handle_shape(["__handle__"]));
        // Before ADR 0095 a bare `handle` key was read as a handle too; that
        // was one of the four divergent readers, and it is not a handle.
        assert!(!is_handle_shape(["handle"]));
        assert!(!is_handle_shape(["id"]));
        assert!(!is_handle_shape(std::iter::empty()));
    }

    #[test]
    fn a_non_handle_record_is_not_a_handle() {
        for value in [
            serde_json::json!({}),
            serde_json::json!({ "id": "t.0000000000000000.0" }),
            serde_json::json!({ "__handle__": "tool", "id": 0 }),
            serde_json::json!({ "__handle__": "other", "id": "x" }),
            serde_json::json!("t.0000000000000000.0"),
        ] {
            assert_eq!(parse_handle_json(&value), None, "{value} read as a handle");
        }
    }

    #[test]
    fn the_retired_process_record_is_not_a_handle() {
        // Before ADR 0095 a process handle was `{__handle__: "process", id,
        // incarnation}`, with the incarnation in its own field and back-filled
        // after the fact by the attempt coordinator. There is one kind now and
        // the incarnation rides inside the id, so the retired spelling names
        // nothing.
        for value in [
            serde_json::json!({ "__handle__": "process", "id": "p-7", "incarnation": 2 }),
            serde_json::json!({ "__handle__": "process", "id": "p-7" }),
        ] {
            assert_eq!(parse_handle_json(&value), None, "{value} read as a handle");
        }
    }

    #[test]
    fn a_process_handle_is_the_one_record_and_its_id_is_the_minted_process_id() {
        let id = HandleId::process(&process(7));
        let record = handle_record_json(&id);
        assert_eq!(
            record,
            serde_json::json!({ "__handle__": "lash", "id": "p.p_00000000000070008000000000000007" })
        );
        assert_eq!(parse_handle_json(&record), Some(id.clone()));
        assert_eq!(
            id.target(),
            Some(HandleTarget::Process {
                process_id: process(7),
            })
        );
        // A field spelled beside the id is not consulted.
        let mut shouted = handle_record_json(&id);
        shouted["incarnation"] = serde_json::json!(99);
        assert_eq!(parse_handle_json(&shouted), Some(id));
    }
}
