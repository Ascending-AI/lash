//! Host front-door admission for durable tool intents.

use lash_core::facade_support::ScopedEffectControllerFacadeOps;
use lash_sansio::SessionId;
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
    /// Derive the only valid key for an identity quadruple.
    pub fn derive(
        session_id: impl AsRef<str>,
        execution_scope_id: impl AsRef<str>,
        tool_call_id: impl AsRef<str>,
        intent_index: u32,
    ) -> Self {
        let session_id = SessionId::from(session_id.as_ref());
        let execution_scope_id = execution_scope_id.as_ref();
        let tool_call_id = tool_call_id.as_ref();
        let identity = lash_core::derive_tool_intent_identity(
            &session_id,
            execution_scope_id,
            Some(tool_call_id),
            intent_index as usize,
        )
        .unwrap_or_else(|_| lash_core::ToolIntentIdentity {
            session_id: session_id.clone(),
            execution_scope_id: execution_scope_id.to_string(),
            tool_call_id: tool_call_id.to_string(),
            intent_index,
            replay_key: String::new(),
            minting_emission_replay_key: None,
        });
        Self {
            protocol_version: lash_core::TOOL_INTENT_PROTOCOL_V3,
            identity,
        }
    }

    /// Read the validated identity fields carried by this key.
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
}

/// Admission result for one host-submitted intent.
///
/// On controller-owned, key-addressed tiers, repeating the same key returns the
/// first typed `outcome` with `replayed: true` and cannot realize a conflicting
/// payload twice -- whether the repeat is caught by the controller's effect
/// journal or, on a fresh invocation with an empty journal, by the durable key
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
        /// `true` covers both ways a submission can realize nothing: a
        /// controller-owned key-addressed journal returned an earlier outcome
        /// without reaching the store, or the store coalesced the write onto
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
/// Four of the five intent kinds are process commands; the fifth is a trigger
/// emission owned by the trigger router, which has no `ProcessEffectOutcome`.
enum RealizedIntent {
    Process(lash_core::ProcessEffectOutcome),
    Trigger(lash_core::facade_support::TriggerEmitReport),
    // Boxed: a registration receipt carries the whole admitted subscription
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
}

#[derive(Default)]
pub(crate) struct RuntimeSubmissionGates {
    by_replay_key: std::sync::Mutex<
        std::collections::HashMap<String, std::sync::Weak<tokio::sync::Mutex<()>>>,
    >,
}

impl RuntimeSubmissionGates {
    async fn lock(&self, replay_key: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let gate = {
            let mut gates = self
                .by_replay_key
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            gates.retain(|_, gate| gate.strong_count() > 0);
            if let Some(gate) = gates.get(replay_key).and_then(std::sync::Weak::upgrade) {
                gate
            } else {
                let gate = std::sync::Arc::new(tokio::sync::Mutex::new(()));
                gates.insert(replay_key.to_string(), std::sync::Arc::downgrade(&gate));
                gate
            }
        };
        gate.lock_owned().await
    }
}

fn ingress_runtime_error(error: crate::EmbedError) -> lash_core::RuntimeError {
    match error {
        crate::EmbedError::Plugin(lash_core::PluginError::Runtime(error)) => error,
        crate::EmbedError::Plugin(lash_core::PluginError::RuntimeEffectController(error)) => {
            let mut runtime = lash_core::RuntimeError::new(error.code, error.message);
            runtime.summary = error.summary;
            match error.cause {
                Some(cause) => runtime.with_cause(cause),
                None => runtime,
            }
        }
        error => {
            lash_core::RuntimeError::new(lash_core::RuntimeErrorCode::Plugin, error.to_string())
        }
    }
}

#[async_trait::async_trait]
impl lash_core::ToolIntentOutcomeSink for ToolIntentIngress {
    async fn lock_submission_gate(&self, replay_key: &str) -> lash_core::ToolIntentSubmissionGuard {
        lash_core::ToolIntentSubmissionGuard::from_owned_mutex_guard(
            self.core
                .tool_intent_submission_gates
                .lock(replay_key)
                .await,
        )
    }

    async fn admit(
        &self,
        record: lash_core::ToolIntentSubmissionRecord,
    ) -> Result<lash_core::ToolIntentSubmissionAdmission, lash_core::RuntimeError> {
        self.process_registry()
            .map_err(ingress_runtime_error)?
            .admit_tool_intent_submission(record)
            .await
            .map_err(|error| ingress_runtime_error(error.into()))
    }

    async fn complete_submission(
        &self,
        identity: &lash_core::ToolIntentIdentity,
        outcome: lash_core::ToolIntentExecutionOutcome,
    ) -> Result<(), lash_core::RuntimeError> {
        self.process_registry()
            .map_err(ingress_runtime_error)?
            .complete_tool_intent_submission(&identity.replay_key, outcome)
            .await
            .map(|_| ())
            .map_err(|error| ingress_runtime_error(error.into()))
    }

    async fn retain_in_journal(
        &self,
        identity: &lash_core::ToolIntentIdentity,
        submitted: lash_core::ToolIntent,
        outcome: lash_core::ToolIntentExecutionOutcome,
    ) -> Result<(), lash_core::RuntimeError> {
        let registry = self.process_registry().map_err(ingress_runtime_error)?;
        let submission = lash_core::ToolIntentSubmissionRecord::new(identity.clone(), submitted)
            .map_err(|error| {
                lash_core::RuntimeError::new(
                    lash_core::RuntimeErrorCode::RecordEncodingFailed,
                    format!("failed to hash admitted tool-intent submission: {error}"),
                )
            })?;
        match registry
            .admit_tool_intent_submission(submission)
            .await
            .map_err(|error| ingress_runtime_error(error.into()))?
        {
            lash_core::ToolIntentSubmissionAdmission::Admitted => {
                registry
                    .complete_tool_intent_submission(&identity.replay_key, outcome)
                    .await
                    .map_err(|error| ingress_runtime_error(error.into()))?;
            }
            lash_core::ToolIntentSubmissionAdmission::Existing(existing) => {
                if existing.outcome.is_none() {
                    registry
                        .complete_tool_intent_submission(&identity.replay_key, outcome)
                        .await
                        .map_err(|error| ingress_runtime_error(error.into()))?;
                }
            }
        }
        Ok(())
    }
}

enum RealizationFailure {
    Refused(ToolIntentIngressRefusal),
    Command(lash_core::ToolIntentKind, crate::EmbedError),
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
        session_id: impl Into<SessionId>,
        scope: lash_core::ExecutionScope,
    ) -> crate::Result<ToolIntentIngress> {
        scope.validate().map_err(lash_core::RuntimeError::from)?;
        let session_id = session_id.into();
        if session_id.trim().is_empty() {
            return Err(lash_core::RuntimeError::new(
                lash_core::RuntimeErrorCode::MissingExecutionScopeId,
                "tool-intent ingress requires a non-empty session id",
            )
            .into());
        }
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
        }
    }

    /// Derive an idempotency key bound to this ingress's actual session and
    /// execution scope.
    pub fn key(&self, tool_call_id: impl AsRef<str>, intent_index: u32) -> ToolIntentIngressKey {
        ToolIntentIngressKey::derive(
            &self.session_id,
            self.scope.id(),
            tool_call_id,
            intent_index,
        )
    }

    /// Submit one durable intent using first-writer-wins identity semantics.
    ///
    /// Validation happens before any process command. Admission and realization
    /// use the identity-derived replay key at the configured effect host, so a
    /// crash redrives the same command frame rather than creating a second
    /// realization. On a
    /// controller-owned key-addressed tier, reuse of an identity returns the
    /// first writer's outcome with `replayed: true`; the later payload is not
    /// realized. Runtime-owned tiers report process-store identity collisions as
    /// [`ToolIntentIngressRefusal::DuplicateIdentity`]. Controller-owned tiers
    /// report the same refusal: every shape lands on a durable key at the point
    /// it mutates, so a re-submitted identity realizes once and a changed
    /// payload under a bound identity is refused at the store. `CancelProcess`
    /// carries no content past its target, and its store fence lives on the
    /// target record, so its identity is bound to the target it first named in
    /// the durable submission ledger instead: a re-used identity naming a
    /// *different* target is refused there, before the second target is
    /// touched.
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
            session_id = %identity.session_id,
            execution_scope_id = %identity.execution_scope_id,
            tool_call_id = %identity.tool_call_id,
            intent_index = identity.intent_index,
            replay_key = %identity.replay_key,
            submitted_kind = %intent.kind().as_str(),
        );
        async {
            let outcome = self.submit_inner(key, intent).await;
            Self::record_decision(&identity, &outcome);
            outcome
        }
        .instrument(span)
        .await
    }

    async fn submit_inner(
        &self,
        key: ToolIntentIngressKey,
        intent: lash_core::ToolIntent,
    ) -> ToolIntentIngressOutcome {
        if let Some(refusal) = self.validate(&key, &intent) {
            return ToolIntentIngressOutcome::Refused { refusal };
        }
        let identity = key.identity;
        let submitted_intent = intent.clone();
        let (outcome, replayed) = match self.realize(&identity, intent).await {
            Ok((result, replayed)) => (
                lash_core::ToolIntentExecutionOutcome::Executed {
                    identity: identity.clone(),
                    kind: result.0,
                    result: result.1,
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
                        code: "tool_intent_ingress_realization_failed".to_string(),
                        message: error.to_string(),
                    },
                };
                if let Err(store_error) = self
                    .core
                    .env
                    .core
                    .control
                    .effect_host
                    .record_tool_intent_outcome(
                        self,
                        &identity,
                        submitted_intent.clone(),
                        outcome.clone(),
                    )
                    .await
                {
                    outcome = lash_core::ToolIntentExecutionOutcome::Refused {
                        identity: Some(identity.clone()),
                        intent_index: identity.intent_index,
                        kind,
                        refusal: lash_core::ToolIntentRefusalReason::CommandFailed {
                            code: "tool_intent_ingress_outcome_persist_failed".to_string(),
                            message: store_error.to_string(),
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
            session_id = %identity.session_id,
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
            ToolIntentIngressRefusal::IdentityBoundToDifferentIntent { .. } => {
                "identity_bound_to_different_intent"
            }
            ToolIntentIngressRefusal::DuplicateIdentity { .. } => "duplicate_identity",
            ToolIntentIngressRefusal::RecordedOutcomeOutsideIntentProtocol { .. } => {
                "recorded_outcome_outside_intent_protocol"
            }
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
        let expected = Self::expected_identity(identity);
        let expected_replay_key = expected
            .as_ref()
            .map(|identity| identity.replay_key.clone())
            .unwrap_or_default();
        if identity.tool_call_id.trim().is_empty()
            || expected
                .as_ref()
                .map(|expected| expected.replay_key != identity.replay_key)
                .unwrap_or(true)
        {
            return Some(ToolIntentIngressRefusal::MalformedKey {
                expected_replay_key,
                recorded_replay_key: identity.replay_key.clone(),
            });
        }
        if identity.session_id != self.session_id {
            return Some(ToolIntentIngressRefusal::ForeignSession {
                expected: self.session_id.to_string(),
                recorded: identity.session_id.to_string(),
            });
        }
        if identity.execution_scope_id != self.scope.id() {
            return Some(ToolIntentIngressRefusal::ForeignExecutionScope {
                expected: self.scope.id().to_string(),
                recorded: identity.execution_scope_id.clone(),
            });
        }
        if intent.session_id() != self.session_id {
            return Some(ToolIntentIngressRefusal::IntentSessionMismatch {
                expected: self.session_id.to_string(),
                recorded: intent.session_id().to_string(),
            });
        }
        None
    }

    /// The identity a well-formed record must carry, re-derived from its own
    /// durable fields by the single re-derivation constructor. Deriving it here
    /// instead dropped `minting_emission_replay_key` and so read every
    /// runtime-minted identity as forged (FIG-2994).
    fn expected_identity(
        identity: &lash_core::ToolIntentIdentity,
    ) -> Option<lash_core::ToolIntentIdentity> {
        lash_core::rederive_tool_intent_identity(identity).ok()
    }

    async fn realize(
        &self,
        identity: &lash_core::ToolIntentIdentity,
        intent: lash_core::ToolIntent,
    ) -> std::result::Result<
        ((lash_core::ToolIntentKind, serde_json::Value), bool),
        RealizationFailure,
    > {
        let kind = intent.kind();
        let submitted_intent = intent.clone();
        let preparation = self
            .core
            .env
            .core
            .control
            .effect_host
            .prepare_tool_intent(self, identity, intent.clone())
            .await
            .map_err(|error| {
                RealizationFailure::Command(
                    kind,
                    crate::EmbedError::Plugin(lash_core::PluginError::Runtime(error)),
                )
            })?;
        let intent = match &preparation {
            lash_core::ToolIntentPreparation::ControllerOwned => {
                self.bind_controller_owned_cancel_target(identity, &intent)
                    .await?;
                intent
            }
            lash_core::ToolIntentPreparation::RuntimeOwned {
                admission,
                _guard: _,
            } => {
                let submitted =
                    lash_core::ToolIntentSubmissionRecord::new(identity.clone(), intent.clone())
                        .map_err(|error| {
                            RealizationFailure::Command(
                                kind,
                                crate::EmbedError::Plugin(lash_core::PluginError::Session(
                                    format!("failed to hash tool-intent submission: {error}"),
                                )),
                            )
                        })?;
                match admission {
                    lash_core::ToolIntentSubmissionAdmission::Admitted => intent,
                    lash_core::ToolIntentSubmissionAdmission::Existing(existing) => {
                        if existing.protocol_version != lash_core::TOOL_INTENT_PROTOCOL_V3 {
                            return Err(RealizationFailure::Refused(
                                ToolIntentIngressRefusal::UnsupportedProtocolVersion {
                                    recorded: existing.protocol_version,
                                },
                            ));
                        }
                        if existing.kind != kind {
                            return Err(RealizationFailure::Refused(
                                ToolIntentIngressRefusal::IdentityBoundToDifferentIntent {
                                    recorded_kind: existing.kind,
                                    submitted_kind: kind,
                                },
                            ));
                        }
                        if existing.payload_hash != submitted.payload_hash {
                            return Err(RealizationFailure::Refused(
                                ToolIntentIngressRefusal::DuplicateIdentity { kind },
                            ));
                        }
                        if existing.outcome.is_some() {
                            return Err(RealizationFailure::Refused(
                                ToolIntentIngressRefusal::DuplicateIdentity { kind },
                            ));
                        }
                        existing.intent.clone()
                    }
                }
            }
        };
        let (result, replayed) = self
            .realize_inner(identity, intent)
            .await
            .map_err(|error| Self::realization_failure(kind, error))?;
        let trigger_result = match &result {
            RealizedIntent::Trigger(report) => Some((
                lash_core::ToolIntentKind::EmitTrigger,
                serde_json::to_value(report).unwrap_or(serde_json::Value::Null),
            )),
            RealizedIntent::TriggerRegistration(receipt) => Some((
                lash_core::ToolIntentKind::RegisterTrigger,
                serde_json::to_value(receipt).unwrap_or(serde_json::Value::Null),
            )),
            RealizedIntent::Process(_) => None,
        };
        let result = match trigger_result {
            Some((trigger_kind, value)) => {
                // `realize_inner` dispatches on the submitted intent, so this
                // pairing only breaks if an admitted submission row carries a
                // kind its own payload contradicts. The trigger routes have no
                // journal replay to cross-check, so the row is the only place
                // that corruption can come from; refuse rather than report a
                // trigger outcome under another kind.
                if kind != trigger_kind {
                    return Err(RealizationFailure::Refused(
                        ToolIntentIngressRefusal::IdentityBoundToDifferentIntent {
                            recorded_kind: trigger_kind,
                            submitted_kind: kind,
                        },
                    ));
                }
                let outcome = lash_core::ToolIntentExecutionOutcome::Executed {
                    identity: identity.clone(),
                    kind,
                    result: value.clone(),
                };
                self.core
                    .env
                    .core
                    .control
                    .effect_host
                    .record_tool_intent_outcome(self, identity, submitted_intent.clone(), outcome)
                    .await
                    .map_err(|error| {
                        RealizationFailure::Command(
                            kind,
                            crate::EmbedError::Plugin(lash_core::PluginError::Runtime(error)),
                        )
                    })?;
                return Ok(((kind, value), replayed));
            }
            None => match result {
                RealizedIntent::Process(result) => result,
                RealizedIntent::Trigger(_) | RealizedIntent::TriggerRegistration(_) => {
                    unreachable!("trigger outcomes are settled above")
                }
            },
        };
        let recorded_kind = match &result {
            lash_core::ProcessEffectOutcome::Start { .. } => {
                lash_core::ToolIntentKind::StartProcess
            }
            lash_core::ProcessEffectOutcome::Signal { .. } => {
                lash_core::ToolIntentKind::SignalProcess
            }
            lash_core::ProcessEffectOutcome::Cancel { .. } => {
                lash_core::ToolIntentKind::CancelProcess
            }
            lash_core::ProcessEffectOutcome::CancelRefused { .. } => {
                lash_core::ToolIntentKind::CancelProcess
            }
            lash_core::ProcessEffectOutcome::EmitEvent { .. } => {
                lash_core::ToolIntentKind::EmitProcessEvent
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
        };
        if recorded_kind != kind {
            return Err(RealizationFailure::Refused(
                ToolIntentIngressRefusal::IdentityBoundToDifferentIntent {
                    recorded_kind,
                    submitted_kind: kind,
                },
            ));
        }
        let value = match result {
            lash_core::ProcessEffectOutcome::Start { record } => {
                let summary = lash_core::ProcessHandleView::from_record(*record);
                serde_json::to_value(summary).unwrap_or(serde_json::Value::Null)
            }
            lash_core::ProcessEffectOutcome::Signal { event } => {
                serde_json::to_value(*event).unwrap_or(serde_json::Value::Null)
            }
            lash_core::ProcessEffectOutcome::Cancel { record } => serde_json::to_value(
                lash_core::ProcessCancelReceipt::from_record(*record).map_err(|error| {
                    RealizationFailure::Command(kind, crate::EmbedError::Plugin(error))
                })?,
            )
            .unwrap_or(serde_json::Value::Null),
            lash_core::ProcessEffectOutcome::CancelRefused { refusal } => {
                return Err(RealizationFailure::Command(
                    kind,
                    crate::EmbedError::Plugin(refusal),
                ));
            }
            lash_core::ProcessEffectOutcome::EmitEvent { event, .. } => {
                serde_json::to_value(*event).unwrap_or(serde_json::Value::Null)
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
        };
        let outcome = lash_core::ToolIntentExecutionOutcome::Executed {
            identity: identity.clone(),
            kind,
            result: value.clone(),
        };
        self.core
            .env
            .core
            .control
            .effect_host
            .record_tool_intent_outcome(self, identity, submitted_intent, outcome)
            .await
            .map_err(|error| {
                RealizationFailure::Command(
                    kind,
                    crate::EmbedError::Plugin(lash_core::PluginError::Runtime(error)),
                )
            })?;
        Ok(((kind, value), replayed))
    }

    /// Bind a controller-owned `CancelProcess` identity to the target it first
    /// named.
    ///
    /// The other four shapes carry their content into the durable key they
    /// land on — a registration fingerprint, an event replay key, an
    /// occurrence idempotency key — so a re-used identity carrying different
    /// content is refused by the store itself. A cancel carries nothing but
    /// its target, and its fence lives on the *target* record: a bound
    /// identity re-submitted against a second process finds that record
    /// unfenced and cancels it. Nothing on the first target can see that
    /// (FIG-3072).
    ///
    /// The binding is therefore taken where the runtime-owned tier takes it:
    /// the durable tool-intent submission ledger, which this ingress already
    /// writes on the controller-owned tier through
    /// [`retain_in_journal`](lash_core::ToolIntentOutcomeSink::retain_in_journal)
    /// once an outcome exists. Claiming the row *before* realization instead
    /// is what makes the target durable across invocations: the redelivery
    /// arrives with an empty effect journal, reads the row the first
    /// invocation left, and compares payload hashes.
    ///
    /// A matching payload is not refused, unlike on the runtime-owned tier: a
    /// redelivered invocation legitimately re-presents its own submission, and
    /// the target record coalesces it onto the recorded request. Only a
    /// changed payload — for a cancel, only a changed target — is refused, as
    /// [`ToolIntentIngressRefusal::DuplicateIdentity`], the same vocabulary
    /// both other shapes and the runtime-owned tier use.
    async fn bind_controller_owned_cancel_target(
        &self,
        identity: &lash_core::ToolIntentIdentity,
        intent: &lash_core::ToolIntent,
    ) -> std::result::Result<(), RealizationFailure> {
        let kind = intent.kind();
        if kind != lash_core::ToolIntentKind::CancelProcess {
            return Ok(());
        }
        let submitted =
            lash_core::ToolIntentSubmissionRecord::new(identity.clone(), intent.clone()).map_err(
                |error| {
                    RealizationFailure::Command(
                        kind,
                        crate::EmbedError::Plugin(lash_core::PluginError::Session(format!(
                            "failed to hash tool-intent submission: {error}"
                        ))),
                    )
                },
            )?;
        use lash_core::ToolIntentOutcomeSink as _;
        let _guard = self.lock_submission_gate(&identity.replay_key).await;
        let admission = self.admit(submitted.clone()).await.map_err(|error| {
            RealizationFailure::Command(
                kind,
                crate::EmbedError::Plugin(lash_core::PluginError::Runtime(error)),
            )
        })?;
        let lash_core::ToolIntentSubmissionAdmission::Existing(existing) = admission else {
            return Ok(());
        };
        if existing.protocol_version != lash_core::TOOL_INTENT_PROTOCOL_V3 {
            return Err(RealizationFailure::Refused(
                ToolIntentIngressRefusal::UnsupportedProtocolVersion {
                    recorded: existing.protocol_version,
                },
            ));
        }
        if existing.kind != kind {
            return Err(RealizationFailure::Refused(
                ToolIntentIngressRefusal::IdentityBoundToDifferentIntent {
                    recorded_kind: existing.kind,
                    submitted_kind: kind,
                },
            ));
        }
        if existing.payload_hash != submitted.payload_hash {
            return Err(RealizationFailure::Refused(
                ToolIntentIngressRefusal::DuplicateIdentity { kind },
            ));
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
    /// [`lash_core::durable_identity_conflict`], and that refusal is the same
    /// fact the runtime-owned tier reports from its submission ledger. Mapping
    /// it here is what gives hosts one refusal vocabulary across both tiers
    /// (FIG-1489) instead of a typed refusal on one and a generic command
    /// failure on the other.
    fn realization_failure(
        kind: lash_core::ToolIntentKind,
        error: crate::EmbedError,
    ) -> RealizationFailure {
        if let crate::EmbedError::Plugin(plugin) = &error
            && lash_core::is_durable_identity_conflict(plugin)
        {
            return RealizationFailure::Refused(ToolIntentIngressRefusal::DuplicateIdentity {
                kind,
            });
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
    ) -> crate::Result<(RealizedIntent, bool)> {
        let command = match intent {
            lash_core::ToolIntent::StartProcess(intent) => {
                // The declaration carries no id. The replay key is the process
                // id, so a re-submitted declaration starts the same process:
                // one constructor, shared with core's recorded-intent seam
                // (FIG-2876, FIG-2994).
                let request = intent.into_request(identity);
                let env_spec = request.env_spec.clone();
                let observers = request.observers.clone();
                let registration = self
                    .admit_engine_start(request.into_registration(None), env_spec.as_ref())
                    .await?;
                lash_core::ProcessCommand::Start {
                    registration,
                    observers,
                    env_spec,
                    execution_context: Box::new(lash_core::ProcessExecutionContext::default()),
                }
            }
            lash_core::ToolIntent::SignalProcess(intent) => {
                let process_ref = self
                    .process_registry()?
                    .resolve_process_ref(&intent.process_id)
                    .await?;
                let event_type =
                    lash_core::facade_support::process_signal_event_type(&intent.signal_name)?;
                // Core's recorded-intent seam mints the same key from the same
                // three parts; both routes call the one constructor (FIG-2876).
                let request = lash_core::ProcessEventAppendRequest::new(event_type, intent.payload)
                    .with_replay_key(lash_core::facade_support::process_signal_wait_key(
                        &intent.process_id,
                        &intent.signal_name,
                        &identity.replay_key,
                    ));
                lash_core::ProcessCommand::Signal {
                    process_ref,
                    signal_name: intent.signal_name,
                    signal_id: identity.replay_key.clone(),
                    request,
                }
            }
            lash_core::ToolIntent::CancelProcess(intent) => {
                let process_ref = self
                    .process_registry()?
                    .resolve_process_ref(&intent.process_id)
                    .await?;
                // Same stamping core's recorded-intent cancel seam applies
                // (`runtime/session_manager/process_runners/control.rs`): the
                // replay key requests the cancel and the whole identity is the
                // replay attribution. Not a separate contract — the broad
                // `ProcessCommand` type is why it is spelled again here.
                lash_core::ProcessCommand::Cancel {
                    process_ref,
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
            lash_core::ToolIntent::RegisterProcessDefinition(intent) => {
                // The definition registry table is a separate child of
                // FIG-2990; the declaration is admitted and identified here,
                // and realization refuses until that table exists.
                return Err(crate::EmbedError::Plugin(lash_core::PluginError::Session(
                    format!(
                        "process definition registry is unavailable in this runtime: \
                         cannot register a `{}` definition",
                        intent.engine_kind
                    ),
                )));
            }
            lash_core::ToolIntent::RegisterTrigger(intent) => {
                let receipt = self
                    .register_recorded_trigger(identity, intent.draft)
                    .await?;
                return Ok((
                    RealizedIntent::TriggerRegistration(Box::new(receipt)),
                    false,
                ));
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
        draft: lash_core::TriggerSubscriptionDraft,
    ) -> crate::Result<lash_core::TriggerMutationReceipt> {
        let store = self
            .core
            .env
            .trigger_store
            .as_ref()
            .cloned()
            .ok_or_else(|| {
                crate::EmbedError::Plugin(lash_core::PluginError::Session(
                    "trigger store is unavailable in this runtime".to_string(),
                ))
            })?;
        let scoped = self
            .core
            .env
            .core
            .control
            .effect_host
            .scoped(self.scope.clone())?;
        let invocation = lash_core::RuntimeEffectInvocation::new(
            lash_core::EffectAddress::new(
                scoped.execution_scope().clone(),
                identity.replay_key.clone(),
            )
            .map_err(|error| {
                crate::EmbedError::Plugin(lash_core::PluginError::Session(error.to_string()))
            })?,
            lash_core::RuntimeAttribution::for_session(self.session_id.clone()),
            identity.replay_key.clone(),
        )
        .with_replay_attribution(lash_core::RuntimeReplayAttribution::ToolIntent(
            identity.clone(),
        ));
        let session_scope = lash_core::SessionScope::new(self.session_id.clone());
        let outcome = scoped
            .execute_effect(
                lash_core::RuntimeEffectEnvelope::new(
                    invocation,
                    lash_core::RuntimeEffectCommand::Trigger {
                        command: Box::new(lash_core::TriggerCommand::Register {
                            owner_scope: lash_core::TriggerOwnerScope::session(
                                self.session_id.clone(),
                            ),
                            actor: lash_core::ProcessOriginator::session(session_scope),
                            draft,
                        }),
                    },
                ),
                lash_core::RuntimeEffectLocalExecutor::triggers(store),
            )
            .await
            .map_err(|error| {
                crate::EmbedError::Plugin(lash_core::PluginError::RuntimeEffectController(error))
            })?
            .into_trigger()
            .map_err(|error| {
                crate::EmbedError::Plugin(lash_core::PluginError::RuntimeEffectController(error))
            })?
            .map_err(|error| {
                crate::EmbedError::Plugin(lash_core::PluginError::Session(error.to_string()))
            })?;
        match outcome {
            lash_core::TriggerCommandOutcome::Mutation { receipt } => Ok(*receipt),
            other => Err(crate::EmbedError::Plugin(lash_core::PluginError::Session(
                format!("trigger registration returned a non-mutation outcome: {other:?}"),
            ))),
        }
    }

    /// Emit one recorded trigger declaration through the same router the
    /// runtime intent executor uses.
    async fn emit_recorded_trigger(
        &self,
        request: lash_core::TriggerOccurrenceRequest,
    ) -> crate::Result<(
        lash_core::facade_support::TriggerEmitReport,
        lash_core::StoreRealization,
    )> {
        let store = self
            .core
            .env
            .trigger_store
            .as_ref()
            .cloned()
            .ok_or_else(|| {
                crate::EmbedError::Plugin(lash_core::PluginError::Session(
                    "trigger store is unavailable in this runtime".to_string(),
                ))
            })?;
        let ports = self.core.substrate_slot.ports().await;
        let process_work = ports
            .process
            .ok_or(crate::EmbedError::MissingProcessRegistry)?;
        let router = lash_core::facade_support::TriggerRouter::new(store, process_work)
            .with_process_artifacts(
                std::sync::Arc::clone(&self.core.env.core.durability.process_env_store),
                self.core.host_process_engines.clone(),
            );
        let scoped = self
            .core
            .env
            .core
            .control
            .effect_host
            .scoped(self.scope.clone())?;
        router
            .emit_recorded_reporting_realization(request, &scoped)
            .await
            .map_err(Into::into)
    }

    /// Run the engine's pure admission gate over a host-submitted start using
    /// the exact execution environment recorded on the request. The gate may
    /// use that immutable environment to derive identity, but cannot inspect a
    /// live catalog or artifact store, so replaying the same intent is safe.
    async fn admit_engine_start(
        &self,
        registration: lash_core::ProcessRegistration,
        env_spec: Option<&lash_core::ProcessExecutionEnvSpec>,
    ) -> crate::Result<lash_core::ProcessRegistration> {
        let lash_core::ProcessInput::Engine { kind, payload } = registration.input.as_ref() else {
            return Ok(registration);
        };
        // `LashCore::env.core` deliberately carries no engines: every runtime
        // construction site installs the plugin-contributed ones onto a clean
        // clone (see `LashCoreBuilder::build`). Resolve the same way a session
        // open does, or a plugin-contributed kind would be refused here as
        // unregistered.
        let engines = self.resolved_process_engines()?;
        let identity = engines.admit(kind, payload, env_spec).await?;
        Ok(registration.with_admitted_identity(identity))
    }

    /// The engine registry a session opened on this core would see: this core's
    /// directly-wired engines plus the plugin-contributed ones.
    fn resolved_process_engines(
        &self,
    ) -> crate::Result<lash_core::facade_support::ProcessEngineRegistry> {
        Ok(self.core.host_process_engines.clone())
    }

    fn process_registry(&self) -> crate::Result<std::sync::Arc<dyn lash_core::ProcessRegistry>> {
        self.core
            .env
            .process_registry
            .as_ref()
            .cloned()
            .ok_or_else(|| {
                crate::EmbedError::Plugin(lash_core::PluginError::Session(
                    "process registry is unavailable in this runtime".to_string(),
                ))
            })
    }

    async fn run_command(
        &self,
        identity: &lash_core::ToolIntentIdentity,
        command: lash_core::ProcessCommand,
    ) -> crate::Result<(lash_core::ProcessEffectOutcome, bool)> {
        self.run_command_with_replay_key(identity, identity.replay_key.clone(), command)
            .await
    }

    async fn run_command_with_replay_key(
        &self,
        identity: &lash_core::ToolIntentIdentity,
        replay_key: String,
        command: lash_core::ProcessCommand,
    ) -> crate::Result<(lash_core::ProcessEffectOutcome, bool)> {
        let registry = self.process_registry()?;
        let scoped = self
            .core
            .env
            .core
            .control
            .effect_host
            .scoped(self.scope.clone())?;
        let invocation = lash_core::RuntimeEffectInvocation::new(
            lash_core::EffectAddress::new(scoped.execution_scope().clone(), replay_key.clone())
                .expect("tool intent ingress carries an admitted effect scope"),
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
                *store_realization
                    .lock()
                    .expect("tool intent ingress realization verdict is never poisoned") =
                    Some(realization);
            })
        };
        let outcome = scoped
            .execute_process_effect(
                lash_core::RuntimeEffectEnvelope::new(
                    invocation,
                    lash_core::RuntimeEffectCommand::process(command),
                ),
                lash_core::RuntimeEffectLocalExecutor::processes(registry, {
                    let ports = self.core.substrate_slot.ports().await;
                    self.core
                        .env
                        .clone()
                        .with_work_ports(ports.process.clone(), ports.queued_port())
                        .process_work()
                        .ok_or(crate::EmbedError::MissingProcessRegistry)?
                })
                .with_process_env_store(std::sync::Arc::clone(
                    &self.core.env.core.durability.process_env_store,
                ))
                .with_process_engines(self.core.host_process_engines.clone())
                .with_process_outcome_observer(outcome_observer),
            )
            .await
            // Kept typed rather than flattened to prose: the durable-identity
            // refusal travels as a `RuntimeErrorCode`, and `realization_failure`
            // reads that code to produce the shared `DuplicateIdentity`
            // vocabulary.
            .map_err(|error| {
                crate::EmbedError::Plugin(lash_core::PluginError::RuntimeEffectController(error))
            })?;
        let lash_core::RuntimeEffectOutcome::Process { result } = outcome else {
            return Err(crate::EmbedError::Plugin(lash_core::PluginError::Session(
                "tool-intent ingress effect returned a non-process outcome".to_string(),
            )));
        };
        let replayed = match *store_realization
            .lock()
            .expect("tool intent ingress realization verdict is never poisoned")
        {
            // Local execution never ran: the journal replayed this effect.
            None => true,
            Some(realization) => realization.is_coalesced(),
        };
        Ok((result, replayed))
    }
}
