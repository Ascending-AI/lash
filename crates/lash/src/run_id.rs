//! The host identity of a logical Run, independent of its input kind.

/// One logical Run: a turn or operation and every physical segment it owns.
/// Its spelling is retained across restart and redrive.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct RunId(crate::TurnId);

impl RunId {
    /// Parse a retained Run identity using Lash's validated identity vocabulary.
    pub fn parse(value: impl Into<String>) -> Result<Self, crate::BlankIdentity> {
        crate::TurnId::parse(value).map(Self)
    }
    /// The retained spelling used to reattach after a restart.
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
    pub(crate) fn stored(&self) -> &crate::TurnId {
        &self.0
    }
}
impl From<crate::TurnId> for RunId {
    fn from(value: crate::TurnId) -> Self {
        Self(value)
    }
}
impl From<RunId> for crate::TurnId {
    fn from(value: RunId) -> Self {
        value.0
    }
}
impl std::fmt::Display for RunId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
