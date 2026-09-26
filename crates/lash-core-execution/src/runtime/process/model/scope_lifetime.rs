//! A process's lifetime: the scope whose close requests its cancellation, or
//! none (FIG-3607, ADR 0108).
//!
//! Three facts, kept apart:
//!
//! - **Ancestry** is where a start came from: its starter and the chain above
//!   it, ending at a session or at a root. It is recorded on the process,
//!   immutable, and never read back off a live ancestor.
//! - **Lifetime** is what ends it: `Until(scope)` asks for its cooperative
//!   cancellation when `scope` closes, and `Detached` names no scope. It is a
//!   recorded decision, taken once against the start's admitted ancestry.
//! - **Provenance** (the originator and wake target) is observation only.
//!
//! A runtime start (a model tool, `spawn_agent`, a process body) draws its
//! lifetime from a [`StartCx`] the runtime materializes from the admitted
//! scope; the model never picks one. A host, remote or trigger start is a
//! root: it has no starter, so it is `Detached` or `Until` a session the host
//! looked up.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::SessionId;
use crate::EffectOpener;

/// A scope a process may live until.
///
/// An effect opener (a logical turn root, a queued-work drain until FIG-3600
/// S8, one process) or a session. A session is never an effect-group opener:
/// it owns lifetimes, not effects.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", content = "scope", rename_all = "snake_case")]
pub enum ScopeId {
    /// An effect opener's scope.
    Opener(EffectOpener),
    /// A session's scope, closed when the session is deleted.
    Session(SessionId),
}

/// How a start came to hold a scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ScopeGrant {
    /// The scope is in the start's admitted ancestry.
    Ancestor,
    /// A host looked the session up at the facade: the one grant a root start
    /// may hold.
    HostSessionLookup,
}

/// A scope a start may name as its lifetime, together with how it was granted.
///
/// There is no `Deserialize` and no public constructor beyond the host's
/// session lookup: a runtime start takes its refs from its [`StartCx`], so a
/// ref names a scope the start was admitted under. Registration re-checks the
/// grant in every build, so an escaped ref is refused, never trusted.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ScopeRef {
    id: ScopeId,
    grant: ScopeGrant,
}

impl ScopeRef {
    fn ancestor(id: ScopeId) -> Self {
        Self {
            id,
            grant: ScopeGrant::Ancestor,
        }
    }

    /// The grant a host's session lookup mints. Only the facade calls this,
    /// after it has read the session; registration accepts it only on a root
    /// start.
    #[must_use]
    pub fn host_session_lookup(session_id: SessionId) -> Self {
        Self {
            id: ScopeId::Session(session_id),
            grant: ScopeGrant::HostSessionLookup,
        }
    }

    /// The scope this ref names.
    #[must_use]
    pub fn id(&self) -> &ScopeId {
        &self.id
    }

    /// How the ref was granted.
    #[must_use]
    pub fn grant(&self) -> ScopeGrant {
        self.grant
    }
}

/// What ends a process, as a start declares it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Lifetime {
    /// Cancellation is requested when the scope closes.
    Until(ScopeRef),
    /// No scope ends the process: it runs until it finishes, is cancelled, or
    /// is abandoned.
    Detached,
}

impl Lifetime {
    /// The recorded decision this lifetime is.
    #[must_use]
    pub fn decision(&self) -> LifetimeDecision {
        match self {
            Self::Until(scope) => LifetimeDecision::Until {
                scope: scope.id.clone(),
                grant: scope.grant,
            },
            Self::Detached => LifetimeDecision::Detached,
        }
    }
}

impl From<Lifetime> for LifetimeDecision {
    fn from(lifetime: Lifetime) -> Self {
        lifetime.decision()
    }
}

/// The concrete lifetime a start recorded: journaled with the start before it
/// registers, persisted on the process, and read back unchanged by every
/// replay and recovery (FIG-3607 R4b). A replay never re-runs the policy that
/// chose it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "lifetime", rename_all = "snake_case")]
pub enum LifetimeDecision {
    /// Cancellation is requested when `scope` closes.
    Until { scope: ScopeId, grant: ScopeGrant },
    /// No scope ends the process.
    Detached,
}

impl LifetimeDecision {
    /// The scope whose close ends the process, when it has one.
    #[must_use]
    pub fn scope(&self) -> Option<&ScopeId> {
        match self {
            Self::Until { scope, .. } => Some(scope),
            Self::Detached => None,
        }
    }

    /// Storage discriminant, as written to the `lifetime` column.
    #[must_use]
    pub fn storage_label(&self) -> &'static str {
        match self {
            Self::Until { .. } => "until",
            Self::Detached => "detached",
        }
    }
}

/// Where a start came from: its starter, then the chain above it, nearest
/// first, ending at a session or at a root. Empty for a root start.
///
/// Recorded on the process, immutable, and retained independently of the
/// ancestors' own records, so a start context is rebuilt after an
/// intermediate ancestor is pruned (FIG-3607 R1).
#[derive(
    Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(transparent)]
pub struct Ancestry(Vec<ScopeId>);

impl Ancestry {
    /// A root start's ancestry.
    #[must_use]
    pub fn root() -> Self {
        Self(Vec::new())
    }

    /// An ancestry as recorded data carries it (a stored record or a wire
    /// envelope), nearest first. Registration checks a start's lifetime
    /// against the ancestry its realization recorded, never one an author
    /// supplied (FIG-3607 R3).
    #[must_use]
    pub fn from_scopes(scopes: impl IntoIterator<Item = ScopeId>) -> Self {
        Self(scopes.into_iter().collect())
    }

    /// The scopes, nearest first.
    #[must_use]
    pub fn scopes(&self) -> &[ScopeId] {
        &self.0
    }

    /// The scope that started the process; `None` for a root.
    #[must_use]
    pub fn starter(&self) -> Option<&ScopeId> {
        self.0.first()
    }

    /// Whether this is a root start's ancestry.
    #[must_use]
    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    /// Whether `scope` is in this ancestry.
    #[must_use]
    pub fn contains(&self, scope: &ScopeId) -> bool {
        self.0.contains(scope)
    }
}

/// What a process hands the starts made inside it: its own scope and its
/// ancestry, and the session capability its descendants inherit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessLineage {
    ancestry: Ancestry,
    session: Option<SessionId>,
}

impl ProcessLineage {
    /// The lineage a process's body starts children under.
    ///
    /// Its scopes are the process itself, then the session it runs of its own
    /// (a `SessionTurn` child session: a subagent's own session replaces the
    /// inherited one, R10), then its recorded ancestry, ending at its session
    /// capability's scope. The session capability is the process's own
    /// session when it has one, and otherwise the one it inherited; a
    /// standalone process has none.
    #[must_use]
    pub fn of_process(
        process_id: &crate::ProcessId,
        ancestry: &Ancestry,
        session_capability: Option<&SessionId>,
        own_session: Option<&SessionId>,
    ) -> Self {
        let mut scopes = vec![ScopeId::process(process_id.clone())];
        if let Some(own) = own_session {
            scopes.push(ScopeId::Session(own.clone()));
        }
        for scope in ancestry.scopes() {
            if !scopes.contains(scope) {
                scopes.push(scope.clone());
            }
        }
        let session = own_session.or(session_capability).cloned();
        if let Some(session) = session.as_ref() {
            let scope = ScopeId::Session(session.clone());
            if !scopes.contains(&scope) {
                scopes.push(scope);
            }
        }
        Self {
            ancestry: Ancestry(scopes),
            session,
        }
    }

    /// The ancestry of a start made under this lineage.
    #[must_use]
    pub fn ancestry(&self) -> &Ancestry {
        &self.ancestry
    }

    /// The session capability a start made under this lineage inherits.
    #[must_use]
    pub fn session(&self) -> Option<&SessionId> {
        self.session.as_ref()
    }
}

/// The context a runtime start draws its lifetime from, materialized once per
/// admission from the admitted scope and the recorded lineage it runs under,
/// with no live reads (FIG-3607 R2).
///
/// Host, remote and trigger starts are roots and have none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartCx {
    starter: ScopeRef,
    ancestors: Vec<ScopeRef>,
    session: Option<ScopeRef>,
}

/// Why a start context could not be materialized.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum StartCxError {
    /// The admitted scope names no opener (a session delete or a runtime
    /// operation starts no processes).
    #[error(transparent)]
    NotAnOpener(#[from] crate::EffectOpenerError),
    /// A process scope ran without the lineage of the process it names.
    #[error("process scope `{process_id}` runs without its recorded lineage")]
    MissingLineage { process_id: crate::ProcessId },
}

impl StartCx {
    /// The context of a start admitted under `scope`.
    ///
    /// - A turn or queued drain of session `s` starts with the opener, then
    ///   `Session(s)`, then, when `s` is owned by a process, that process's
    ///   lineage; the session capability is `s`.
    /// - A process opener's context is the lineage the process runs under,
    ///   which must name that process.
    ///
    /// # Errors
    ///
    /// [`StartCxError::NotAnOpener`] for an administrative scope, and
    /// [`StartCxError::MissingLineage`] for a process scope without the
    /// lineage of the process it names.
    pub fn materialize(
        scope: &crate::AdmittedScope,
        lineage: Option<&ProcessLineage>,
    ) -> Result<Self, StartCxError> {
        let opener = EffectOpener::for_scope(scope)?;
        match &opener {
            EffectOpener::Process { process_id } => {
                let Some(lineage) = lineage.filter(|lineage| {
                    lineage.ancestry.starter() == Some(&ScopeId::process(process_id.clone()))
                }) else {
                    return Err(StartCxError::MissingLineage {
                        process_id: process_id.clone(),
                    });
                };
                Ok(Self::from_scopes(
                    ScopeId::process(process_id.clone()),
                    lineage.ancestry.scopes().get(1..).unwrap_or(&[]).to_vec(),
                    lineage.session.clone(),
                ))
            }
            EffectOpener::Turn { session_id, .. } | EffectOpener::QueueDrain { session_id, .. } => {
                let session_id = session_id.clone();
                let starter = ScopeId::Opener(opener.clone());
                let mut above = vec![ScopeId::Session(session_id.clone())];
                if let Some(lineage) = lineage {
                    for scope in lineage.ancestry.scopes() {
                        if !above.contains(scope) && *scope != starter {
                            above.push(scope.clone());
                        }
                    }
                }
                Ok(Self::from_scopes(starter, above, Some(session_id)))
            }
        }
    }

    fn from_scopes(starter: ScopeId, above: Vec<ScopeId>, session: Option<SessionId>) -> Self {
        let starter = ScopeRef::ancestor(starter);
        let session = session.map(|session| ScopeRef::ancestor(ScopeId::Session(session)));
        let ancestors = std::iter::once(starter.clone())
            .chain(above.into_iter().map(ScopeRef::ancestor))
            .collect();
        Self {
            starter,
            ancestors,
            session,
        }
    }

    /// The scope that starts the child: the admitted opener.
    #[must_use]
    pub fn starter(&self) -> ScopeRef {
        self.starter.clone()
    }

    /// The session capability the start may bind to, when it has one. A
    /// standalone process tree has none.
    #[must_use]
    pub fn session(&self) -> Option<ScopeRef> {
        self.session.clone()
    }

    /// The ancestors, nearest first, including the starter, ending at the
    /// session or at the root.
    #[must_use]
    pub fn ancestors(&self) -> &[ScopeRef] {
        &self.ancestors
    }

    /// The ancestry a start made in this context records.
    #[must_use]
    pub fn ancestry(&self) -> Ancestry {
        Ancestry(
            self.ancestors
                .iter()
                .map(|scope| scope.id.clone())
                .collect(),
        )
    }

    /// The session capability a start made in this context records.
    #[must_use]
    pub fn session_capability(&self) -> Option<SessionId> {
        self.session.as_ref().and_then(|scope| match &scope.id {
            ScopeId::Session(session) => Some(session.clone()),
            ScopeId::Opener(_) => None,
        })
    }
}

/// How a start-issuing plugin chooses a child's lifetime: a required
/// constructor argument with no default, resolved once against the admitted
/// [`StartCx`] and recorded (FIG-3607 R4b).
pub type LifetimePolicy = Arc<dyn Fn(&StartCx) -> Lifetime + Send + Sync>;

/// The named lifetime policies.
pub mod lifetime {
    use super::{Lifetime, LifetimePolicy, StartCx};
    use std::sync::Arc;

    /// Until the start's session closes, or its starter ends when it has no
    /// session (a standalone process tree).
    #[must_use]
    pub fn session_or_starter(cx: &StartCx) -> Lifetime {
        Lifetime::Until(cx.session().unwrap_or_else(|| cx.starter()))
    }

    /// Until the start's starter ends.
    #[must_use]
    pub fn starter(cx: &StartCx) -> Lifetime {
        Lifetime::Until(cx.starter())
    }

    /// No scope ends the child.
    #[must_use]
    pub fn detached(_cx: &StartCx) -> Lifetime {
        Lifetime::Detached
    }

    /// A policy from one of the named functions (or any other).
    #[must_use]
    pub fn policy(choose: fn(&StartCx) -> Lifetime) -> LifetimePolicy {
        Arc::new(choose)
    }
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
pub const SCOPE_STORAGE_PAYLOAD_VERSION: u16 = 2;

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

    /// The scope of one queued-work drain.
    #[must_use]
    pub fn queue_drain(session_id: impl Into<SessionId>, drain_id: impl Into<String>) -> Self {
        Self::Opener(EffectOpener::queue_drain(session_id, drain_id))
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

    /// Storage discriminant, as written to a `*_kind` column. The opener arms
    /// take their opener's arm name.
    #[must_use]
    pub fn storage_kind(&self) -> &'static str {
        match self {
            Self::Opener(EffectOpener::Turn { .. }) => "turn",
            Self::Opener(EffectOpener::QueueDrain { .. }) => "queue_drain",
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
        fleet_format: crate::FleetFormat,
    ) -> Result<String, serde_json::Error> {
        serde_json::to_string(&ScopeStoragePayload {
            version: fleet_format.writer_version(lash_core_store::surface_format!(
                SCOPE_STORAGE_PAYLOAD_VERSION
            )) as u16,
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
        fleet_format: crate::FleetFormat,
    ) -> Result<Self, ScopeStorageError> {
        let surface = lash_core_store::surface_format!(SCOPE_STORAGE_PAYLOAD_VERSION);
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
            lash_core_store::store::upcast_json_record(
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

#[cfg(test)]
mod tests {
    use super::*;

    fn turn_scope() -> ScopeId {
        ScopeId::turn(SessionId::from("s"), crate::TurnId::from("t"))
    }

    fn process_scope() -> ScopeId {
        ScopeId::process(crate::process_id_for_test("worker"))
    }

    #[test]
    fn a_scope_round_trips_through_its_versioned_payload() {
        for scope in [
            turn_scope(),
            process_scope(),
            ScopeId::queue_drain(SessionId::from("s"), "d"),
            ScopeId::session("s"),
        ] {
            let payload = scope
                .storage_payload(crate::FleetFormat::current())
                .expect("encode the payload");
            let decoded = ScopeId::from_storage_columns(
                scope.storage_kind(),
                &scope.storage_id(),
                &payload,
                crate::FleetFormat::current(),
            )
            .expect("the payload is the authority the projection agrees with");
            assert_eq!(decoded, scope);
        }
    }

    /// `("s/a","c")` and `("s","a/c")` share no projection, and a session
    /// named like a turn's encoding does not alias it.
    #[test]
    fn scopes_that_render_alike_have_distinct_projections() {
        let first = ScopeId::turn(SessionId::from("s/a"), crate::TurnId::from("c"));
        let second = ScopeId::turn(SessionId::from("s"), crate::TurnId::from("a/c"));
        assert_ne!(first.storage_id(), second.storage_id());
        let turn = ScopeId::turn(SessionId::from("s"), crate::TurnId::from("t"));
        let session = ScopeId::session(turn.storage_id());
        assert_ne!(turn.storage_id(), session.storage_id());
    }

    /// ADR 0094's parent-scope payload is refused, never reinterpreted as a
    /// scope.
    #[test]
    fn a_parent_scope_payload_is_refused() {
        let old = serde_json::json!({
            "version": 2,
            "scope": { "kind": "owned", "opener": { "kind": "turn", "session_id": "s", "turn_id": "t" } },
        })
        .to_string();
        let error = ScopeId::from_storage_columns(
            "turn",
            "turn:1:s:1:t",
            &old,
            crate::FleetFormat::current(),
        )
        .expect_err("an old payload must not decode");
        assert!(
            matches!(error, ScopeStorageError::Malformed(_)),
            "an old payload is malformed: {error:?}"
        );
    }

    #[test]
    fn a_payload_that_disagrees_with_its_projection_is_refused() {
        let payload = turn_scope()
            .storage_payload(crate::FleetFormat::current())
            .expect("encode a turn");
        let error = ScopeId::from_storage_columns(
            "turn",
            &process_scope().storage_id(),
            &payload,
            crate::FleetFormat::current(),
        )
        .expect_err("a mismatched projection must refuse");
        assert!(
            matches!(error, ScopeStorageError::ProjectionMismatch { .. }),
            "{error}"
        );
    }

    fn turn_admitted() -> crate::AdmittedScope {
        crate::AdmittedScope::turn(SessionId::from("s"), crate::TurnId::from("root"))
    }

    /// A turn's context starts at its root, ends at its session, and holds
    /// that session as its capability.
    #[test]
    fn a_turn_start_context_ends_at_its_session() {
        let cx = StartCx::materialize(&turn_admitted(), None).expect("a turn is an opener");
        assert_eq!(cx.starter().id(), &turn_scope_root());
        assert_eq!(
            cx.ancestry().scopes(),
            &[turn_scope_root(), ScopeId::session("s")]
        );
        assert_eq!(cx.session_capability(), Some(SessionId::from("s")));
        assert_eq!(
            lifetime::session_or_starter(&cx).decision(),
            LifetimeDecision::Until {
                scope: ScopeId::session("s"),
                grant: ScopeGrant::Ancestor,
            }
        );
        assert_eq!(
            lifetime::starter(&cx).decision().scope(),
            Some(&turn_scope_root())
        );
    }

    fn turn_scope_root() -> ScopeId {
        ScopeId::turn(SessionId::from("s"), crate::TurnId::from("root"))
    }

    /// A process body's context is its lineage; a standalone process has no
    /// session, so `session_or_starter` binds to the process itself.
    #[test]
    fn a_standalone_process_context_has_no_session() {
        let process = crate::process_id_for_test("standalone");
        let lineage = ProcessLineage::of_process(&process, &Ancestry::root(), None, None);
        let scope = crate::AdmittedScope::process(process.clone());
        let cx = StartCx::materialize(&scope, Some(&lineage)).expect("lineage names the process");
        assert_eq!(cx.session(), None);
        assert_eq!(cx.ancestry().scopes(), &[ScopeId::process(process.clone())]);
        assert_eq!(
            lifetime::session_or_starter(&cx).decision().scope(),
            Some(&ScopeId::process(process))
        );
    }

    /// A child of a turn-started process inherits the process's ancestry, so
    /// its context names the turn and the session after the process.
    #[test]
    fn a_process_lineage_carries_its_recorded_ancestry() {
        let process = crate::process_id_for_test("child");
        let parent_cx = StartCx::materialize(&turn_admitted(), None).expect("turn");
        let lineage = ProcessLineage::of_process(
            &process,
            &parent_cx.ancestry(),
            parent_cx.session_capability().as_ref(),
            None,
        );
        let cx = StartCx::materialize(
            &crate::AdmittedScope::process(process.clone()),
            Some(&lineage),
        )
        .expect("lineage names the process");
        assert_eq!(
            cx.ancestry().scopes(),
            &[
                ScopeId::process(process),
                turn_scope_root(),
                ScopeId::session("s")
            ]
        );
        assert_eq!(cx.session_capability(), Some(SessionId::from("s")));
    }

    /// A subagent's own session replaces the inherited one (R10).
    #[test]
    fn a_subagent_session_replaces_the_inherited_session() {
        let process = crate::process_id_for_test("subagent");
        let own = SessionId::from("session:process:subagent");
        let parent_cx = StartCx::materialize(&turn_admitted(), None).expect("turn");
        let lineage = ProcessLineage::of_process(
            &process,
            &parent_cx.ancestry(),
            parent_cx.session_capability().as_ref(),
            Some(&own),
        );
        assert_eq!(lineage.session(), Some(&own));
        assert_eq!(
            lineage.ancestry().scopes(),
            &[
                ScopeId::process(process),
                ScopeId::Session(own),
                turn_scope_root(),
                ScopeId::session("s")
            ]
        );
    }

    /// A process scope without its lineage refuses rather than inventing a
    /// root context.
    #[test]
    fn a_process_scope_without_its_lineage_is_refused() {
        let process = crate::process_id_for_test("orphan");
        let error = StartCx::materialize(&crate::AdmittedScope::process(process.clone()), None)
            .expect_err("no lineage");
        assert_eq!(
            error,
            StartCxError::MissingLineage {
                process_id: process
            }
        );
    }
}
