//! The Slack-compatible wire contract, shared by both binaries.
//!
//! This module is the whole point of the example's shape: the platform
//! *serializes* these types and the bot *deserializes* them, so the contract
//! between "someone else's product" and "the Lash bot living inside it" is one
//! reviewable set of structs rather than two hand-rolled JSON dialects. Field
//! names, id prefixes and pagination cursors mirror the real Slack API (see
//! `README.md` for the fidelity statement and the deliberate divergences).

pub mod events;
pub mod methods;

use serde::{Deserialize, Serialize};

/// Slack's `response_metadata` block — the only place a cursor ever appears.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ResponseMetadata {
    /// Opaque continuation token. Empty string when the page is the last one,
    /// matching Slack, which returns `""` rather than omitting the field.
    pub next_cursor: String,
}

/// Slack's uniform failure envelope: `{"ok": false, "error": "..."}`.
///
/// Slack answers *every* Web API call with HTTP 200 and signals failure in the
/// body, so integrators that only check the status code silently swallow
/// errors. The platform mirrors that (see [`crate::wire::ApiError::status`]),
/// and the bot's client checks `ok` on every response.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApiErrorBody {
    /// Always `false`.
    pub ok: bool,
    /// Slack's snake_case error code, e.g. `channel_not_found`.
    pub error: String,
    /// Present on `missing_scope`, listing the scope the call wanted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub needed: Option<String>,
    /// Present on `missing_scope`, listing the scopes the token carries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provided: Option<String>,
}

impl ApiErrorBody {
    /// Build the body for a bare error code.
    pub fn new(error: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: error.into(),
            needed: None,
            provided: None,
        }
    }
}

/// Slack's cursor encoding: base64 of `"<kind>:<value>"`.
///
/// Verified against the cursors in Slack's own reference responses —
/// `conversations.list` returns `base64("team:C061FA5PB")`, `users.list`
/// returns `base64("user:U0G9WFXNZ")`, and `conversations.history` returns
/// `base64("next_ts:1512085861000543")` (epoch microseconds, no decimal
/// point). Keeping the encoding identical means a client written against real
/// Slack cannot tell the difference, and it keeps cursors opaque to callers.
pub mod cursor {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as BASE64;

    use crate::ids::Ts;

    /// Cursor kind for `conversations.history` / `conversations.replies`.
    pub const NEXT_TS: &str = "next_ts";
    /// Cursor kind for `conversations.list`.
    pub const TEAM: &str = "team";
    /// Cursor kind for `users.list`.
    pub const USER: &str = "user";

    /// Encode `"<kind>:<value>"`.
    pub fn encode(kind: &str, value: &str) -> String {
        BASE64.encode(format!("{kind}:{value}"))
    }

    /// Decode a cursor, returning its value only when the kind matches.
    ///
    /// A cursor minted for a different method is not "empty", it is invalid —
    /// callers surface `invalid_cursor` rather than silently restarting the
    /// page walk from the top.
    pub fn decode(kind: &str, cursor: &str) -> Option<String> {
        let decoded = BASE64.decode(cursor).ok()?;
        let decoded = String::from_utf8(decoded).ok()?;
        let (found, value) = decoded.split_once(':')?;
        (found == kind).then(|| value.to_string())
    }

    /// Encode a message-timestamp cursor. Slack strips the decimal point here,
    /// so the cursor body is raw epoch microseconds.
    pub fn encode_ts(ts: Ts) -> String {
        encode(NEXT_TS, &ts.micros().to_string())
    }

    /// Decode a message-timestamp cursor back to a [`Ts`].
    pub fn decode_ts(cursor: &str) -> Option<Ts> {
        decode(NEXT_TS, cursor)?.parse().ok().map(Ts::from_micros)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::Ts;

    #[test]
    fn history_cursors_match_slacks_documented_encoding() {
        // Straight from Slack's conversations.history reference response.
        let documented = "bmV4dF90czoxNTEyMDg1ODYxMDAwNTQz";
        let ts = Ts::from_micros(1_512_085_861_000_543);
        assert_eq!(cursor::encode_ts(ts), documented);
        assert_eq!(cursor::decode_ts(documented), Some(ts));
    }

    #[test]
    fn list_cursors_match_slacks_documented_encoding() {
        assert_eq!(
            cursor::encode(cursor::TEAM, "C061FA5PB"),
            "dGVhbTpDMDYxRkE1UEI="
        );
        assert_eq!(
            cursor::encode(cursor::USER, "U0G9WFXNZ"),
            "dXNlcjpVMEc5V0ZYTlo="
        );
    }

    #[test]
    fn a_cursor_from_another_method_is_rejected_rather_than_ignored() {
        let channel_cursor = cursor::encode(cursor::TEAM, "C061FA5PB");
        assert_eq!(cursor::decode(cursor::USER, &channel_cursor), None);
        assert_eq!(cursor::decode_ts(&channel_cursor), None);
    }
}
