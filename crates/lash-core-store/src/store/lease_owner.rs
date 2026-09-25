//! The identity of a durable holder: one worker or process owner and its boot
//! incarnation. Restate's process workflow names its execution with it.

use crate::ProcessId;

/// Stable identity for a lease holder.
///
/// Hosts keep `owner_id` stable for one worker or process, never one turn, and
/// assign a new `incarnation_id` on every process boot. The slack-clone example
/// is the reference shape: one constant worker owner id plus its boot-specific
/// incarnation.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LeaseOwnerIdentity {
    pub owner_id: String,
    pub incarnation_id: String,
}

impl LeaseOwnerIdentity {
    /// Constructs explicit owner and incarnation identity for store implementors; equality and
    /// fencing depend on both components, not a display-form concatenation.
    pub fn opaque(
        owner_id: impl Into<String>,
        incarnation_id: impl Into<String>,
    ) -> LeaseOwnerIdentity {
        LeaseOwnerIdentity {
            owner_id: owner_id.into(),
            incarnation_id: incarnation_id.into(),
        }
    }

    /// Stable owner identity for one engine process execution.
    ///
    /// Construction and recognition share this single representation so a
    /// formatting drift cannot silently turn a continuation into a fresh
    /// execution. The `restate:` owner-id spelling is durable: it is already
    /// written into lease rows and process start records, so it stays even
    /// though the constructor name no longer names an engine.
    pub fn engine_process_execution(
        process_id: &ProcessId,
        execution_id: impl Into<String>,
    ) -> LeaseOwnerIdentity {
        Self::opaque(format!("restate:{process_id}"), execution_id)
    }

    pub fn engine_process_execution_id(&self, process_id: &ProcessId) -> Option<&str> {
        let expected = Self::engine_process_execution(process_id, &self.incarnation_id);
        self.same_incarnation(&expected)
            .then_some(self.incarnation_id.as_str())
    }

    /// Reports the same lease incarnation to store implementors only when both owner ID and
    /// incarnation ID match exactly.
    pub fn same_incarnation(&self, other: &LeaseOwnerIdentity) -> bool {
        self.owner_id == other.owner_id && self.incarnation_id == other.incarnation_id
    }
}
