//! Provider-facing cache and affinity identities derived from a request
//! scope (FIG-3534).
//!
//! Host-minted session and continuation ids can carry tenant or user
//! identifiers, and the wire formats that take them (`prompt_cache_key`,
//! affinity `session_id`, Codex `session-id` headers, Google `sessionId`)
//! only need a *stable* key. Everything provider-facing therefore carries the
//! lowercase hex domain hash, never the readable id, so affinity survives
//! across requests and processes without disclosing what the host put in the
//! id — and two ids can never collide by sharing a prefix.

use super::types::{LlmRequest, LlmRequestScope};
use crate::core_support::blake3_domain_hash_hex;

impl LlmRequestScope {
    /// Provider-facing affinity identity for this session: the lowercase hex
    /// domain hash of the session id.
    pub fn provider_session_affinity_key(&self) -> String {
        blake3_domain_hash_hex(
            "lash-provider-session-affinity/v1",
            self.session_id.as_str(),
        )
    }

    /// Provider-facing prompt-cache key for this continuation: the lowercase
    /// hex domain hash of [`Self::continuation_key`]. Same rationale as
    /// [`Self::provider_session_affinity_key`].
    pub fn provider_prompt_cache_key(&self) -> String {
        blake3_domain_hash_hex("lash-provider-prompt-cache-key/v1", self.continuation_key())
    }
}

impl LlmRequest {
    /// See [`LlmRequestScope::provider_session_affinity_key`].
    pub fn provider_session_affinity_key(&self) -> String {
        self.scope.provider_session_affinity_key()
    }

    /// See [`LlmRequestScope::provider_prompt_cache_key`].
    pub fn provider_prompt_cache_key(&self) -> String {
        self.scope.provider_prompt_cache_key()
    }
}
