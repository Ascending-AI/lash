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

/// The field naming a record as a handle.
pub const HANDLE_FIELD: &str = "__handle__";

/// The one handle kind. Every handle record carries this as [`HANDLE_FIELD`].
pub const HANDLE_KIND: &str = "lash";

/// The handle kind minted before ADR 0095 collapsed the two encodings.
///
/// FIG-2996 part 2 moves the process handle views onto [`HANDLE_KIND`] and
/// deletes this constant together with the [`parse_handle`] arm that reads it.
/// It is not a reader for stored artifacts — ADR 0095 admits none, and no
/// pre-cutover handle loads — only for the record the process views still emit
/// between part 1 and part 2 of the same cutover.
pub const LEGACY_PROCESS_HANDLE_KIND: &str = "process";

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
    /// A durable process and the incarnation of it this handle was taken
    /// against.
    Process {
        process_id: String,
        incarnation: u64,
    },
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

    /// Mints the handle for `incarnation` of the process `process_id`.
    ///
    /// The process id is spelled last because it is host-supplied and may
    /// itself contain a separator; everything after the third field is part of
    /// it.
    pub fn process(process_id: &str, incarnation: u64) -> Self {
        Self(format!(
            "{PROCESS_TAG}{SEPARATOR}{incarnation:x}{SEPARATOR}{process_id}"
        ))
    }

    /// Reads the parts back out, or `None` if this is not an id this module
    /// minted.
    ///
    /// A `None` here is the whole refusal: a hand-written handle, a handle from
    /// a previous execution and a handle whose text was tampered with all fail
    /// to name live work, and the caller reports that in its own terms.
    pub fn target(&self) -> Option<HandleTarget> {
        let mut parts = self.0.splitn(3, SEPARATOR);
        let tag = parts.next()?;
        let second = parts.next()?;
        let third = parts.next()?;
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
                if second.len() != 16 {
                    return None;
                }
                Some(HandleTarget::Tool {
                    execution_nonce: u64::from_str_radix(second, 16).ok()?,
                    request: u32::from_str_radix(third, 16).ok()?,
                })
            }
            PROCESS_TAG => {
                if third.is_empty() {
                    return None;
                }
                Some(HandleTarget::Process {
                    incarnation: u64::from_str_radix(second, 16).ok()?,
                    process_id: third.to_string(),
                })
            }
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

/// Reads a handle record's two fields into a [`HandleId`].
///
/// `kind` is the record's [`HANDLE_FIELD`] and `id` its `id`, as the caller's
/// own value type spells them. Returns `None` when the record is not a handle,
/// which is how a plain value handed to `await` is told apart from a handle.
///
/// The [`LEGACY_PROCESS_HANDLE_KIND`] arm additionally takes the incarnation,
/// which that record carries in its own field; it goes away with the constant
/// in part 2.
pub fn parse_handle(kind: &str, id: &str, legacy_incarnation: Option<u64>) -> Option<HandleId> {
    match kind {
        HANDLE_KIND => Some(HandleId::from_text(id)),
        LEGACY_PROCESS_HANDLE_KIND => {
            if id.is_empty() {
                return None;
            }
            Some(HandleId::process(id, legacy_incarnation?))
        }
        _ => None,
    }
}

/// Whether a record carrying these field names is the handle shape.
///
/// This is a question about *shape*, not about live work: the linker asks it
/// of a record literal whose values it cannot see, to decide whether awaiting
/// that record is visibly settled. Only [`parse_handle`] can say whether a
/// handle names anything, and only at runtime.
pub fn is_handle_shape<'a>(names: impl IntoIterator<Item = &'a str>) -> bool {
    names.into_iter().any(|name| name == HANDLE_FIELD)
}

/// Reads a handle out of a JSON record, for the crates whose values are JSON.
pub fn parse_handle_json(value: &serde_json::Value) -> Option<HandleId> {
    let kind = value.get(HANDLE_FIELD)?.as_str()?;
    let id = value.get("id")?.as_str()?;
    parse_handle(kind, id, value.get("incarnation").and_then(|v| v.as_u64()))
}

/// Builds the one handle record as JSON.
pub fn handle_record_json(id: &HandleId) -> serde_json::Value {
    serde_json::json!({ HANDLE_FIELD: HANDLE_KIND, "id": id.as_str() })
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

    #[test]
    fn process_handles_round_trip_including_ids_that_contain_the_separator() {
        // Process ids are host-supplied and really do contain separators:
        // `tool:call-01JZ...` and `subagent:session-01JZ...` are the two the
        // process-controls catalogue teaches.
        for process_id in [
            "p-1",
            "tool:call-01JZK7G4QP9Q4J7W3Q2E1H6M9C",
            "a.b.c.d",
            "..",
        ] {
            let id = HandleId::process(process_id, 3);
            assert_eq!(
                id.target(),
                Some(HandleTarget::Process {
                    process_id: process_id.to_string(),
                    incarnation: 3,
                }),
                "process handle {id} lost its parts"
            );
        }
    }

    #[test]
    fn a_tool_handle_and_a_process_handle_never_share_an_id() {
        assert_ne!(HandleId::tool(1, 2), HandleId::process("1", 2));
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
    fn the_legacy_process_record_parses_to_the_same_id_the_mint_produces() {
        // Part 2 deletes this arm; until it lands the process views still emit
        // the two-field record, and it must name the same handle.
        let legacy = serde_json::json!({
            "__handle__": "process",
            "id": "p-7",
            "incarnation": 2,
        });
        assert_eq!(
            parse_handle_json(&legacy),
            Some(HandleId::process("p-7", 2))
        );
    }

    #[test]
    fn a_legacy_process_record_without_an_incarnation_is_refused() {
        assert_eq!(
            parse_handle_json(&serde_json::json!({ "__handle__": "process", "id": "p-7" })),
            None
        );
    }
}
