/// Durable identity for one lifetime of a host-facing session name.
///
/// Session names may be deleted and recreated. Node and effect-replay
/// identities use this store-minted value so a new lifetime cannot alias
/// retained history or journal rows from an older one.
#[derive(
    Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct IncarnationId(String);

impl IncarnationId {
    /// Mint a new durable session identity at a store implementation boundary.
    ///
    /// This constructor is public because third-party persistence backends are
    /// legitimate producers. Callers must not use it as a substitute for
    /// reading back the identity realized by
    /// [`SessionCommitStore::ensure_session_incarnation`](crate::SessionCommitStore::ensure_session_incarnation).
    pub fn mint_for_store() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    /// Decode a durable session identity read from a store implementation.
    ///
    /// Like [`Self::mint_for_store`], this is a runtime-checked trust boundary
    /// for third-party stores, not a general-purpose string constructor.
    pub fn decode_from_store(value: String) -> Self {
        Self(value)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for IncarnationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Process-local identity for one store-less runtime state.
#[derive(
    Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct EphemeralRunId(String);

impl EphemeralRunId {
    fn fresh() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Whether a runtime state has been bound to a store-realized session lifetime.
#[derive(
    Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum SessionLifetime {
    Durable(IncarnationId),
    Ephemeral(EphemeralRunId),
}

impl SessionLifetime {
    pub fn durable(incarnation_id: IncarnationId) -> Self {
        Self::Durable(incarnation_id)
    }

    pub fn as_durable(&self) -> Option<&IncarnationId> {
        match self {
            Self::Durable(incarnation_id) => Some(incarnation_id),
            Self::Ephemeral(_) => None,
        }
    }

    pub fn derivation_id(&self) -> &str {
        match self {
            Self::Durable(incarnation_id) => incarnation_id.as_str(),
            Self::Ephemeral(run_id) => run_id.as_str(),
        }
    }
}

impl Default for SessionLifetime {
    fn default() -> Self {
        Self::Ephemeral(EphemeralRunId::fresh())
    }
}

impl std::fmt::Display for SessionLifetime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.derivation_id())
    }
}
