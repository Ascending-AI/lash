//! Host front-door admission for durable tool intents.

use lash_sansio::SessionId;
use lash_sansio::sync::MutexExt;
use tracing::Instrument;

/// Typed idempotency key for one host-submitted tool intent.
///
/// Its identity is exactly `(session_id, execution_scope_id, tool_call_id,
/// intent_index)`; the replay key is derived by Lash and validated again at
/// submission. The required protocol version rejects keys issued before the
/// current admission-and-realization contract.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ToolIntentIngressKey {
    /// Version selecting the admission and realization contract.
    protocol_version: u16,
    #[serde(flatten)]
    identity: lash_core::ToolIntentIdentity,
}

#[derive(serde::Deserialize)]
struct ToolIntentIngressKeyWire {
    protocol_version: Option<u16>,
    #[serde(flatten)]
    identity: lash_core::ToolIntentIdentity,
}

impl<'de> serde::Deserialize<'de> for ToolIntentIngressKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = <ToolIntentIngressKeyWire as serde::Deserialize>::deserialize(deserializer)?;
        Ok(Self {
            // The unversioned predecessor shape is retained only as an
            // explicit v1 refusal carrier. It never inherits current behavior.
            protocol_version: wire.protocol_version.unwrap_or(1),
            identity: wire.identity,
        })
    }
}

impl ToolIntentIngressKey {
    pub fn derive(
        session_id: &SessionId,
        execution_scope_id: impl AsRef<str>,
        tool_call_id: &lash_core::ToolCallId,
        intent_index: u32,
    ) -> Self {
        let identity = lash_core::derive_tool_intent_identity(
            &lash_core::RuntimeOwner::Session(session_id.clone()),
            execution_scope_id.as_ref(),
            tool_call_id,
            intent_index,
        );
        Self {
            protocol_version: lash_core::TOOL_INTENT_PROTOCOL_V3,
            identity,
        }
    }

    pub fn identity(&self) -> &lash_core::ToolIntentIdentity {
        &self.identity
    }
}

/// Typed refusal returned before a host submission is admitted.
///
/// Refusals are data rather than string errors so transports can preserve
/// malformed and foreign-key distinctions.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum ToolIntentIngressRefusal {
    /// Refuses a predecessor or unknown admission-and-realization protocol.
    UnsupportedProtocolVersion {
        /// Protocol version carried by the submitted ingress key or durable row.
        recorded: u16,
    },
    /// Refuses a submission whose replay key does not match its recorded key.
    MalformedKey {
        /// Replay key derived from the submitted intent identity.
        expected_replay_key: String,
        /// Replay key stored with the recorded intent outcome.
        recorded_replay_key: String,
    },
    /// Refuses a submission recorded for a different session.
    ForeignSession {
        /// Session or scope identity expected by ingress validation.
        expected: String,
        /// Session, scope, or outcome identity found in recorded state.
        recorded: String,
    },
    /// Refuses a submission recorded for a different execution scope.
    ForeignExecutionScope {
        /// Session or scope identity expected by ingress validation.
        expected: String,
        /// Session, scope, or outcome identity found in recorded state.
        recorded: String,
    },
    /// Refuses an intent whose session does not match the ingress session.
    IntentSessionMismatch {
        /// Session or scope identity expected by ingress validation.
        expected: String,
        /// Session, scope, or outcome identity found in recorded state.
        recorded: String,
    },
    /// Refuses a trigger registration claiming an owner scope other than the
    /// one this ingress's session confers (FIG-3116).
    ForeignTriggerOwnerScope {
        /// Owner scope the ingress's session resolves to.
        expected: lash_core::TriggerOwnerScope,
        /// Owner scope the submitted registration claimed.
        recorded: lash_core::TriggerOwnerScope,
    },
    /// Refuses a trigger registration claiming an actor other than the
    /// ingress's own session originator (FIG-3116).
    ForeignTriggerActor {
        /// Actor the ingress's session confers.
        expected: lash_core::ProcessOriginator,
        /// Actor the submitted registration claimed.
        recorded: lash_core::ProcessOriginator,
    },
    /// Refuses an identity already bound to a different intent kind.
    IdentityBoundToDifferentIntent {
        /// Intent kind already bound to this identity.
        recorded_kind: lash_core::ToolIntentKind,
        /// Intent kind supplied by the rejected submission.
        submitted_kind: lash_core::ToolIntentKind,
    },
    /// Refuses a duplicate intent identity.
    DuplicateIdentity {
        /// Intent kind associated with the duplicate identity.
        kind: lash_core::ToolIntentKind,
    },
    /// Refuses a recorded outcome that is not part of the intent protocol.
    RecordedOutcomeOutsideIntentProtocol {
        /// Session, scope, or outcome identity found in recorded state.
        recorded: String,
    },
    /// Refuses a redelivered trigger emission whose occurrence retention has
    /// reclaimed (FIG-4513).
    ///
    /// The first delivery emitted the occurrence and stopped before its
    /// outcome was retained. The occurrence and its deliveries have since
    /// been reclaimed, so the redelivery emits nothing again and has no
    /// recorded outcome to answer.
    TriggerOccurrenceReclaimed,
    /// Refuses a submission whose owner session was durably deleted and
    /// whose submission ledger the host's retained-evidence lever reclaimed
    /// (FIG-1509).
    ///
    /// The reclaimed rows were the only record of what their identities
    /// realized, so the owner keeps a fence and nothing under it is claimed
    /// or realized again.
    SubmissionOwnerReclaimed,
}

/// Admission result for one host-submitted intent.
///
/// Repeating the same key returns the first typed `outcome` with
/// `replayed: true` and cannot realize a conflicting payload twice -- whether
/// the repeat is caught by the effect host's journal or, on a fresh invocation
/// with an empty journal or a host that journals nothing, by the durable key
/// the store already holds.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ToolIntentIngressOutcome {
    /// Carries the admitted tool-intent outcome and whether it was replayed.
    Admitted {
        /// Execution outcome produced for the admitted intent.
        outcome: lash_core::ToolIntentExecutionOutcome,
        /// `false` only when this submission wrote the durable fact.
        ///
        /// `true` covers both ways a submission can realize nothing: the
        /// effect host's journal returned an earlier outcome without
        /// reaching the store, or the store coalesced the write onto
        /// the fact it already held under the same durable key (FIG-3070).
        replayed: bool,
    },
    /// Carries the reason a tool intent was refused.
    Refused {
        /// Reason the tool intent was refused.
        refusal: ToolIntentIngressRefusal,
    },
}

/// One realized host submission, before it is projected to a typed outcome.
///
/// Process commands, trigger emissions and trigger registrations are owned by the trigger router, which has no `ProcessEffectOutcome`.
enum RealizedIntent {
    Process(lash_core::ProcessEffectOutcome),
    Trigger(lash_core::facade_support::TriggerEmitReport),
    // Boxed: a registration handle carries the whole admitted subscription
    // record, including its captured source contract and route, and is an
    // order of magnitude larger than the other two variants.
    TriggerRegistration(Box<lash_core::TriggerMutationReceipt>),
}

/// Session-and-scope-bound host front door for durable intent realization.
///
/// This is the sanctioned way for a host to submit a `ToolIntent` outside a
/// turn. Leaf tool bodies cannot construct this value; they return typed
/// `ToolIntents` to the attempt coordinator instead.
#[derive(Clone)]
pub struct ToolIntentIngress {
    core: crate::LashCore,
    session_id: SessionId,
    scope: lash_core::ExecutionScope,
    trace: crate::send::SendTraceContext,
}

/// What one submission offers its ledger row's trace scope, captured once
/// when the submission was made.
struct SubmissionTrace {
    offer: lash_core::TraceScopeOffer,
    submitted_at_ms: u64,
}

impl SubmissionTrace {
    fn offered(
        &self,
        record: lash_core::ToolIntentSubmissionRecord,
    ) -> lash_core::ToolIntentSubmissionRecord {
        record.with_trace_offer(self.offer.clone(), self.submitted_at_ms)
    }
}

enum RealizationFailure {
    Refused(ToolIntentIngressRefusal),
    Command(lash_core::ToolIntentKind, lash_core::PluginError),
}

impl crate::LashCore {
    /// Bind the sanctioned host ingress for durable tool intents to one actual
    /// session and execution scope.
    ///
    /// Hosts submit durable leaf-style declarations here when no tool attempt
    /// owns the call. Tool bodies return [`crate::tools::ToolIntents`] instead
    /// and never call this front door.
    pub fn tool_intents(
        &self,
        session_id: SessionId,
        scope: lash_core::ExecutionScope,
    ) -> crate::Result<ToolIntentIngress> {
        scope.validate().map_err(lash_core::RuntimeError::from)?;
        if let Some(scoped_session) = scope.session_id()
            && scoped_session != session_id
        {
            return Err(lash_core::RuntimeError::new(
                lash_core::RuntimeErrorCode::MissingExecutionScopeId,
                format!(
                    "tool-intent ingress session `{session_id}` does not match scope session `{scoped_session}`"
                ),
            )
            .into());
        }
        Ok(ToolIntentIngress::new(self.clone(), session_id, scope))
    }
}

impl ToolIntentIngress {
    pub(crate) fn new(
        core: crate::LashCore,
        session_id: SessionId,
        scope: lash_core::ExecutionScope,
    ) -> Self {
        Self {
            core,
            session_id,
            scope,
            trace: crate::send::SendTraceContext::Ambient,
        }
    }

    /// Link the intents this ingress submits to `context`, the trace
    /// context of whatever caused them. An explicit context wins over the
    /// caller's ambient one, which is then never consulted.
    ///
    /// The link sits beside an intent, never inside it: the first
    /// submission of an identity retains the context it was given, and a
    /// redelivery under another context is the same submission and keeps
    /// the first one.
    pub fn trace_context(mut self, context: lash_core::TraceCarrier) -> Self {
        self.trace.set_context(context);
        self
    }

    /// Snapshot the caller's current trace context now, through the core's
    /// telemetry adapter, instead of when each submission is made.
    pub fn capture_trace_context(mut self) -> Self {
        self.trace
            .capture(|| self.core.env.core.tracing.scopes().capture_current());
        self
    }

    /// What a submission made now offers its ledger row.
    fn submission_trace(&self) -> SubmissionTrace {
        SubmissionTrace {
            offer: lash_core::TraceScopeOffer::caused_by(
                self.trace
                    .cause_through(self.core.env.core.tracing.scopes().as_ref()),
            ),
            submitted_at_ms: self.core.env.core.clock.timestamp_ms(),
        }
    }

    /// Derive the idempotency key of the intent at `intent_index` of one host
    /// submission, bound to this ingress's actual session and execution
    /// scope.
    ///
    /// `submission` is the submission's admitted operation handle: the call
    /// the intents belong to is named under that host-submission root (ADR
    /// 0117 §2), so a redelivery presenting the same handle names the same
    /// call, and a new submission under a new handle a new one.
    ///
    /// # Errors
    ///
    /// A blank handle roots no call.
    pub fn key(
        &self,
        submission: impl AsRef<str>,
        intent_index: u32,
    ) -> Result<ToolIntentIngressKey, lash_core::ToolCallRootError> {
        let call_id =
            lash_core::ToolCallAdmission::host_submission("", submission.as_ref())?.call_id(&[]);
        Ok(ToolIntentIngressKey::derive(
            &self.session_id,
            self.scope.id(),
            &call_id,
            intent_index,
        ))
    }

    /// Submit one durable intent using first-writer-wins identity semantics.
    ///
    /// Validation happens before any process command. The configured effect
    /// host owns admission: realization uses the identity-derived replay key
    /// there, so a crash redrives the same command frame rather than creating
    /// a second realization. Reuse of an identity returns the first writer's
    /// outcome with `replayed: true`; the later payload is not realized. Every
    /// shape lands on a durable key at the point it mutates, so a re-submitted
    /// identity the effect journal cannot see realizes once too, and a changed
    /// payload under a bound identity is refused at the store as
    /// [`ToolIntentIngressRefusal::DuplicateIdentity`]. `CancelProcess`
    /// carries no content past its target, and its store fence lives on the
    /// target record, so its identity is bound to the target it first named in
    /// the durable submission ledger instead: a re-used identity naming a
    /// *different* target is refused there, before the second target is
    /// touched. Every outcome is retained in that ledger.
    ///
    /// `StartProcess` and `EmitTrigger` submissions do not retain their
    /// host-chosen realization identifiers. Lash replaces a start's
    /// `request.id` and a trigger's `request.idempotency_key` with the derived
    /// intent replay key before either command reaches its durable store.
    pub async fn submit(
        &self,
        key: ToolIntentIngressKey,
        intent: lash_core::ToolIntent,
    ) -> ToolIntentIngressOutcome {
        let identity = key.identity().clone();
        let span = tracing::info_span!(
            target: "lash::tool_intent_ingress",
            "tool_intent_ingress.submit",
            owner = %identity.owner,
            execution_scope_id = %identity.execution_scope_id,
            tool_call_id = %identity.tool_call_id,
            intent_index = identity.intent_index,
            replay_key = %identity.replay_key,
            submitted_kind = %intent.kind().as_str(),
        );
        // The caller's context is snapshotted here, before the first await.
        let trace = self.submission_trace();
        Box::pin(
            async {
                let outcome = self.submit_inner(key, intent, &trace).await;
                Self::record_decision(&identity, &outcome);
                outcome
            }
            .instrument(span),
        )
        .await
    }

    async fn submit_inner(
        &self,
        key: ToolIntentIngressKey,
        intent: lash_core::ToolIntent,
        trace: &SubmissionTrace,
    ) -> ToolIntentIngressOutcome {
        if let Some(refusal) = self.validate(&key, &intent) {
            return ToolIntentIngressOutcome::Refused { refusal };
        }
        let identity = key.identity;
        let submitted_intent = intent.clone();
        let (outcome, replayed) = match self.realize(&identity, intent, trace).await {
            Ok((result, replayed)) => (
                lash_core::ToolIntentExecutionOutcome::Executed {
                    identity: identity.clone(),
                    realized: result,
                },
                replayed,
            ),
            Err(RealizationFailure::Refused(refusal)) => {
                return ToolIntentIngressOutcome::Refused { refusal };
            }
            Err(RealizationFailure::Command(kind, error)) => {
                let mut outcome = lash_core::ToolIntentExecutionOutcome::Refused {
                    identity: Some(identity.clone()),
                    intent_index: identity.intent_index,
                    kind,
                    refusal: lash_core::ToolIntentRefusalReason::CommandFailed {
                        cause: lash_core::ToolIntentCommandFailure::from(&error),
                    },
                };
                if let Err(store_error) = self
                    .retain_outcome(&identity, submitted_intent.clone(), outcome.clone(), trace)
                    .await
                {
                    outcome = lash_core::ToolIntentExecutionOutcome::Refused {
                        identity: Some(identity.clone()),
                        intent_index: identity.intent_index,
                        kind,
                        refusal: lash_core::ToolIntentRefusalReason::CommandFailed {
                            cause: lash_core::ToolIntentCommandFailure::from(&store_error),
                        },
                    };
                }
                (outcome, false)
            }
        };
        ToolIntentIngressOutcome::Admitted { outcome, replayed }
    }

    fn record_decision(
        identity: &lash_core::ToolIntentIdentity,
        outcome: &ToolIntentIngressOutcome,
    ) {
        let (decision, replayed, refusal_kind) = match outcome {
            ToolIntentIngressOutcome::Admitted { replayed, .. } => (
                if *replayed { "replayed" } else { "admitted" },
                *replayed,
                None,
            ),
            ToolIntentIngressOutcome::Refused { refusal } => {
                ("refused", false, Some(Self::refusal_kind(refusal)))
            }
        };
        tracing::info!(
            target: "lash::tool_intent_ingress",
            owner = %identity.owner,
            execution_scope_id = %identity.execution_scope_id,
            tool_call_id = %identity.tool_call_id,
            intent_index = identity.intent_index,
            replay_key = %identity.replay_key,
            decision,
            replayed,
            refusal_kind = refusal_kind.unwrap_or("none"),
            "tool intent ingress decision"
        );
    }

    fn refusal_kind(refusal: &ToolIntentIngressRefusal) -> &'static str {
        match refusal {
            ToolIntentIngressRefusal::UnsupportedProtocolVersion { .. } => {
                "unsupported_protocol_version"
            }
            ToolIntentIngressRefusal::MalformedKey { .. } => "malformed_key",
            ToolIntentIngressRefusal::ForeignSession { .. } => "foreign_session",
            ToolIntentIngressRefusal::ForeignExecutionScope { .. } => "foreign_execution_scope",
            ToolIntentIngressRefusal::IntentSessionMismatch { .. } => "intent_session_mismatch",
            ToolIntentIngressRefusal::ForeignTriggerOwnerScope { .. } => {
                "foreign_trigger_owner_scope"
            }
            ToolIntentIngressRefusal::ForeignTriggerActor { .. } => "foreign_trigger_actor",
            ToolIntentIngressRefusal::IdentityBoundToDifferentIntent { .. } => {
                "identity_bound_to_different_intent"
            }
            ToolIntentIngressRefusal::DuplicateIdentity { .. } => "duplicate_identity",
            ToolIntentIngressRefusal::RecordedOutcomeOutsideIntentProtocol { .. } => {
                "recorded_outcome_outside_intent_protocol"
            }
            ToolIntentIngressRefusal::TriggerOccurrenceReclaimed => "trigger_occurrence_reclaimed",
            ToolIntentIngressRefusal::SubmissionOwnerReclaimed => "submission_owner_reclaimed",
        }
    }

    fn validate(
        &self,
        key: &ToolIntentIngressKey,
        intent: &lash_core::ToolIntent,
    ) -> Option<ToolIntentIngressRefusal> {
        if key.protocol_version != lash_core::TOOL_INTENT_PROTOCOL_V3 {
            return Some(ToolIntentIngressRefusal::UnsupportedProtocolVersion {
                recorded: key.protocol_version,
            });
        }
        let identity = key.identity();
        let expected_replay_key = Self::expected_identity(identity).replay_key;
        if expected_replay_key != identity.replay_key {
            return Some(ToolIntentIngressRefusal::MalformedKey {
                expected_replay_key,
                recorded_replay_key: identity.replay_key.clone(),
            });
        }
        let own = lash_core::RuntimeOwner::Session(self.session_id.clone());
        if identity.owner != own {
            return Some(ToolIntentIngressRefusal::ForeignSession {
                expected: own.to_string(),
                recorded: identity.owner.to_string(),
            });
        }
        if identity.execution_scope_id != self.scope.id() {
            return Some(ToolIntentIngressRefusal::ForeignExecutionScope {
                expected: self.scope.id().to_string(),
                recorded: identity.execution_scope_id.clone(),
            });
        }
        if *intent.owner() != own {
            return Some(ToolIntentIngressRefusal::IntentSessionMismatch {
                expected: own.to_string(),
                recorded: intent.owner().to_string(),
            });
        }
        if let lash_core::ToolIntent::RegisterTrigger(registration) = intent {
            return self.validate_trigger_authority(registration);
        }
        None
    }

    /// A host front door runs inside no process, so the only registration
    /// authority it confers is its own session's: the owner scope
    /// `resolve_trigger_owner_scope` rules for that session with no
    /// originator, and the session's frameless originator as actor. Any other
    /// claim is refused before realization installs anything (FIG-3116).
    fn validate_trigger_authority(
        &self,
        registration: &lash_core::RegisterTriggerIntent,
    ) -> Option<ToolIntentIngressRefusal> {
        // With no originator the ruling is infallible: the session's own scope.
        let expected_owner = lash_core::resolve_trigger_owner_scope(
            &lash_core::RuntimeOwner::Session(self.session_id.clone()),
            None,
        )
        .unwrap_or_else(|_| lash_core::TriggerOwnerScope::session(self.session_id.clone()));
        if registration.owner_scope != expected_owner {
            return Some(ToolIntentIngressRefusal::ForeignTriggerOwnerScope {
                expected: expected_owner,
                recorded: registration.owner_scope.clone(),
            });
        }
        let expected_actor = lash_core::ProcessOriginator::session(lash_core::SessionScope::new(
            self.session_id.clone(),
        ));
        (registration.actor != expected_actor).then(|| {
            ToolIntentIngressRefusal::ForeignTriggerActor {
                expected: expected_actor,
                recorded: registration.actor.clone(),
            }
        })
    }

    /// The identity a well-formed record must carry, re-derived from its own
    /// durable fields by the single re-derivation constructor. Deriving it here
    /// instead dropped `minting_emission_replay_key` and so read every
    /// runtime-minted identity as forged (FIG-2994).
    fn expected_identity(
        identity: &lash_core::ToolIntentIdentity,
    ) -> lash_core::ToolIntentIdentity {
        lash_core::rederive_tool_intent_identity(identity)
    }

    async fn realize(
        &self,
        identity: &lash_core::ToolIntentIdentity,
        intent: lash_core::ToolIntent,
        trace: &SubmissionTrace,
    ) -> std::result::Result<(lash_core::ToolIntentRealized, bool), RealizationFailure> {
        let kind = intent.kind();
        let submitted_intent = intent.clone();
        if let Some(recorded) = self.admit_submission(identity, &intent, trace).await? {
            return Ok((recorded, true));
        }
        let (result, replayed) = self
            .realize_inner(identity, intent)
            .await
            .map_err(|error| Self::realization_failure(kind, error))?;
        let realized = match result {
            RealizedIntent::Trigger(report) => lash_core::ToolIntentRealized::EmitTrigger(report),
            RealizedIntent::TriggerRegistration(receipt) => {
                lash_core::ToolIntentRealized::RegisterTrigger(receipt)
            }
            RealizedIntent::Process(result) => match result {
                lash_core::ProcessEffectOutcome::Start { record, .. } => {
                    lash_core::ToolIntentRealized::StartProcess(
                        lash_core::ProcessHandleView::from_record(*record),
                    )
                }
                lash_core::ProcessEffectOutcome::Signal { event } => {
                    lash_core::ToolIntentRealized::SignalProcess(event)
                }
                lash_core::ProcessEffectOutcome::Cancel { record } => {
                    lash_core::ToolIntentRealized::CancelProcess(
                        lash_core::ProcessCancelReceipt::from_record(*record)
                            .map_err(|error| RealizationFailure::Command(kind, error))?,
                    )
                }
                lash_core::ProcessEffectOutcome::EmitEvent { event, .. } => {
                    lash_core::ToolIntentRealized::EmitProcessEvent(event)
                }
                lash_core::ProcessEffectOutcome::Definition { definition } => match kind {
                    lash_core::ToolIntentKind::PublishDefinition => {
                        lash_core::ToolIntentRealized::PublishDefinition(definition)
                    }
                    lash_core::ToolIntentKind::GetDefinition => {
                        lash_core::ToolIntentRealized::GetDefinition(definition)
                    }
                    lash_core::ToolIntentKind::StartProcess
                    | lash_core::ToolIntentKind::SignalProcess
                    | lash_core::ToolIntentKind::CancelProcess
                    | lash_core::ToolIntentKind::EmitProcessEvent
                    | lash_core::ToolIntentKind::EmitTrigger
                    | lash_core::ToolIntentKind::RegisterTrigger => {
                        return Err(Self::outside_protocol_outcome("definition"));
                    }
                },
                lash_core::ProcessEffectOutcome::ValidateVisible { .. } => {
                    return Err(Self::outside_protocol_outcome("validate_visible"));
                }
                lash_core::ProcessEffectOutcome::List { .. } => {
                    return Err(Self::outside_protocol_outcome("list"));
                }
                lash_core::ProcessEffectOutcome::Transfer => {
                    return Err(Self::outside_protocol_outcome("transfer"));
                }
                lash_core::ProcessEffectOutcome::DeleteSession { .. } => {
                    return Err(Self::outside_protocol_outcome("delete_session"));
                }
                lash_core::ProcessEffectOutcome::Await { .. } => {
                    return Err(Self::outside_protocol_outcome("await"));
                }
                lash_core::ProcessEffectOutcome::AttachTerminal => {
                    return Err(Self::outside_protocol_outcome("attach_terminal"));
                }
            },
        };
        if realized.kind() != kind {
            return Err(RealizationFailure::Refused(
                ToolIntentIngressRefusal::IdentityBoundToDifferentIntent {
                    recorded_kind: realized.kind(),
                    submitted_kind: kind,
                },
            ));
        }
        let outcome = lash_core::ToolIntentExecutionOutcome::Executed {
            identity: identity.clone(),
            realized: realized.clone(),
        };
        self.retain_outcome(identity, submitted_intent, outcome, trace)
            .await
            .map_err(|error| RealizationFailure::Command(kind, error))?;
        Ok((realized, replayed))
    }

    /// Claim `identity`'s row in the durable tool-intent submission ledger
    /// before anything realizes it, and answer the outcome the row already
    /// records (ADR 0105 §1).
    ///
    /// The row is the submission's recorded admission. A redelivery arrives
    /// with an empty effect journal, so the row is the only record of what the
    /// first invocation did: one that executed answers its recorded result
    /// here, without realizing again against a store that may have moved on,
    /// such as a target that has since ended and been pruned. A row with no
    /// executed outcome (the first invocation crashed before retaining one, or
    /// refused) realizes again; every shape's durable key coalesces what a
    /// first realization committed.
    ///
    /// The claim also binds the identity to its first payload. A cancel
    /// carries nothing but its target, and its fence lives on the target
    /// record, so a bound identity re-submitted against a second process
    /// would find that record unfenced and cancel it (FIG-3072); the other
    /// shapes carry their content into the key they land on. The store's
    /// claim is atomic and answers the first writer, so two concurrent
    /// submissions of one identity cannot both bind. A matching payload is a
    /// redelivery; a changed one is refused as
    /// [`ToolIntentIngressRefusal::DuplicateIdentity`], the vocabulary every
    /// shape's store fence uses, except a start's, whose key answers the
    /// process it first minted whatever the declaration (ADR 0107).
    ///
    /// An owner whose ledger the retained-evidence lever reclaimed answers
    /// [`ToolIntentIngressRefusal::SubmissionOwnerReclaimed`] here, before
    /// anything realizes (FIG-1509).
    async fn admit_submission(
        &self,
        identity: &lash_core::ToolIntentIdentity,
        intent: &lash_core::ToolIntent,
        trace: &SubmissionTrace,
    ) -> std::result::Result<Option<lash_core::ToolIntentRealized>, RealizationFailure> {
        let kind = intent.kind();
        let submitted =
            lash_core::ToolIntentSubmissionRecord::new(identity.clone(), intent.clone())
                .map(|record| trace.offered(record))
                .map_err(|error| {
                    RealizationFailure::Command(
                        kind,
                        lash_core::PluginError::Session(format!(
                            "failed to hash tool-intent submission: {error}"
                        )),
                    )
                })?;
        let admission = self
            .process_registry()
            .map_err(|error| RealizationFailure::Command(kind, error))?
            .admit_tool_intent_submission(submitted.clone())
            .await
            .map_err(|error| RealizationFailure::Command(kind, error))?;
        let existing = match admission {
            lash_core::ToolIntentSubmissionAdmission::Admitted => return Ok(None),
            lash_core::ToolIntentSubmissionAdmission::Existing(existing) => existing,
            lash_core::ToolIntentSubmissionAdmission::Reclaimed => {
                return Err(RealizationFailure::Refused(
                    ToolIntentIngressRefusal::SubmissionOwnerReclaimed,
                ));
            }
        };
        if existing.protocol_version != lash_core::TOOL_INTENT_PROTOCOL_V3 {
            return Err(RealizationFailure::Refused(
                ToolIntentIngressRefusal::UnsupportedProtocolVersion {
                    recorded: existing.protocol_version,
                },
            ));
        }
        if existing.kind() != kind {
            return Err(RealizationFailure::Refused(
                ToolIntentIngressRefusal::IdentityBoundToDifferentIntent {
                    recorded_kind: existing.kind(),
                    submitted_kind: kind,
                },
            ));
        }
        // A start's key is trusted (ADR 0107): a changed declaration under
        // the identity answers the process the key first minted, as the
        // registry would.
        if existing.payload_hash != submitted.payload_hash
            && kind != lash_core::ToolIntentKind::StartProcess
        {
            return Err(RealizationFailure::Refused(
                ToolIntentIngressRefusal::DuplicateIdentity { kind },
            ));
        }
        Ok(match existing.execution_outcome() {
            Some(lash_core::ToolIntentExecutionOutcome::Executed { realized, .. }) => {
                Some(realized)
            }
            _ => None,
        })
    }

    /// Retain `outcome` in the durable tool-intent submission ledger under
    /// `identity`, claiming the row first when [`Self::admit_submission`]
    /// could not. The first recorded outcome is kept: a redelivery that
    /// realizes again never replaces it.
    async fn retain_outcome(
        &self,
        identity: &lash_core::ToolIntentIdentity,
        submitted: lash_core::ToolIntent,
        outcome: lash_core::ToolIntentExecutionOutcome,
        trace: &SubmissionTrace,
    ) -> Result<(), lash_core::PluginError> {
        let registry = self.process_registry()?;
        let submission = lash_core::ToolIntentSubmissionRecord::new(identity.clone(), submitted)
            .map(|record| trace.offered(record))
            .map_err(|error| {
                lash_core::PluginError::Runtime(lash_core::RuntimeError::new(
                    lash_core::RuntimeErrorCode::RecordEncodingFailed,
                    format!("failed to hash admitted tool-intent submission: {error}"),
                ))
            })?;
        let recorded = match registry.admit_tool_intent_submission(submission).await? {
            lash_core::ToolIntentSubmissionAdmission::Admitted => false,
            lash_core::ToolIntentSubmissionAdmission::Existing(existing) => {
                existing.settlement.is_some()
            }
            // The owner was deleted and its ledger reclaimed while this
            // submission realized: its fence already refuses every redelivery,
            // so there is no row left to retain the outcome in.
            lash_core::ToolIntentSubmissionAdmission::Reclaimed => return Ok(()),
        };
        if !recorded {
            let receipt = registry
                .complete_tool_intent_submission(&identity.replay_key, outcome)
                .await?;
            let permit = receipt.permit();
            let runtime = &self.core.env.core.tracing;
            let status = match receipt.record.execution_outcome().as_ref() {
                Some(lash_core::ToolIntentExecutionOutcome::Executed { .. }) => {
                    lash_core::operational_metrics::record_tool_intent_executed(
                        runtime.metrics(),
                        permit.as_ref(),
                        receipt.record.kind().as_str(),
                    );
                    lash_trace::TraceDomainStatus::Completed
                }
                Some(
                    lash_core::ToolIntentExecutionOutcome::Refused { refusal, .. }
                    | lash_core::ToolIntentExecutionOutcome::ProtocolRefused { refusal },
                ) => {
                    lash_core::operational_metrics::record_tool_intent_refused(
                        runtime.metrics(),
                        permit.as_ref(),
                        receipt.record.kind().as_str(),
                        refusal.code().as_ref(),
                    );
                    lash_trace::TraceDomainStatus::Failed
                }
                None => return Ok(()),
            };
            if let (Some(scope), Some(at_ms)) = (
                &receipt.record.trace,
                receipt
                    .record
                    .settlement
                    .as_ref()
                    .map(|settlement| settlement.at_ms),
            ) {
                runtime.unreplayed(Some(scope.clone())).transition(
                    permit.as_ref(),
                    at_ms,
                    lash_trace::TraceTransitionKind::Terminal,
                    0,
                    || {
                        let mut completion = lash_trace::TraceDomainCompletion::new(
                            lash_trace::TraceDomainOperation::ToolIntent,
                            scope.started_at_ms,
                            status,
                        );
                        completion.intent_kind = Some(receipt.record.kind().as_str().to_string());
                        (
                            lash_trace::TraceContext::default(),
                            lash_trace::TraceEvent::DomainCompleted { completion },
                        )
                    },
                );
            }
        }
        Ok(())
    }

    /// Classify one realization error.
    ///
    /// Every shape this ingress realizes is fenced by a durable key at the
    /// point it mutates: the process registration fingerprint for a start, the
    /// event replay key for a signal or an emitted event, the cancel replay
    /// override, the occurrence idempotency key for a trigger. When one of
    /// those keys is re-presented with different content the store refuses with
    /// [`lash_core::durable_identity_conflict`]. Mapping it here to
    /// [`ToolIntentIngressRefusal::DuplicateIdentity`] gives hosts one refusal
    /// vocabulary for every shape (FIG-1489) instead of a generic command
    /// failure.
    ///
    /// A trigger store that finds the occurrence's tombstone refuses the
    /// ingest with [`lash_core::trigger_occurrence_reclaimed`], which maps to
    /// [`ToolIntentIngressRefusal::TriggerOccurrenceReclaimed`] (FIG-4513).
    fn realization_failure(
        kind: lash_core::ToolIntentKind,
        error: lash_core::PluginError,
    ) -> RealizationFailure {
        if lash_core::is_durable_identity_conflict(&error) {
            return RealizationFailure::Refused(ToolIntentIngressRefusal::DuplicateIdentity {
                kind,
            });
        }
        if lash_core::is_trigger_occurrence_reclaimed(&error) {
            return RealizationFailure::Refused(
                ToolIntentIngressRefusal::TriggerOccurrenceReclaimed,
            );
        }
        RealizationFailure::Command(kind, error)
    }

    fn outside_protocol_outcome(recorded: &str) -> RealizationFailure {
        RealizationFailure::Refused(
            ToolIntentIngressRefusal::RecordedOutcomeOutsideIntentProtocol {
                recorded: recorded.to_string(),
            },
        )
    }

    async fn realize_inner(
        &self,
        identity: &lash_core::ToolIntentIdentity,
        intent: lash_core::ToolIntent,
    ) -> Result<(RealizedIntent, bool), lash_core::PluginError> {
        if let Some(env_ref) = intent.execution_env_ref() {
            let claim = lash_core::ReferrerClaim::guarded(lash_core::ReferrerGuard::Journal(
                self.scope
                    .journal_identity()
                    .map_err(|error| lash_core::PluginError::Session(error.to_string()))?,
            ));
            self.core
                .env
                .core
                .durability
                .process_env_store
                .acquire_process_execution_env(&claim, env_ref)
                .await
                .map_err(lash_core::PluginError::from)?;
        }
        let command = match intent {
            lash_core::ToolIntent::StartProcess(intent) => {
                // The declaration carries no id. The replay key is the process
                // id, so a re-submitted declaration starts the same process:
                // one constructor, shared with core's recorded-intent seam
                // (FIG-2876, FIG-2994).
                let request = intent.into_request(identity);
                // A host-submitted start is a root: its lifetime is
                // `Detached` or `Until` a session the host holds (FIG-3607
                // R3). The start's recorded admission checks that session is
                // live, never a lookup ahead of it (ADR 0105 §1).
                let observers = request.observers.clone();
                let registration = request.into_registration();
                lash_core::ProcessCommand::Start {
                    registration,
                    observers,
                    execution_context: Box::new(lash_core::ProcessExecutionContext::default()),
                }
            }
            lash_core::ToolIntent::SignalProcess(intent) => {
                // The recorded append admission refuses an unknown or pruned
                // target; no registry read comes ahead of it (ADR 0105 §1).
                let process_id = intent.process_id.clone();
                // The intent's replay key is the signal's id, as on core's
                // recorded-intent seam: the append key is derived from the
                // signal's identity, never spelled here (FIG-4299).
                lash_core::ProcessCommand::Signal {
                    signal: lash_core::ProcessSignal::new(
                        lash_core::ProcessSignalIdentity::new(
                            process_id,
                            intent.signal_name,
                            identity.replay_key.clone(),
                        )?,
                        intent.payload,
                    ),
                }
            }
            lash_core::ToolIntent::CancelProcess(intent) => {
                // The recorded cancel admission refuses an unknown or pruned
                // target; no registry read comes ahead of it (ADR 0105 §1).
                let process_id = intent.process_id;
                // Same stamping core's recorded-intent cancel seam applies
                // (`runtime/session_manager/process_runners/control.rs`): the
                // replay key requests the cancel and the whole identity is the
                // replay attribution. Not a separate contract — the broad
                // `ProcessCommand` type is why it is spelled again here.
                lash_core::ProcessCommand::Cancel {
                    process_id,
                    origin: lash_core::CancelOrigin::ModelRequested,
                    requester: identity.replay_key.clone(),
                    attribution: Some(lash_core::RuntimeReplayAttribution::ToolIntent(
                        identity.clone(),
                    )),
                }
            }
            lash_core::ToolIntent::EmitProcessEvent(intent) => {
                // A plain event emission appends under the declaration's own
                // replay key, with no signal-key derivation: core's
                // `emit_event_recorded_intent` passes the same key through.
                let request =
                    lash_core::ProcessEventAppendRequest::new(intent.event_type, intent.payload)
                        .with_replay_key(identity.replay_key.clone());
                lash_core::ProcessCommand::EmitEvent {
                    process_id: intent.process_id,
                    request,
                }
            }
            lash_core::ToolIntent::EmitTrigger(intent) => {
                let mut request = intent.request;
                request.idempotency_key = identity.replay_key.clone();
                let (report, realization) = self.emit_recorded_trigger(request).await?;
                // The replay-derived occurrence idempotency key, not an
                // effect-journal key, is the dedupe point for a re-submitted
                // trigger emission, so this route has no journal verdict to
                // read. It reports the trigger store's own: a re-submitted
                // occurrence coalesces onto the recorded one (FIG-3070).
                let replayed = realization.is_coalesced();
                return Ok((RealizedIntent::Trigger(report), replayed));
            }
            lash_core::ToolIntent::PublishDefinition(intent) => {
                lash_core::ProcessCommand::PublishDefinition {
                    draft: intent.draft,
                    module: intent.module,
                }
            }
            lash_core::ToolIntent::GetDefinition(intent) => {
                lash_core::ProcessCommand::GetDefinition {
                    definition_id: intent.definition_id,
                }
            }
            lash_core::ToolIntent::RegisterTrigger(intent) => {
                let handle = self.register_recorded_trigger(identity, *intent).await?;
                return Ok((RealizedIntent::TriggerRegistration(handle), false));
            }
        };
        let (result, replayed) = self.run_command(identity, command).await?;
        Ok((RealizedIntent::Process(result), replayed))
    }

    /// Install one recorded subscription draft through the same trigger effect
    /// the runtime intent executor uses, keyed by the declaration's replay key.
    async fn register_recorded_trigger(
        &self,
        identity: &lash_core::ToolIntentIdentity,
        intent: lash_core::RegisterTriggerIntent,
    ) -> Result<Box<lash_core::TriggerMutationReceipt>, lash_core::PluginError> {
        // `validate` already pinned `owner_scope` and `actor` to this
        // ingress's own session authority (FIG-3116).
        let store = self.core.env.core.trigger_store();
        let scoped = self
            .core
            .env
            .core
            .control
            .effect_host
            .scoped(lash_core::AdmittedScope::new(self.scope.clone()))
            .map_err(lash_core::PluginError::Runtime)?;
        // The realizing execution's journal holds what it publishes, and the
        // command's effect holds the revision it commits before it commits
        // (ADR 0113 §3.4, §3.7).
        let creator = scoped
            .execution_scope()
            .journal_identity()
            .map_err(|error| lash_core::PluginError::Session(error.to_string()))?;
        let store: std::sync::Arc<dyn lash_core::TriggerStore> =
            std::sync::Arc::new(lash_core::triggers::RevisionReferrerTriggerStore::new(
                store,
                self.core.host_process_engines.clone(),
                creator.clone(),
            ));
        let draft = intent.draft;
        let invocation = lash_core::RuntimeEffectInvocation::new(
            lash_core::EffectAddress::new(
                scoped.execution_scope().clone(),
                identity.replay_key.clone(),
            )
            .map_err(|error| lash_core::PluginError::Session(error.to_string()))?,
            lash_core::RuntimeAttribution::for_session(self.session_id.clone()),
            identity.replay_key.clone(),
        )
        .with_replay_attribution(lash_core::RuntimeReplayAttribution::ToolIntent(
            identity.clone(),
        ));
        let outcome = scoped
            .tool_effect(
                lash_core::RuntimeEffectEnvelope::new(
                    invocation,
                    lash_core::RuntimeEffectCommand::Trigger {
                        command: Box::new(lash_core::TriggerCommand::Register {
                            owner_scope: intent.owner_scope,
                            actor: intent.actor,
                            draft,
                        }),
                    },
                ),
                lash_core::RuntimeEffectLocalExecutor::triggers(store),
            )
            .await
            .map_err(lash_core::PluginError::RuntimeEffectController)?
            .into_trigger()
            .map_err(lash_core::PluginError::RuntimeEffectController)?
            .map_err(|error| lash_core::PluginError::TriggerOperation(Box::new(error)))?;
        match outcome {
            lash_core::TriggerCommandOutcome::Mutation { receipt } => Ok(receipt),
            other => Err(lash_core::PluginError::Session(format!(
                "trigger registration returned a non-mutation outcome: {other:?}"
            ))),
        }
    }

    async fn emit_recorded_trigger(
        &self,
        request: lash_core::TriggerOccurrenceRequest,
    ) -> Result<
        (
            lash_core::facade_support::TriggerEmitReport,
            lash_core::StoreRealization,
        ),
        lash_core::PluginError,
    > {
        let store = self.core.env.core.trigger_store();
        let ports = self.core.substrate_slot.ports().await;
        let process_work = ports.process;
        let mut router = lash_core::facade_support::TriggerRouter::new(store, process_work)
            .with_process_artifacts(
                std::sync::Arc::clone(&self.core.env.core.durability.process_env_store),
                self.core.host_process_engines.clone(),
            );
        if let Some(restorer) = &self.core.env.core.control.trigger_route_restorer {
            router = router.with_route_restorer(std::sync::Arc::clone(restorer));
        }
        let scoped = self
            .core
            .env
            .core
            .control
            .effect_host
            .scoped(lash_core::AdmittedScope::new(self.scope.clone()))
            .map_err(lash_core::PluginError::Runtime)?;
        router
            .emit_recorded_reporting_realization(request, &scoped)
            .await
    }

    fn process_registry(
        &self,
    ) -> Result<std::sync::Arc<dyn lash_core::ProcessRegistry>, lash_core::PluginError> {
        self.core.env.process_registry().cloned().ok_or_else(|| {
            lash_core::PluginError::Session(
                "process registry is unavailable in this runtime".to_string(),
            )
        })
    }

    async fn run_command(
        &self,
        identity: &lash_core::ToolIntentIdentity,
        command: lash_core::ProcessCommand,
    ) -> Result<(lash_core::ProcessEffectOutcome, bool), lash_core::PluginError> {
        self.run_command_with_replay_key(identity, identity.replay_key.clone(), command)
            .await
    }

    async fn run_command_with_replay_key(
        &self,
        identity: &lash_core::ToolIntentIdentity,
        replay_key: String,
        command: lash_core::ProcessCommand,
    ) -> Result<(lash_core::ProcessEffectOutcome, bool), lash_core::PluginError> {
        let registry = self.process_registry()?;
        let scoped = self
            .core
            .env
            .core
            .control
            .effect_host
            .scoped(lash_core::AdmittedScope::new(self.scope.clone()))
            .map_err(lash_core::PluginError::Runtime)?;
        #[expect(
            clippy::expect_used,
            reason = "the scope comes from the effect host's own `scoped` handle, which \
                      is admitted by construction"
        )]
        let invocation = lash_core::RuntimeEffectInvocation::new(
            lash_core::EffectAddress::new(scoped.execution_scope().clone(), replay_key.clone())
                .expect(
                    "the scope comes from the effect host's own `scoped` handle, which \
                     is admitted by construction",
                ),
            lash_core::RuntimeAttribution::for_session(self.session_id.clone()),
            format!("tool-intent-ingress:{}", identity.intent_index),
        )
        .with_replay_attribution(lash_core::RuntimeReplayAttribution::ToolIntent(
            identity.clone(),
        ));
        // Two independent ways this submission can fail to realize anything,
        // folded into the one `replayed` bit the host reads (FIG-3070):
        //
        //  * the effect journal replayed a recorded outcome, so local
        //    execution never ran and the observer never fires; and
        //  * local execution ran against a fresh journal and the *store*
        //    coalesced the write onto the durable key it already held.
        //
        // Only the store knows the second, so it reports its verdict through
        // the observer instead of the ingress inferring one from "did we
        // execute locally".
        let store_realization =
            std::sync::Arc::new(std::sync::Mutex::new(None::<lash_core::StoreRealization>));
        let outcome_observer: lash_core::ProcessOutcomeObserver = {
            let store_realization = std::sync::Arc::clone(&store_realization);
            std::sync::Arc::new(move |_, realization| {
                *store_realization.lock_recover() = Some(realization);
            })
        };
        let definition_command = matches!(
            &command,
            lash_core::ProcessCommand::PublishDefinition { .. }
                | lash_core::ProcessCommand::GetDefinition { .. }
        );
        let local_executor = if definition_command {
            lash_core::RuntimeEffectLocalExecutor::definition_artifacts(
                self.core.host_process_engines.clone(),
                lash_core::ReferrerClaim::guarded(lash_core::ReferrerGuard::Journal(
                    self.scope
                        .journal_identity()
                        .map_err(|error| lash_core::PluginError::Session(error.to_string()))?,
                )),
            )
        } else {
            lash_core::RuntimeEffectLocalExecutor::processes(
                registry,
                std::sync::Arc::clone(self.core.substrate_slot.ports().await.process.port()),
                self.core.host_process_engines.clone(),
                lash_core::runtime::HostStartAdmission {
                    tracing: Some(self.core.env.core.tracing.clone()),
                    session_catalog: Some(std::sync::Arc::clone(&self.core.store_factory) as _),
                    session_turn_admission: None,
                },
            )
            .with_process_attachments(self.core.backend.attachment_referrers())
            .with_process_starts(
                self.core
                    .backend
                    .obligation_ledger(lash_core::store::ObligationKind::ProcessStart),
                std::sync::Arc::clone(&self.core.env.core.clock),
                self.core.env.core.control.relay_policy(),
                self.core.env.core.tracing.metrics().clone(),
            )
            .with_process_env_store(std::sync::Arc::clone(
                &self.core.env.core.durability.process_env_store,
            ))
            .with_process_outcome_observer(outcome_observer)
        };
        let outcome = scoped
            .process_effect(
                lash_core::RuntimeEffectEnvelope::new(
                    invocation,
                    lash_core::RuntimeEffectCommand::process(command),
                ),
                local_executor,
            )
            .await
            // Kept typed rather than flattened to prose: the durable-identity
            // refusal travels as a `RuntimeErrorCode`, and `realization_failure`
            // reads that code to produce the shared `DuplicateIdentity`
            // vocabulary.
            .map_err(lash_core::PluginError::RuntimeEffectController)?;
        let lash_core::RuntimeEffectOutcome::Process { result } = outcome else {
            return Err(lash_core::PluginError::Session(
                "tool-intent ingress effect returned a non-process outcome".to_string(),
            ));
        };
        let replayed = match *store_realization.lock_recover() {
            // Local execution never ran: the journal replayed this effect.
            None => true,
            Some(realization) => realization.is_coalesced(),
        };
        Ok((result, replayed))
    }
}
