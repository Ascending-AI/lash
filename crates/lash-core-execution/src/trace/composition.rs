//! What a runtime's trace already reported of its sessions' model-facing
//! compositions.
//!
//! A durable turn opens its session's runtime afresh at the committed head,
//! so a composition reported once per change is remembered here, by the
//! trace runtime every turn on the node shares, and not by the session the
//! turn builds (FIG-5353). Forgetting is safe: a composition the memory no
//! longer holds is reported again, as a full snapshot.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use lash_protocol_types::sync::MutexExt;

use crate::SessionId;
use crate::llm::types::LlmToolSpec;

/// One fingerprint per tool contract of a request, in its order.
type ToolContractFingerprints = Arc<Vec<[u8; 32]>>;

#[derive(Default)]
pub(super) struct CompositionMemory {
    /// The latest tool lists fingerprinted, oldest first.
    tool_fingerprints: Mutex<Vec<(Arc<Vec<LlmToolSpec>>, ToolContractFingerprints)>>,
    /// Each session's last reported composition fingerprint.
    reported: Mutex<HashMap<SessionId, [u8; 32]>>,
}

impl CompositionMemory {
    /// The fingerprints of `tools`, reusing those of the latest
    /// `generations` tool lists.
    pub(super) fn tool_fingerprints(
        &self,
        tools: &Arc<Vec<LlmToolSpec>>,
        generations: usize,
    ) -> ToolContractFingerprints {
        let mut cache = self.tool_fingerprints.lock_recover();
        if let Some((_, fingerprints)) = cache.iter().find(|(cached_tools, _)| {
            Arc::ptr_eq(cached_tools, tools) || cached_tools.as_slice() == tools.as_slice()
        }) {
            return Arc::clone(fingerprints);
        }
        let fingerprints: ToolContractFingerprints = Arc::new(
            tools
                .iter()
                .map(crate::trace::composition_tool_fingerprint)
                .collect(),
        );
        if generations > 0 {
            while cache.len() >= generations {
                cache.remove(0);
            }
            cache.push((Arc::clone(tools), Arc::clone(&fingerprints)));
        }
        fingerprints
    }

    /// Whether `fingerprint` changes `session`'s reported composition, which
    /// it then becomes. At most `sessions` sessions are remembered: a full
    /// memory starts over.
    pub(super) fn report(
        &self,
        session: &SessionId,
        fingerprint: [u8; 32],
        sessions: usize,
    ) -> bool {
        let mut reported = self.reported.lock_recover();
        if reported.get(session) == Some(&fingerprint) {
            return false;
        }
        if sessions == 0 {
            return true;
        }
        if !reported.contains_key(session) && reported.len() >= sessions {
            reported.clear();
        }
        reported.insert(session.clone(), fingerprint);
        true
    }
}
