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
    pub fn fresh() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for IncarnationId {
    fn default() -> Self {
        Self::fresh()
    }
}

impl std::fmt::Display for IncarnationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for IncarnationId {
    fn from(value: String) -> Self {
        Self(value)
    }
}
