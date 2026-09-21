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

/// `kind` is the record's [`HANDLE_FIELD`] and `id` its `id`, as the caller's own value type
/// spells them.
///
/// There is one kind and one place the parts live. A record that spells its
/// incarnation beside the id is not a handle: the incarnation belongs inside
/// the id, so a handle that names an incarnation it was not taken against
/// cannot be built.
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
    fn a_process_handle_is_the_one_record_and_carries_its_incarnation_inside_the_id() {
        let id = HandleId::process("p-7", 2);
        let record = handle_record_json(&id);
        assert_eq!(
            record,
            serde_json::json!({ "__handle__": "lash", "id": "p.2.p-7" })
        );
        assert_eq!(parse_handle_json(&record), Some(id.clone()));
        assert_eq!(
            id.target(),
            Some(HandleTarget::Process {
                process_id: "p-7".to_string(),
                incarnation: 2,
            })
        );
        // An incarnation spelled beside the id is not consulted, so it cannot
        // disagree with the one the handle was taken against.
        let mut shouted = handle_record_json(&id);
        shouted["incarnation"] = serde_json::json!(99);
        assert_eq!(parse_handle_json(&shouted), Some(id));
    }
}
