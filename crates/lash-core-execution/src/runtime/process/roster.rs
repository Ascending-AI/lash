//! Bounded fleet discovery and its durable change fence.

use serde::{Deserialize, Serialize};

use super::{
    ProcessChangeCursor, ProcessId, ProcessListFilter, ProcessOriginatorFilter, ProcessRecord,
    ProcessStatusFilter, ScopeId,
};

// Bounds allocations and parsing before inspecting any untrusted token contents.
const MAX_ROSTER_TOKEN_BYTES: usize = 64 * 1024;

/// Why an opaque roster token cannot be decoded. Start a fresh scan when its
/// version is unsupported; tokens do not promise compatibility across all releases.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ProcessRosterTokenError {
    #[error("the roster token is malformed")]
    Malformed,
    #[error("the roster token version is unsupported; start a fresh scan")]
    UnsupportedVersion,
    #[error("the roster token exceeds the 64 KiB decode limit")]
    TooLarge,
}

// This private codec preserves the selector without establishing a public filter shape.
#[derive(Serialize, Deserialize)]
#[serde(remote = "ProcessListFilter", deny_unknown_fields)]
struct RosterFilter {
    definition_id: Option<crate::ProcessDefinitionId>,
    status: ProcessStatusFilter,
    originator: Option<ProcessOriginatorFilter>,
    until: Option<ScopeId>,
    cancel_pending_before_ms: Option<u64>,
    identity_kind: Option<String>,
    identity_label: Option<String>,
    created_at_start_ms: Option<u64>,
    created_at_end_ms: Option<u64>,
    retired_since_ms: Option<u64>,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "ProcessRosterCursor", deny_unknown_fields)]
struct RosterPosition {
    store: String,
    #[serde(with = "RosterFilter")]
    filter: ProcessListFilter,
    after: ProcessId,
    through: ProcessId,
    change_cursor: ProcessChangeCursor,
}

#[derive(Serialize, Deserialize)]
struct RosterTokenV1(#[serde(with = "RosterPosition")] ProcessRosterCursor);

fn token_v1(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut token = String::with_capacity(2 + bytes.len() * 2);
    token.push_str("1.");
    for byte in bytes {
        token.push(char::from(HEX[usize::from(byte >> 4)]));
        token.push(char::from(HEX[usize::from(byte & 15)]));
    }
    token
}

/// Maximum matching records in a roster page, excluding its single lookahead row.
pub const MAX_PROCESS_ROSTER_PAGE_SIZE: usize = 256;

/// The verified durable change high water and oldest resumable position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessChangeBounds {
    pub current: ProcessChangeCursor,
    pub retained_after: ProcessChangeCursor,
}

/// A store-bound keyset continuation. Pass it unchanged with the same filter.
#[derive(Clone, Debug, PartialEq)]
pub struct ProcessRosterCursor {
    store: String,
    filter: ProcessListFilter,
    after: ProcessId,
    through: ProcessId,
    change_cursor: ProcessChangeCursor,
}

impl ProcessRosterCursor {
    /// Constructs a continuation for process-store implementors.
    pub fn new(
        store: String,
        filter: ProcessListFilter,
        after: ProcessId,
        through: ProcessId,
        change_cursor: ProcessChangeCursor,
    ) -> Self {
        Self {
            store,
            filter,
            after,
            through,
            change_cursor,
        }
    }

    /// Encodes this position as an opaque, URL-safe token for transport by a host.
    /// Hosts must authorize reads themselves: the token is neither authorization
    /// nor a snapshot promise. Resume with the same filter through `Processes::list`,
    /// which validates the issuing store, selection and retained change fence.
    /// Tokens over 64 KiB (including unusually large selector strings) cannot be decoded.
    #[expect(
        clippy::expect_used,
        reason = "the private position codec contains only strings, integers, enums and struct fields, with no fallible serializer or JSON map keys"
    )]
    pub fn to_token(&self) -> String {
        let bytes = serde_json::to_vec(&RosterTokenV1(self.clone()))
            .expect("roster positions contain only JSON-serializable values");
        token_v1(&bytes)
    }

    /// Decodes at most 64 KiB of token text into position data. This does not
    /// validate the position against a store or authorize any read. Unsupported
    /// versions require a fresh scan.
    pub fn from_token(token: &str) -> Result<Self, ProcessRosterTokenError> {
        use ProcessRosterTokenError::{Malformed, TooLarge, UnsupportedVersion};
        if token.len() > MAX_ROSTER_TOKEN_BYTES {
            return Err(TooLarge);
        }
        let (version, encoded) = token.split_once('.').ok_or(Malformed)?;
        if version != "1" {
            return Err(
                if !version.is_empty() && version.bytes().all(|b| b.is_ascii_digit()) {
                    UnsupportedVersion
                } else {
                    Malformed
                },
            );
        }
        let (pairs, remainder) = encoded.as_bytes().as_chunks::<2>();
        if !remainder.is_empty() {
            return Err(Malformed);
        }
        let nibble = |byte: u8| match byte {
            b'0'..=b'9' => Ok(byte - b'0'),
            b'a'..=b'f' => Ok(byte - b'a' + 10),
            _ => Err(Malformed),
        };
        let bytes = pairs
            .iter()
            .map(|pair| Ok((nibble(pair[0])? << 4) | nibble(pair[1])?))
            .collect::<Result<Vec<u8>, ProcessRosterTokenError>>()?;
        let RosterTokenV1(cursor) = serde_json::from_slice(&bytes).map_err(|_| Malformed)?;
        Ok(cursor)
    }

    pub fn after(&self) -> &ProcessId {
        &self.after
    }
    pub fn through(&self) -> &ProcessId {
        &self.through
    }
    pub fn change_cursor(&self) -> ProcessChangeCursor {
        self.change_cursor
    }

    /// Validates the issuing store, selection and retained scan fence.
    pub fn validate(
        &self,
        store: &str,
        filter: &ProcessListFilter,
        bounds: ProcessChangeBounds,
    ) -> Result<(), crate::PluginError> {
        if self.store != store {
            return Err(crate::PluginError::ProcessRegistryCursorBackendMismatch {
                expected: store.to_owned(),
                actual: self.store.clone(),
            });
        }
        if &self.filter != filter {
            return Err(crate::PluginError::ProcessRosterFilterMismatch {});
        }
        if self.change_cursor.store_sequence() < bounds.retained_after.store_sequence() {
            return Err(crate::PluginError::ProcessChangeCursorPruned {
                requested_cursor: self.change_cursor,
                tombstone_compaction_horizon: bounds.retained_after,
            });
        }
        Ok(())
    }
}

/// Store-side roster page. Enumeration is followed by changes after `change_cursor`.
/// The selection is applied before the keyset limit, including its lookahead.
#[derive(Clone, Debug, PartialEq)]
pub struct ProcessRosterRecords {
    pub records: Vec<ProcessRecord>,
    pub continuation: Option<ProcessRosterCursor>,
    pub change_cursor: ProcessChangeCursor,
    pub verified_through: ProcessChangeCursor,
}

impl ProcessRosterRecords {
    /// Builds a page from a bounded keyset read of at most `limit + 1` matching records.
    pub fn from_candidates(
        store: String,
        filter: &ProcessListFilter,
        limit: usize,
        cursor: Option<&ProcessRosterCursor>,
        through: Option<ProcessId>,
        bounds: ProcessChangeBounds,
        mut candidates: Vec<ProcessRecord>,
    ) -> Self {
        debug_assert!(
            candidates
                .iter()
                .all(|record| filter.matches_record(record))
        );
        let change_cursor = cursor.map_or(bounds.current, ProcessRosterCursor::change_cursor);
        let more = candidates.len() > limit;
        candidates.truncate(limit);
        let continuation = if more {
            candidates.last().zip(through).map(|(last, through)| {
                ProcessRosterCursor::new(
                    store,
                    filter.clone(),
                    last.id.clone(),
                    through,
                    change_cursor,
                )
            })
        } else {
            None
        };
        Self {
            records: candidates,
            continuation,
            change_cursor,
            verified_through: bounds.current,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Token admission accepts a complete position and refuses malformed,
    /// unsupported or oversized input before it can be used for a read.
    #[test]
    fn roster_tokens_refuse_malformed_and_unsupported_versions() {
        let cursor = ProcessRosterCursor::new(
            "store".into(),
            ProcessListFilter {
                definition_id: Some(crate::ProcessDefinitionId::from_sha256_digest([3; 32])),
                status: ProcessStatusFilter::Any,
                originator: Some(ProcessOriginatorFilter::session("origin")),
                until: Some(ScopeId::session("until")),
                cancel_pending_before_ms: Some(9),
                identity_kind: Some("engine".into()),
                identity_label: Some("label".into()),
                created_at_start_ms: Some(1),
                created_at_end_ms: Some(8),
                retired_since_ms: Some(5),
            },
            crate::ProcessIdMint::sequential_id_for_testing(1),
            crate::ProcessIdMint::sequential_id_for_testing(2),
            ProcessChangeCursor::from_store_sequence(7),
        );
        let token = cursor.to_token();
        assert_eq!(ProcessRosterCursor::from_token(&token), Ok(cursor));
        for token in ["", "1", "1.", "1.0", "1.gg", "v1.00", "1.7b7d"] {
            assert_eq!(
                ProcessRosterCursor::from_token(token),
                Err(ProcessRosterTokenError::Malformed)
            );
        }
        let json = serde_json::to_vec(&RosterTokenV1(
            ProcessRosterCursor::from_token(&token).expect("valid token"),
        ))
        .expect("position JSON");
        let mut position: serde_json::Value = serde_json::from_slice(&json).expect("position");
        position["filter"]["unknown"] = serde_json::json!(true);
        assert_eq!(
            ProcessRosterCursor::from_token(&token_v1(
                &serde_json::to_vec(&position).expect("JSON")
            )),
            Err(ProcessRosterTokenError::Malformed),
        );
        let deep = format!("{}0{}", "[".repeat(200), "]".repeat(200));
        assert_eq!(
            ProcessRosterCursor::from_token(&token_v1(deep.as_bytes())),
            Err(ProcessRosterTokenError::Malformed)
        );
        assert_eq!(
            ProcessRosterCursor::from_token(&token.replacen("1.", "2.", 1)),
            Err(ProcessRosterTokenError::UnsupportedVersion)
        );
        assert_eq!(
            ProcessRosterCursor::from_token(&"x".repeat(MAX_ROSTER_TOKEN_BYTES + 1)),
            Err(ProcessRosterTokenError::TooLarge)
        );
    }
}
