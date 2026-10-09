//! The stable identity of a lease holder: a worker process and its boot.

use crate::ProcessId;

/// Stable name of a lease holder: a node name or an engine process owner.
///
/// Keep this name stable across boots and distinct for holders serving together.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeaseOwnerId(String);

impl LeaseOwnerId {
    /// Names the worker or process that owns the lease.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

/// Identity of one boot or engine process execution of a lease holder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeaseIncarnationId(String);

impl LeaseIncarnationId {
    /// Identifies this boot or execution, independently of its stable owner name.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

/// Stable identity for a lease holder.
///
/// Hosts keep `owner_id` stable for one worker or process, never one turn, and
/// assign a new `incarnation_id` on every process boot: one constant worker
/// owner id plus its boot-specific incarnation.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LeaseOwnerIdentity {
    pub owner_id: String,
    pub incarnation_id: String,
}

impl LeaseOwnerIdentity {
    /// Constructs an identity from the stable owner name and its boot incarnation.
    ///
    /// Distinct argument types prevent a node name from being used as its incarnation.
    /// Equality and fencing depend on both stored components.
    pub fn opaque(
        owner_id: LeaseOwnerId,
        incarnation_id: LeaseIncarnationId,
    ) -> LeaseOwnerIdentity {
        LeaseOwnerIdentity {
            owner_id: owner_id.0,
            incarnation_id: incarnation_id.0,
        }
    }

    /// Stable owner identity for one engine process execution.
    ///
    /// Construction and recognition share this single representation so a
    /// formatting drift cannot silently turn a continuation into a fresh
    /// execution. The `process:` owner-id spelling is durable: it is written
    /// into lease rows and process start records.
    pub fn engine_process_execution(
        process_id: &ProcessId,
        execution_id: LeaseIncarnationId,
    ) -> LeaseOwnerIdentity {
        Self::opaque(
            LeaseOwnerId::new(format!("process:{process_id}")),
            execution_id,
        )
    }

    pub fn engine_process_execution_id(&self, process_id: &ProcessId) -> Option<&str> {
        let expected = Self::engine_process_execution(
            process_id,
            LeaseIncarnationId::new(&self.incarnation_id),
        );
        self.same_incarnation(&expected)
            .then_some(self.incarnation_id.as_str())
    }

    /// Reports the same lease incarnation to store implementors only when both owner ID and
    /// incarnation ID match exactly.
    pub fn same_incarnation(&self, other: &LeaseOwnerIdentity) -> bool {
        self.owner_id == other.owner_id && self.incarnation_id == other.incarnation_id
    }
}
