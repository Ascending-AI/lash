//! Scope binding identity for durable turn-cancellation authority.

use crate::RuntimeError;
use serde::{Deserialize, Serialize};
const PHYSICAL_SCOPE_BINDING_SEPARATOR: &str = "#lash-physical-scope:";
/// Bind a durable cancellation authority to the non-session physical scope
/// that owns its journal. Turn/session scopes are already tied to their
/// address and keep the deployment identity unchanged.
pub fn turn_control_binding_id_for_scope(
    base: &str,
    scope: &crate::ExecutionScope,
) -> Result<String, RuntimeError> {
    match scope.journal_identity() {
        Ok(identity) if scope.session_id().is_none() => Ok(format!(
            "{base}{PHYSICAL_SCOPE_BINDING_SEPARATOR}{}",
            identity.key()
        )),
        Ok(_) => Ok(base.to_string()),
        Err(error) => Err(error.into()),
    }
}
pub fn binding_id_admits_scope(binding_id: &str, scope: &crate::ExecutionScope) -> bool {
    match scope.journal_identity() {
        Ok(identity) if scope.session_id().is_none() => binding_id.ends_with(&format!(
            "{PHYSICAL_SCOPE_BINDING_SEPARATOR}{}",
            identity.key()
        )),
        Ok(_) => !binding_id.contains(PHYSICAL_SCOPE_BINDING_SEPARATOR),
        Err(_) => false,
    }
}
/// Select the scope persisted with a turn-closure authorization.
///
/// Session-bound controllers may be driving a queue drain or another turn when
/// they discover an orphan. The durable input row's turn address is the
/// canonical admission identity in that case. Process and runtime-operation
/// controllers with a journal-bound cancellation authority instead carry the physical identity selected before
/// session work began, so recovery must preserve it exactly. Store-owned Native
/// promises use the turn address even when ordinary effects run in an operation scope.
pub fn admitted_turn_cancel_scope(
    address: &crate::TurnAddress,
    controller_scope: &crate::ExecutionScope,
    binding_id: &str,
) -> crate::ExecutionScope {
    if controller_scope.session_id().is_some()
        || !binding_id.contains(PHYSICAL_SCOPE_BINDING_SEPARATOR)
    {
        address.execution_scope()
    } else {
        controller_scope.clone()
    }
}

/// The durable identity of the turn-cancellation authority that may cancel one
/// unit of admitted work.
///
/// This is the value [`turn_control_binding_id_for_scope`] mints and
/// [`binding_id_admits_scope`] checks: the authority's deployment base, plus
/// the physical scope suffix when the owning scope has no session. It is the
/// address the cooperative cancel path signals and the address FIG-3409's
/// cancel disposition is fenced on.
///
/// A newtype rather than a bare `String` because it is retained in a durable
/// shape (ADR 0099 §3's tool-child request) and a frozen format may not carry
/// an unvalidated string: an empty or whitespace binding would name an
/// authority that cannot exist, and would be discovered at the moment a
/// recovered child tried to honour a cancellation rather than when it was
/// written. Live in-process plumbing still passes `&str`; this type is the
/// boundary where the value becomes a durable fact.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct TurnControlBindingId(String);

impl TurnControlBindingId {
    /// Refuses only what cannot be an authority at all. The structure above the
    /// separator is the minting function's, not this type's — re-deriving it
    /// here would be a second copy of
    /// [`turn_control_binding_id_for_scope`]'s rule, free to disagree with it.
    pub fn new(value: impl Into<String>) -> Result<Self, TurnControlBindingIdError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(TurnControlBindingIdError::Empty);
        }
        Ok(Self(value))
    }

    /// Borrows the identity as the `&str` the live plumbing takes.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for TurnControlBindingId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

impl<'de> Deserialize<'de> for TurnControlBindingId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

/// Rejection produced when constructing a [`TurnControlBindingId`].
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum TurnControlBindingIdError {
    /// An authority with no identity cannot be addressed.
    #[error("turn-control binding id must not be empty")]
    Empty,
}

#[cfg(test)]
mod binding_id_tests {
    use super::*;

    #[test]
    fn a_binding_id_round_trips_and_refuses_an_empty_authority() {
        let id = TurnControlBindingId::new("authority-1").expect("a valid binding id");
        let json = serde_json::to_string(&id).expect("serializes");
        assert_eq!(json, "\"authority-1\"");
        assert_eq!(
            serde_json::from_str::<TurnControlBindingId>(&json).expect("decodes"),
            id
        );
        assert_eq!(
            TurnControlBindingId::new("  ").expect_err("blank is refused"),
            TurnControlBindingIdError::Empty
        );
        assert!(
            serde_json::from_str::<TurnControlBindingId>("\"\"").is_err(),
            "decoding must refuse what the constructor refuses"
        );
    }

    /// The physical-scope suffix the minting function adds survives the
    /// newtype, so `binding_id_admits_scope` still reads what it wrote.
    #[test]
    fn a_physical_scope_binding_survives_the_newtype() {
        let scope = crate::ExecutionScope::runtime_operation("op-1");
        let minted = turn_control_binding_id_for_scope("base", &scope).expect("mints");
        let typed = TurnControlBindingId::new(minted).expect("a valid binding id");
        assert!(binding_id_admits_scope(typed.as_str(), &scope));
    }
}
