//! The logical opener of durable work: a turn, or one process incarnation
//! (ADR 0099 §1).
//!
//! # Why this is not an `ExecutionScope`
//!
//! ADR 0099 §1 states the identity plainly: "An opener is `Turn(session_id,
//! turn_id)` or `Process(ProcessRef { process_id, incarnation })`. Its identity
//! is stable across worker attempts and segments, and it changes on process
//! re-registration. Group identity, retained authority, cancellation, usage
//! attribution and retirement all bind that exact opener. A retired or
//! mismatched incarnation is refused, never rebound to the current process
//! carrying the same name."
//!
//! [`ExecutionScope`](crate::ExecutionScope) cannot express that.
//! `ExecutionScope::Process` carries `process_id` alone
//! (`crates/lash-sansio/src/effect_identity.rs`), while
//! [`ProcessRef`] exists precisely to "Pin a reusable process name to one
//! store-minted incarnation". A scope is therefore a *claim address* — the
//! right thing to fence a journal row on — and not an opener identity. Retaining
//! a scope where the contract calls for an opener leaves recovery with nothing
//! to validate, and lets a process re-registered under the same name alias its
//! predecessor's groups, closes and cancellation fences.
//!
//! # Why it is an enum and not a rendered string
//!
//! Because the two arms can spell each other. A turn's scope identity is a
//! free-form string — an effect graph key, or a host-chosen session id when the
//! work runs outside an effect — so it can contain exactly the
//! `{process_id}#{incarnation}` text a process opener renders to. An untagged
//! rendering therefore admits two distinct openers that mint one identity, which
//! is the aliasing §1 refuses. Serialized here with an explicit `kind` tag, and
//! compared as a value rather than as text, that collision is unrepresentable.
//!
//! Two projections of the value exist, and they are not interchangeable.
//! [`render`](EffectOpener::render) is the *diagnostic* one — readable, and
//! deliberately free-form: its `:`-joined components mean
//! `Turn("a:b", "c")` and `Turn("a", "b:c")` render to the same text.
//! [`identity_encoding`](EffectOpener::identity_encoding) is the *canonical*
//! one — every component length-prefixed, so distinct openers always encode
//! distinctly. Anything a key preimage or an embedded id consumes must take
//! the encoding; the rendering is for humans. Neither is the identity itself:
//! nothing parses either back.

use serde::{Deserialize, Serialize};

use crate::admitted_scope::AdmittedScope;
use crate::process_identity::ProcessRef;
use crate::{SessionId, TurnId};

/// The exact logical opener that durable work binds (ADR 0099 §1).
///
/// Shared vocabulary rather than a per-lane spelling: the retained tool-child
/// request (FIG-3408) records it, recovery validates it (FIG-3396 §1), and the
/// Lashlang host bridges mint their identities under it (FIG-3394). One type so
/// the three cannot disagree about what an opener is.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EffectOpener {
    /// A turn of a session.
    Turn {
        /// The session the turn belongs to.
        session_id: SessionId,
        /// The turn's durable identity.
        turn_id: TurnId,
    },
    /// One drain of a session's queued work.
    ///
    /// A drain is a lifecycle owner in exactly the sense §1 means: it is
    /// durable and retry-stable. `drain_id` is the host's idempotency key for
    /// a retried drain, and `crates/lash/src/turn.rs` makes it the *alternative*
    /// to a turn id for identifying one physical unit — "keep `drain_id(...)`
    /// as the durable idempotency key for retried drains, or keep
    /// `turn_id(...)` as the host-minted physical turn identity" — resolving
    /// the execution scope to `queue_drain_scope(session, drain_id)` when no
    /// turn id exists. One drain may run several queued turns, and the opener
    /// lives until the drain ends, not until the first turn does.
    QueueDrain {
        /// The session whose queue is being drained.
        session_id: SessionId,
        /// The drain's durable identity. A plain string because no typed id
        /// exists for it; empty is refused, as `ExecutionScope::validate`
        /// refuses an empty scope id.
        drain_id: String,
    },
    /// One process incarnation. The reusable name alone is not the opener.
    Process {
        /// The name bound to the store-minted incarnation that owns this work.
        process_ref: ProcessRef,
    },
}

impl EffectOpener {
    /// The opener of one turn.
    #[must_use]
    pub fn turn(session_id: impl Into<SessionId>, turn_id: impl Into<TurnId>) -> Self {
        Self::Turn {
            session_id: session_id.into(),
            turn_id: turn_id.into(),
        }
    }

    /// The opener of one queued-work drain.
    #[must_use]
    pub fn queue_drain(session_id: impl Into<SessionId>, drain_id: impl Into<String>) -> Self {
        Self::QueueDrain {
            session_id: session_id.into(),
            drain_id: drain_id.into(),
        }
    }

    /// The opener of one process incarnation.
    #[must_use]
    pub fn process(process_ref: ProcessRef) -> Self {
        Self::Process { process_ref }
    }

    /// The session this opener attributes its work to, when it has one.
    ///
    /// A process opener answers `None`: a process is not owned by the session
    /// that registered it, and ADR 0094 governs its lifetime through its own
    /// Parent Scope. Callers that need a session for a process opener carry it
    /// separately and say so.
    #[must_use]
    pub fn session_id(&self) -> Option<&SessionId> {
        match self {
            Self::Turn { session_id, .. } | Self::QueueDrain { session_id, .. } => Some(session_id),
            Self::Process { .. } => None,
        }
    }

    /// The process incarnation this opener is, when it is one.
    #[must_use]
    pub fn process_ref(&self) -> Option<&ProcessRef> {
        match self {
            Self::Turn { .. } | Self::QueueDrain { .. } => None,
            Self::Process { process_ref } => Some(process_ref),
        }
    }

    /// A kind-tagged rendering, for diagnostics.
    ///
    /// Tagged because the arms can otherwise spell each other (see the module
    /// documentation), but *not* component-framed: the components inside an
    /// arm are joined with `:` and are themselves free-form, so
    /// `Turn("a:b", "c")` and `Turn("a", "b:c")` render identically. That is
    /// fine for a diagnostic and fatal for a key preimage — anything minted
    /// must use [`identity_encoding`](EffectOpener::identity_encoding), which
    /// carries unambiguous component boundaries. Nothing parses either
    /// projection back into an `EffectOpener`; the value is the identity.
    ///
    /// # Why the separator is `:` and not `#`
    ///
    /// A rendered opener is embedded in identities that are themselves
    /// embedded in identities. The Lashlang host bridges mint a tool-call id
    /// under the opener (FIG-3394), and the subagent spawn tool then builds a
    /// child's `SessionId` and `ProcessId` out of that call id verbatim
    /// (`crates/lash-subagents/src/rlm.rs`). `#` and `/` are both reserved
    /// there: `invalid_process_key_reason`
    /// (`crates/lash-core-store/src/store/process_key.rs`) refuses any process
    /// id containing `#` as a "reserved segment separator", and ADR 0094's
    /// retired `ParentScope` storage codec split a stored turn scope on `/`
    /// and a stored process scope on `#` — the shape FIG-3418 replaced with
    /// [`EffectOpener::identity_encoding`] precisely because delimiters make a
    /// key unparseable-but-collidable.
    /// A `#` here therefore does not misparse — it makes the child process
    /// unregistrable, which surfaces as the parent turn never finishing.
    /// Measured: it turned every subagent spawn into `Stopped(MaxTurns)`.
    #[must_use]
    pub fn render(&self) -> String {
        match self {
            Self::Turn {
                session_id,
                turn_id,
            } => format!("turn:{session_id}:{turn_id}"),
            Self::QueueDrain {
                session_id,
                drain_id,
            } => format!("drain:{session_id}:{drain_id}"),
            Self::Process { process_ref } => format!(
                "process:{}:incarnation:{}",
                process_ref.process_id, process_ref.incarnation
            ),
        }
    }

    /// The canonical encoding of this opener, for key preimages and embedded
    /// ids.
    ///
    /// Unlike [`render`](EffectOpener::render) — the diagnostic projection,
    /// which may stay ambiguous — every component is emitted as its decimal
    /// byte length, a `:`, and then its bytes, so two different openers can
    /// never mint one encoding. `Turn("a:b", "c")` encodes `turn:3:a:b:1:c`
    /// while `Turn("a", "b:c")` encodes `turn:1:a:3:b:c`: the `:`s inside a
    /// component are data, never a boundary. This is the discipline
    /// `IdentityEncoder::string` applies to binary durable preimages
    /// (`crates/lash-core-ids/src/stable_identity.rs`), rendered as text so
    /// the minted identity stays readable.
    ///
    /// The encoding introduces only digits and `:` — never `#` or `/` — so an
    /// identity built on it remains embeddable in a `SessionId`/`ProcessId`
    /// (the separator note on [`EffectOpener::render`] covers why that
    /// matters).
    #[must_use]
    pub fn identity_encoding(&self) -> String {
        fn push_component(encoding: &mut String, component: &str) {
            encoding.push_str(&component.len().to_string());
            encoding.push(':');
            encoding.push_str(component);
        }
        match self {
            Self::Turn {
                session_id,
                turn_id,
            } => {
                let mut encoding = String::from("turn:");
                push_component(&mut encoding, session_id.as_str());
                encoding.push(':');
                push_component(&mut encoding, turn_id.as_str());
                encoding
            }
            Self::QueueDrain {
                session_id,
                drain_id,
            } => {
                let mut encoding = String::from("drain:");
                push_component(&mut encoding, session_id.as_str());
                encoding.push(':');
                push_component(&mut encoding, drain_id);
                encoding
            }
            Self::Process { process_ref } => {
                let mut encoding = String::from("process:");
                push_component(&mut encoding, process_ref.process_id.as_str());
                encoding.push_str(":incarnation:");
                encoding.push_str(&process_ref.incarnation.to_string());
                encoding
            }
        }
    }

    /// The one owner derivation: the admitted execution scope, and nothing
    /// else.
    ///
    /// Every surface that must name the owner of durable work — the lifecycle
    /// parent a child start declares, the host identities the Lashlang bridges
    /// mint, the recorded attempt a tool body runs inside — derives through
    /// here. There is deliberately no registry parameter: an [`AdmittedScope`]
    /// already carries the `ProcessRef` the admission authority bound, and
    /// resolving the reusable name again is exactly the defect ADR 0099 §1
    /// closes — a same-name successor must not rebind work its predecessor
    /// still owns. A recovery path that must validate a *retained* pair uses
    /// `ProcessQuery::get_process_ref`, which answers the exact
    /// `(process_id, incarnation)` or refuses it; `resolve_process_ref` — a
    /// name lookup — is not an owner derivation.
    ///
    /// A queued turn is a real production shape, not an edge one: a turn
    /// started with `drain_id` and no turn id runs its whole effect tree under
    /// `ExecutionScope::QueueDrain`, so a drain is an opener in its own right.
    /// And a cell under `ExecutionScope::Process` — the shape every
    /// `agents.spawn` child takes — is opened by the process incarnation, not
    /// by any turn inside it: a worker retry keeps the incarnation and reuses
    /// the journal, while a re-registration is a different opener.
    ///
    /// # Errors
    ///
    /// The administrative scope kinds (`SessionDelete`, `RuntimeOperation`)
    /// are refused — widening what an opener is is a contract decision, not a
    /// fallback. A process scope with no incarnation cannot reach this
    /// function: [`AdmittedScope`] refuses it at construction.
    pub fn for_scope(scope: &AdmittedScope) -> Result<Self, EffectOpenerError> {
        match scope.scope() {
            crate::ExecutionScope::Turn {
                session_id,
                turn_id,
            } => Ok(Self::turn(session_id.clone(), turn_id.clone())),
            crate::ExecutionScope::QueueDrain {
                session_id,
                drain_id,
            } => Ok(Self::queue_drain(session_id.clone(), drain_id.clone())),
            crate::ExecutionScope::Process { .. } => {
                #[expect(
                    clippy::expect_used,
                    reason = "an AdmittedScope cannot pair a Process scope with no incarnation — its only construction checks the pair"
                )]
                let process_ref = scope
                    .process_ref()
                    .expect("an admitted process scope carries its incarnation");
                Ok(Self::process(process_ref.clone()))
            }
            crate::ExecutionScope::SessionDelete { .. } => Err(EffectOpenerError::NotAnOpener {
                scope_kind: "session-delete",
            }),
            crate::ExecutionScope::RuntimeOperation { .. } => Err(EffectOpenerError::NotAnOpener {
                scope_kind: "runtime-operation",
            }),
        }
    }
}

/// A scope that names no opener this contract can express.
///
/// Refusals, not extra arms: widening the opener is a contract decision, and
/// inventing an identity here would hide the site that needed it.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EffectOpenerError {
    /// A scope kind that is not an opener at all. `SessionDelete` and
    /// `RuntimeOperation` run administrative work and own no durable effects.
    #[error(
        "{scope_kind} scope names no opener: neither a turn, a queued-work drain nor a process incarnation"
    )]
    NotAnOpener {
        /// The scope kind, for the diagnostic.
        scope_kind: &'static str,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process_identity::ProcessIncarnation;

    fn process_opener(name: &str, incarnation: u64) -> EffectOpener {
        EffectOpener::process(ProcessRef::new(
            name,
            ProcessIncarnation::from_registration_sequence(incarnation),
        ))
    }

    /// The defect §1 names: a name reused after re-registration is a different
    /// opener, and must not compare equal to its predecessor.
    #[test]
    fn a_reregistered_process_name_is_a_different_opener() {
        assert_ne!(process_opener("indexer", 1), process_opener("indexer", 2));
        assert_ne!(
            process_opener("indexer", 1).render(),
            process_opener("indexer", 2).render()
        );
    }

    /// The reason the rendering is tagged. A turn scope is free-form text and
    /// can spell a process opener exactly; untagged, the two would mint one
    /// identity.
    #[test]
    fn a_turn_whose_ids_spell_a_process_opener_still_renders_distinctly() {
        let masquerading = EffectOpener::turn("process:indexer", "1");
        let real = process_opener("indexer", 1);
        assert_ne!(masquerading, real);
        assert_ne!(masquerading.render(), real.render());
    }

    /// A rendered opener introduces neither reserved separator.
    ///
    /// `#` and `/` both carve stored ids apart (see [`EffectOpener::render`]),
    /// and a rendering that introduced one would make every identity built on
    /// top of it unusable as a process id. Asserted for ids that carry
    /// neither, so the rendering is what is being tested rather than the
    /// caller's input.
    #[test]
    fn a_rendered_opener_introduces_no_reserved_separator() {
        for opener in [
            EffectOpener::turn("session-1", "turn-7"),
            EffectOpener::queue_drain("session-1", "drain-3"),
            process_opener("indexer", 3),
        ] {
            for projection in [opener.render(), opener.identity_encoding()] {
                assert!(
                    !projection.contains('#'),
                    "`#` is refused inside a process id: {projection}"
                );
                assert!(
                    !projection.contains('/'),
                    "`/` splits a stored turn parent scope: {projection}"
                );
            }
        }
    }

    /// The collision the canonical encoding exists to close.
    ///
    /// `SessionId`, `TurnId` and drain ids accept arbitrary strings, so the
    /// `:`-joined rendering of `Turn("a:b", "c")` and `Turn("a", "b:c")` is
    /// the same text — `turn:a:b:c`. That is acceptable for the diagnostic
    /// projection; the canonical encoding length-prefixes every component, so
    /// the two encode as `turn:3:a:b:1:c` and `turn:1:a:3:b:c` and any
    /// identity derived from them stays distinct.
    #[test]
    fn delimiter_bearing_components_render_equal_but_encode_distinctly() {
        for (early, late) in [
            (
                EffectOpener::turn("a:b", "c"),
                EffectOpener::turn("a", "b:c"),
            ),
            (
                EffectOpener::queue_drain("a:b", "c"),
                EffectOpener::queue_drain("a", "b:c"),
            ),
        ] {
            assert_eq!(
                early.render(),
                late.render(),
                "the diagnostic rendering is free-form and may collide"
            );
            assert_ne!(
                early.identity_encoding(),
                late.identity_encoding(),
                "the canonical encoding must keep the component boundary exact"
            );
        }
        assert_eq!(
            EffectOpener::turn("a:b", "c").identity_encoding(),
            "turn:3:a:b:1:c"
        );
        assert_eq!(
            EffectOpener::turn("a", "b:c").identity_encoding(),
            "turn:1:a:3:b:c"
        );
        assert_eq!(
            process_opener("indexer", 3).identity_encoding(),
            "process:7:indexer:incarnation:3"
        );
    }

    /// An empty component is encodable too: `0:` marks its boundary exactly.
    #[test]
    fn an_empty_component_encodes_unambiguously() {
        assert_eq!(
            EffectOpener::turn("", "t").identity_encoding(),
            "turn:0::1:t"
        );
        assert_ne!(
            EffectOpener::turn("", ":t").identity_encoding(),
            EffectOpener::turn(":", "t").identity_encoding()
        );
    }

    /// A drain is its own opener, distinct from any turn it runs.
    ///
    /// One drain may run several queued turns, so the drain's opener is not
    /// the opener of any one of them, and a turn whose ids spell a drain must
    /// not reach it either.
    #[test]
    fn a_drain_is_not_the_turns_it_runs() {
        let drain = EffectOpener::queue_drain("session-1", "drain-3");
        assert_ne!(drain, EffectOpener::turn("session-1", "drain-3"));
        assert_ne!(
            drain.render(),
            EffectOpener::turn("session-1", "drain-3").render()
        );
        assert_eq!(drain.session_id().map(SessionId::as_str), Some("session-1"));
        assert!(drain.process_ref().is_none());
    }

    /// Kind-tagged on the wire, so a decoded opener cannot change arm.
    #[test]
    fn an_opener_round_trips_its_kind_and_its_incarnation() {
        for opener in [EffectOpener::turn("s", "t"), process_opener("indexer", 7)] {
            let json = serde_json::to_string(&opener).expect("an opener serializes");
            let decoded: EffectOpener = serde_json::from_str(&json).expect("an opener decodes");
            assert_eq!(decoded, opener);
        }
        let json = serde_json::to_string(&process_opener("indexer", 7)).expect("serializes");
        assert!(
            json.contains("\"kind\":\"process\""),
            "the kind tag is what keeps the two arms apart, got {json}"
        );
    }

    /// A process opener has no session of its own; a turn opener does. Callers
    /// that need a session beside a process opener must carry it and say so.
    #[test]
    fn only_a_turn_opener_answers_with_a_session() {
        assert_eq!(
            EffectOpener::turn("s", "t")
                .session_id()
                .map(SessionId::as_str),
            Some("s")
        );
        assert!(process_opener("indexer", 1).session_id().is_none());
        assert!(EffectOpener::turn("s", "t").process_ref().is_none());
        assert_eq!(
            process_opener("indexer", 1)
                .process_ref()
                .map(|r| r.incarnation.registration_sequence()),
            Some(1)
        );
    }

    // -----------------------------------------------------------------------
    // `for_scope`: the one owner derivation — an admitted scope, with no name
    // resolution.
    // -----------------------------------------------------------------------

    /// Turn and drain scopes carry their whole owner in the scope itself.
    #[test]
    fn a_turn_and_a_drain_scope_derive_their_openers() {
        assert_eq!(
            EffectOpener::for_scope(&AdmittedScope::turn("s", "t")).expect("a turn is an opener"),
            EffectOpener::turn("s", "t")
        );
        assert_eq!(
            EffectOpener::for_scope(&AdmittedScope::queue_drain("s", "d"))
                .expect("a drain is an opener"),
            EffectOpener::queue_drain("s", "d")
        );
    }

    /// A process scope plus its pinned incarnation is a process opener — the
    /// incarnation is what the derivation adds to the reusable name.
    #[test]
    fn a_process_scope_plus_its_pin_is_a_process_opener() {
        let pin = ProcessRef::new("worker", ProcessIncarnation::from_registration_sequence(3));
        assert_eq!(
            EffectOpener::for_scope(&AdmittedScope::process(pin.clone()))
                .expect("the pinned incarnation is the opener"),
            EffectOpener::process(pin)
        );
    }

    /// The administrative scope kinds run no durable effects and own nothing.
    #[test]
    fn administrative_scopes_have_no_opener() {
        assert_eq!(
            EffectOpener::for_scope(&AdmittedScope::session_delete("s")),
            Err(EffectOpenerError::NotAnOpener {
                scope_kind: "session-delete",
            })
        );
        assert_eq!(
            EffectOpener::for_scope(&AdmittedScope::runtime_operation("op")),
            Err(EffectOpenerError::NotAnOpener {
                scope_kind: "runtime-operation",
            })
        );
    }
}
