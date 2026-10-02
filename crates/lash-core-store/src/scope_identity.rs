//! Shared identity and storage encoding of a process lifetime scope.

use crate::SessionId;
use crate::effect_opener::EffectOpener;
use serde::{Deserialize, Serialize};

/// A scope a process may live until.
///
/// An effect opener (a logical turn root, a session operation, one process)
/// or a session. A session is never an effect-group opener:
/// it owns lifetimes, not effects.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", content = "scope", rename_all = "snake_case")]
pub enum ScopeId {
    /// An effect opener's scope.
    Opener(EffectOpener),
    /// A session's scope, closed when the session is deleted.
    Session(SessionId),
}

/// The version stamped into [`ScopeId::storage_payload`].
///
/// The payload is the authority a ledger row reads; the `(kind, id)` columns
/// beside it are only its index projection. A reader refuses any other
/// version rather than guessing at a shape it was not built for.
///
/// The payload is a [`ScopeId`] (an opener or a session). Version 2 carried
/// ADR 0094's parent scope until FIG-3607 changed it in place under the
/// pre-1.0 version freeze (FIG-3846); such a payload's scope is not a
/// [`ScopeId`], so it is refused as malformed.
///
/// version_guard(
///     roots(ScopeStoragePayload),
/// )
#[cfg(not(feature = "synthetic-next"))]
/// version_surface = "migrate"
/// format_manifest = "ScopeStoragePayload"
pub const SCOPE_STORAGE_PAYLOAD_VERSION: u16 = 2;

/// Phase A's synthetic N+1 (ADR 0115 §6) moves the surface one version on
/// with version 2's shape; its registered lift reads what N wrote.
#[cfg(feature = "synthetic-next")]
/// version_surface = "migrate"
/// format_manifest = "ScopeStoragePayload"
pub const SCOPE_STORAGE_PAYLOAD_VERSION: u16 = 3;

/// The versioned typed scope persisted beside the index projection.
#[derive(Serialize, Deserialize)]
struct ScopeStoragePayload {
    version: u16,
    scope: ScopeId,
}

/// A stored scope payload a reader refuses.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ScopeStorageError {
    /// The payload decodes but names a version this build does not read.
    #[error("unsupported scope payload version {found}")]
    UnsupportedVersion {
        /// The version found in the stored payload.
        found: u16,
    },
    /// The payload is not the versioned typed shape at all.
    #[error("malformed scope payload: {0}")]
    Malformed(String),
    /// The typed payload disagrees with the `(kind, id)` index projection
    /// stored beside it.
    #[error("scope payload does not match its index projection (kind `{kind}`, id `{id}`)")]
    ProjectionMismatch {
        /// The stored kind value.
        kind: String,
        /// The stored id value.
        id: String,
    },
}

impl ScopeId {
    /// The scope of one turn root.
    #[must_use]
    pub fn turn(session_id: impl Into<SessionId>, turn_id: impl Into<crate::TurnId>) -> Self {
        Self::Opener(EffectOpener::turn(session_id, turn_id))
    }

    /// The scope of one session operation.
    #[must_use]
    pub fn session_operation(
        session_id: impl Into<SessionId>,
        operation_id: impl Into<String>,
    ) -> Self {
        Self::Opener(EffectOpener::session_operation(session_id, operation_id))
    }

    /// The scope of one process.
    #[must_use]
    pub fn process(process_id: crate::ProcessId) -> Self {
        Self::Opener(EffectOpener::process(process_id))
    }

    /// The scope of one session.
    #[must_use]
    pub fn session(session_id: impl Into<SessionId>) -> Self {
        Self::Session(session_id.into())
    }

    /// The scope an effect opener owns.
    #[must_use]
    pub fn from_opener(opener: &EffectOpener) -> Self {
        Self::Opener(opener.clone())
    }

    /// The opener, when this is an opener's scope.
    #[must_use]
    pub fn opener(&self) -> Option<&EffectOpener> {
        match self {
            Self::Opener(opener) => Some(opener),
            Self::Session(_) => None,
        }
    }

    /// The session scope this scope lies inside: a turn's or a session
    /// operation's session. A session lies inside no other scope, and a process scope
    /// inside no session (ADR 0094 ends it through its own parent).
    ///
    /// A session's close closes every scope inside it (FIG-3948): once a
    /// session has closed it admits no root, so a turn id it never admitted
    /// can no longer become one.
    #[must_use]
    pub fn enclosing_session(&self) -> Option<Self> {
        self.opener()
            .and_then(EffectOpener::session_id)
            .map(|session_id| Self::Session(session_id.clone()))
    }

    /// Storage discriminant, as written to a `*_kind` column. The opener arms
    /// take their opener's arm name.
    #[must_use]
    pub fn storage_kind(&self) -> &'static str {
        match self {
            Self::Opener(EffectOpener::Turn { .. }) => "turn",
            Self::Opener(EffectOpener::SessionOperation { .. }) => "session_operation",
            Self::Opener(EffectOpener::Process { .. }) => "process",
            Self::Session(_) => "session",
        }
    }

    /// Index identity, as written to a `*_id` column: the canonical
    /// projection, never parsed back. An opener's is
    /// [`EffectOpener::identity_encoding`]; a session's is length-framed the
    /// same way, so no two scopes share a projection.
    #[must_use]
    pub fn storage_id(&self) -> String {
        match self {
            Self::Opener(opener) => opener.identity_encoding(),
            Self::Session(session_id) => {
                format!("session:{}:{}", session_id.as_str().len(), session_id)
            }
        }
    }

    /// The versioned typed payload stored beside the index projection.
    ///
    /// # Errors
    ///
    /// The serializer's error, which a typed scope never produces.
    pub fn storage_payload(
        &self,
        fleet_format: crate::store::FleetFormat,
    ) -> Result<String, serde_json::Error> {
        serde_json::to_string(&ScopeStoragePayload {
            version: fleet_format
                .writer_version(crate::surface_format!(SCOPE_STORAGE_PAYLOAD_VERSION))
                as u16,
            scope: self.clone(),
        })
    }

    /// Rebuilds a scope from a stored row's index projection and typed
    /// payload. The payload is the authority; the projection is accepted only
    /// as the one this build writes for the decoded scope.
    ///
    /// # Errors
    ///
    /// [`ScopeStorageError::Malformed`] for undecodable bytes (including ADR
    /// 0094's parent-scope payloads), [`ScopeStorageError::UnsupportedVersion`]
    /// for a version outside the read window `fleet_format` records (FIG-3796,
    /// ADR 0106 §2), and [`ScopeStorageError::ProjectionMismatch`] when
    /// the typed scope and the index columns disagree.
    pub fn from_storage_columns(
        kind: &str,
        id: &str,
        payload: &str,
        fleet_format: crate::store::FleetFormat,
    ) -> Result<Self, ScopeStorageError> {
        let surface = crate::surface_format!(SCOPE_STORAGE_PAYLOAD_VERSION);
        let window = fleet_format.read_window(surface);
        let mut value: serde_json::Value = serde_json::from_str(payload)
            .map_err(|error| ScopeStorageError::Malformed(error.to_string()))?;
        let found = value
            .get("version")
            .and_then(serde_json::Value::as_u64)
            .and_then(|version| u16::try_from(version).ok())
            .ok_or_else(|| {
                ScopeStorageError::Malformed("scope payload carries no u16 version".to_string())
            })?;
        if !window.admits(u32::from(found)) {
            return Err(ScopeStorageError::UnsupportedVersion { found });
        }
        if u32::from(found) != window.newest() {
            crate::store::upcast_json_record(
                "scope payload",
                surface,
                u32::from(found),
                window.newest(),
                &mut value,
            )
            .map_err(|error| ScopeStorageError::Malformed(error.to_string()))?;
        }
        let envelope: ScopeStoragePayload = serde_json::from_value(value)
            .map_err(|error| ScopeStorageError::Malformed(error.to_string()))?;
        let scope = envelope.scope;
        if scope.storage_kind() != kind || scope.storage_id() != id {
            return Err(ScopeStorageError::ProjectionMismatch {
                kind: kind.to_string(),
                id: id.to_string(),
            });
        }
        Ok(scope)
    }

    /// A diagnostic rendering.
    #[must_use]
    pub fn render(&self) -> String {
        match self {
            Self::Opener(opener) => opener.render(),
            Self::Session(session_id) => format!("session:{session_id}"),
        }
    }
}

impl std::fmt::Display for ScopeId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.render())
    }
}
