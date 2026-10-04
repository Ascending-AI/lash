//! Scrubbed HTTP occurrences, matched in their recorded order.

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A transcript never contains credentials. Dynamic request facts must be
/// bound by the scenario before boot; matching has no wildcard or fallback.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpTranscript {
    pub name: String,
    pub occurrences: Vec<HttpOccurrence>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpOccurrence {
    pub identity: String,
    pub method: String,
    pub path: String,
    pub body: Value,
    pub response: RecordedResponse,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordedResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    /// Raw wire bytes, including SSE delimiters. Chunk boundaries may split
    /// an SSE event: the production client must perform its own framing.
    pub chunks: Vec<ResponseChunk>,
    pub termination: StreamEnd,
    /// Expected wire usage/failure, reconciled by the scenario with the
    /// production client's typed result and the committed Run output.
    pub usage: Value,
    pub typed_failure: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseChunk {
    pub bytes: String,
    /// A transport barrier after these bytes have been written. This is
    /// evidence of socket delivery, never of X/D/V journal durability.
    pub hold_after: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamEnd {
    Complete,
    /// Close without the terminating HTTP chunk, yielding a real read error.
    Disconnect,
}

impl HttpTranscript {
    pub fn from_json(bytes: &[u8]) -> Result<Self> {
        let transcript: Self = serde_json::from_slice(bytes)?;
        transcript.validate()?;
        Ok(transcript)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(!self.name.is_empty(), "transcript has no name");
        ensure!(
            !self.occurrences.is_empty(),
            "empty transcript selects nothing"
        );
        let mut identities = std::collections::BTreeSet::new();
        let mut barriers = std::collections::BTreeSet::new();
        for occurrence in &self.occurrences {
            ensure!(
                !occurrence.identity.is_empty() && identities.insert(&occurrence.identity),
                "missing or duplicate occurrence identity"
            );
            ensure!(
                occurrence.method == "POST" || occurrence.method == "GET",
                "unsupported transcript method"
            );
            ensure!(
                occurrence.path.starts_with('/')
                    && !occurrence.path.chars().any(char::is_whitespace),
                "invalid transcript path"
            );
            ensure!(
                (200..=599).contains(&occurrence.response.status),
                "invalid transcript status"
            );
            for (name, value) in &occurrence.response.headers {
                ensure!(
                    !name.is_empty()
                        && name
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                        && !value.contains(['\r', '\n']),
                    "invalid response header"
                );
                ensure!(
                    !matches!(
                        name.to_ascii_lowercase().as_str(),
                        "content-length" | "transfer-encoding" | "connection" | "set-cookie"
                    ),
                    "fixture owns framing and contains no cookies"
                );
            }
            for chunk in &occurrence.response.chunks {
                ensure!(!chunk.bytes.is_empty(), "empty HTTP data chunk");
                if let Some(barrier) = &chunk.hold_after {
                    ensure!(
                        !barrier.is_empty() && barriers.insert(barrier),
                        "missing or duplicate transport barrier"
                    );
                }
            }
        }
        Ok(())
    }
}
