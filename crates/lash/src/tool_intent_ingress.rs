//! Host front-door admission for durable tool intents.

use lash_core::facade_support::ScopedEffectControllerFacadeOps;
use tracing::Instrument;

/// Typed idempotency key for one host-submitted tool intent.
///
/// Its identity is exactly `(session_id, execution_scope_id, tool_call_id,
/// intent_index)`; the replay key is derived by Lash and validated again at
/// submission.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct ToolIntentIngressKey(lash_core::ToolIntentIdentity);

impl ToolIntentIngressKey {
    /// Derive the only valid key for an identity quadruple.
    pub fn derive(
        session_id: impl AsRef<str>,
        execution_scope_id: impl AsRef<str>,
        tool_call_id: impl AsRef<str>,
        intent_index: u32,
    ) -> Self {
        let session_id = session_id.as_ref();
        let execution_scope_id = execution_scope_id.as_ref();
        let tool_call_id = tool_call_id.as_ref();
        let identity = lash_core::derive_tool_intent_identity(
            session_id,
            execution_scope_id,
            Some(tool_call_id),
            intent_index as usize,
        )
        .unwrap_or_else(|_| lash_core::ToolIntentIdentity {
            session_id: session_id.to_string(),
            execution_scope_id: execution_scope_id.to_string(),
            tool_call_id: tool_call_id.to_string(),
            intent_index,
            replay_key: String::new(),
        });
        Self(identity)
    }

    /// Rehydrate a transport-decoded identity for typed validation by
    /// [`ToolIntentIngress::submit`].
    ///
    /// Construction does not trust the embedded replay key; submission returns
    /// `MalformedKey` if it was forged or corrupted.
    pub fn from_identity(identity: lash_core::ToolIntentIdentity) -> Self {
        Self(identity)
    }

    /// Read the validated identity fields carried by this key.
    pub fn identity(&self) -> &lash_core::ToolIntentIdentity {
        &self.0
    }
}

/// Typed refusal returned before a host submission is admitted.
///
/// Refusals are data rather than string errors so transports can preserve
/// malformed and foreign-key distinctions.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum ToolIntentIngressRefusal {
    MalformedKey {
        expected_replay_key: String,
        recorded_replay_key: String,
    },
    ForeignSession {
        expected: String,
        recorded: String,
    },
    ForeignExecutionScope {
        expected: String,
        recorded: String,
    },
    IntentSessionMismatch {
        expected: String,
        recorded: String,
    },
    IdentityBoundToDifferentIntent {
        recorded_kind: lash_core::ToolIntentKind,
        submitted_kind: lash_core::ToolIntentKind,
    },
    DuplicateIdentity {
        kind: lash_core::ToolIntentKind,
    },
    RecordedOutcomeOutsideIntentProtocol {
        recorded: String,
    },
}

/// Admission result for one host-submitted intent.
///
/// On controller-owned, key-addressed tiers, repeating the same key returns the
/// first typed `outcome` with `replayed: true` and cannot realize a conflicting
/// payload twice.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ToolIntentIngressOutcome {
    Admitted {
        outcome: lash_core::ToolIntentExecutionOutcome,
        /// `false` when this submission realized the command and `true` when a
        /// controller-owned key-addressed journal returned an earlier outcome.
        replayed: bool,
    },
    Refused {
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
}

/// Session-and-scope-bound host front door for durable intent realization.
///
/// This is the sanctioned way for a host to submit a `ToolIntent` outside a
/// turn. Leaf tool bodies cannot construct this value; they return typed
/// `ToolIntents` to the attempt coordinator instead.
#[derive(Clone)]
pub struct ToolIntentIngress {
    core: crate::LashCore,
    session_id: String,
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
        session_id: impl Into<String>,
        scope: lash_core::ExecutionScope,
    ) -> crate::Result<ToolIntentIngress> {
        scope.validate()?;
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
        session_id: String,
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
    /// [`ToolIntentIngressRefusal::DuplicateIdentity`]. Ordinal-addressed tiers
    /// do not key-replay submissions, so the host must avoid resubmitting an
    /// identity as a new invocation.
    ///
    /// A `StartProcess` submission does not retain the host-chosen
    /// `request.id`: Lash replaces it with the derived intent replay key, and
    /// that derived key is the process id returned in the observable result.
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
        let identity = key.0;
        let submitted_intent = intent.clone();
        let (outcome, replayed) = match self.realize(&identity, intent).await {
            Ok((result, parent_end, replayed)) => (
                lash_core::ToolIntentExecutionOutcome::Executed {
                    identity: identity.clone(),
                    kind: result.0,
                    result: result.1,
                    parent_end,
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
                if self.core.env.core.control.effect_host.replay_ownership()
                    == lash_core::EffectReplayOwnership::Runtime
                    && let Ok(registry) = self.process_registry()
                    && let Err(store_error) = registry
                        .complete_tool_intent_submission(&identity.replay_key, outcome.clone())
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
        if self.core.env.core.control.effect_host.replay_ownership()
            == lash_core::EffectReplayOwnership::Controller
            && let Err(error) = self
                .retain_controller_owned_outcome(
                    identity.clone(),
                    submitted_intent,
                    outcome.clone(),
                )
                .await
        {
            return ToolIntentIngressOutcome::Admitted {
                outcome: lash_core::ToolIntentExecutionOutcome::Refused {
                    identity: Some(identity.clone()),
                    intent_index: identity.intent_index,
                    kind: outcome
                        .kind()
                        .unwrap_or(lash_core::ToolIntentKind::StartProcess),
                    refusal: lash_core::ToolIntentRefusalReason::CommandFailed {
                        code: "tool_intent_ingress_retention_failed".to_string(),
                        message: error.to_string(),
                    },
                },
                replayed,
            };
        }
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

    async fn retain_controller_owned_outcome(
        &self,
        identity: lash_core::ToolIntentIdentity,
        intent: lash_core::ToolIntent,
        outcome: lash_core::ToolIntentExecutionOutcome,
    ) -> crate::Result<()> {
        let registry = self.process_registry()?;
        let submission = lash_core::ToolIntentSubmissionRecord::new(identity.clone(), intent)
            .map_err(|error| {
                crate::EmbedError::Plugin(lash_core::PluginError::Session(format!(
                    "failed to hash admitted tool-intent submission: {error}"
                )))
            })?;
        match registry.admit_tool_intent_submission(submission).await? {
            lash_core::ToolIntentSubmissionAdmission::Admitted => {
                registry
                    .complete_tool_intent_submission(&identity.replay_key, outcome)
                    .await?;
            }
            lash_core::ToolIntentSubmissionAdmission::Existing(existing) => {
                if existing.outcome.is_none() {
                    registry
                        .complete_tool_intent_submission(&identity.replay_key, outcome)
                        .await?;
                }
            }
        }
        Ok(())
    }

    /// Apply and settle every durable parent-end action retained for this
    /// ingress's owning scope.
    ///
    /// Hosts call this operation when the bound scope ends. Lash reconstructs
    /// the same [`lash_core::ProcessParentEndPlan`] used by recorded process
    /// completion, applies only the ratified `Abandon` or `Cancel` policy with
    /// a replay-derived key, and marks each action settled after its typed
    /// outcome is known. A crash may therefore repeat the call safely.
    pub async fn settle_parent_end(
        &self,
    ) -> crate::Result<Vec<lash_core::ToolIntentParentEndOutcome>> {
        let registry = self.process_registry()?;
        let pending = registry
            .pending_tool_intent_parent_end(&self.session_id, self.scope.id())
            .await?;
        let plan = lash_core::ProcessParentEndPlan {
            process_id: self.scope.id().to_string(),
            actions: pending
                .iter()
                .filter_map(|submission| match submission.outcome.as_ref() {
                    Some(lash_core::ToolIntentExecutionOutcome::Executed {
                        identity,
                        parent_end: Some(parent_end),
                        ..
                    }) => Some(lash_core::ToolIntentParentEndAction {
                        identity: identity.clone(),
                        parent_end: parent_end.clone(),
                    }),
                    _ => None,
                })
                .collect(),
        };
        let mut outcomes = Vec::with_capacity(plan.actions.len());
        for action in plan.actions {
            let replay_key = format!("{}:parent-end", action.identity.replay_key);
            let command = lash_core::ProcessCommand::ParentEnd {
                identity: action.identity.clone(),
                process_id: action.parent_end.process_id.clone(),
                policy: action.parent_end.policy,
                reason: "host-ingress start intent parent scope ended".to_string(),
            };
            let (outcome, _) = self
                .run_command_with_replay_key(&action.identity, replay_key, command)
                .await?;
            let lash_core::ProcessEffectOutcome::ParentEnd { outcome } = outcome else {
                return Err(crate::EmbedError::Plugin(lash_core::PluginError::Session(
                    "tool-intent ingress parent-end command returned the wrong outcome".to_string(),
                )));
            };
            registry
                .complete_tool_intent_parent_end(&action.identity.replay_key)
                .await?;
            outcomes.push(*outcome);
        }
        Ok(outcomes)
    }

    fn validate(
        &self,
        key: &ToolIntentIngressKey,
        intent: &lash_core::ToolIntent,
    ) -> Option<ToolIntentIngressRefusal> {
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
                expected: self.session_id.clone(),
                recorded: identity.session_id.clone(),
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
                expected: self.session_id.clone(),
                recorded: intent.session_id().to_string(),
            });
        }
        None
    }

    fn expected_identity(
        identity: &lash_core::ToolIntentIdentity,
    ) -> Option<lash_core::ToolIntentIdentity> {
        lash_core::derive_tool_intent_identity(
            &identity.session_id,
            &identity.execution_scope_id,
            Some(&identity.tool_call_id),
            identity.intent_index as usize,
        )
        .ok()
    }

    async fn realize(
        &self,
        identity: &lash_core::ToolIntentIdentity,
        intent: lash_core::ToolIntent,
    ) -> std::result::Result<
        (
            (lash_core::ToolIntentKind, serde_json::Value),
            Option<lash_core::ToolIntentParentEnd>,
            bool,
        ),
        RealizationFailure,
    > {
        let kind = intent.kind();
        let runtime_owned = self.core.env.core.control.effect_host.replay_ownership()
            == lash_core::EffectReplayOwnership::Runtime;
        let _runtime_submission_guard = if runtime_owned {
            Some(
                self.core
                    .tool_intent_submission_gates
                    .lock(&identity.replay_key)
                    .await,
            )
        } else {
            None
        };
        let intent = if runtime_owned {
            let submitted =
                lash_core::ToolIntentSubmissionRecord::new(identity.clone(), intent.clone())
                    .map_err(|error| {
                        RealizationFailure::Command(
                            kind,
                            crate::EmbedError::Plugin(lash_core::PluginError::Session(format!(
                                "failed to hash tool-intent submission: {error}"
                            ))),
                        )
                    })?;
            match self
                .process_registry()
                .map_err(|error| RealizationFailure::Command(kind, error))?
                .admit_tool_intent_submission(submitted.clone())
                .await
                .map_err(|error| RealizationFailure::Command(kind, error.into()))?
            {
                lash_core::ToolIntentSubmissionAdmission::Admitted => intent,
                lash_core::ToolIntentSubmissionAdmission::Existing(existing) => {
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
                    existing.intent
                }
            }
        } else {
            intent
        };
        let (result, parent_end_policy, replayed) = self
            .realize_inner(identity, intent)
            .await
            .map_err(|error| RealizationFailure::Command(kind, error))?;
        let result = match result {
            RealizedIntent::Trigger(report) => {
                let value = serde_json::to_value(report).unwrap_or(serde_json::Value::Null);
                if kind != lash_core::ToolIntentKind::EmitTrigger {
                    return Err(RealizationFailure::Refused(
                        ToolIntentIngressRefusal::IdentityBoundToDifferentIntent {
                            recorded_kind: lash_core::ToolIntentKind::EmitTrigger,
                            submitted_kind: kind,
                        },
                    ));
                }
                self.complete_runtime_owned_submission(identity, kind, &value, None)
                    .await?;
                return Ok(((kind, value), None, replayed));
            }
            RealizedIntent::Process(result) => result,
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
            lash_core::ProcessEffectOutcome::ParentEnd { .. } => {
                return Err(Self::outside_protocol_outcome("parent_end"));
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
        let (value, parent_end) = match result {
            lash_core::ProcessEffectOutcome::Start { record } => {
                let summary = lash_core::ProcessHandleSummary::from_record(*record);
                let Some(policy) = parent_end_policy else {
                    return Err(RealizationFailure::Command(
                        kind,
                        crate::EmbedError::Plugin(lash_core::PluginError::Session(
                            "tool-intent ingress start outcome has no submitted parent-end policy"
                                .to_string(),
                        )),
                    ));
                };
                let parent_end = lash_core::ToolIntentParentEnd {
                    process_id: summary.id.clone(),
                    policy,
                };
                let (value, parent_end) = (
                    serde_json::to_value(summary).unwrap_or(serde_json::Value::Null),
                    Some(parent_end),
                );
                (value, parent_end)
            }
            lash_core::ProcessEffectOutcome::Signal { event } => (
                serde_json::to_value(*event).unwrap_or(serde_json::Value::Null),
                None,
            ),
            lash_core::ProcessEffectOutcome::Cancel { record } => (
                serde_json::to_value(lash_core::ProcessCancelSummary::from_record(*record))
                    .unwrap_or(serde_json::Value::Null),
                None,
            ),
            lash_core::ProcessEffectOutcome::EmitEvent { event, .. } => (
                serde_json::to_value(*event).unwrap_or(serde_json::Value::Null),
                None,
            ),
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
            lash_core::ProcessEffectOutcome::ParentEnd { .. } => {
                return Err(Self::outside_protocol_outcome("parent_end"));
            }
        };
        self.complete_runtime_owned_submission(identity, kind, &value, parent_end.clone())
            .await?;
        Ok(((kind, value), parent_end, replayed))
    }

    /// Record the first typed outcome for a runtime-owned submission. Every
    /// realized intent kind lands here so the durable submission row and the
    /// returned outcome never disagree.
    async fn complete_runtime_owned_submission(
        &self,
        identity: &lash_core::ToolIntentIdentity,
        kind: lash_core::ToolIntentKind,
        value: &serde_json::Value,
        parent_end: Option<lash_core::ToolIntentParentEnd>,
    ) -> std::result::Result<(), RealizationFailure> {
        if self.core.env.core.control.effect_host.replay_ownership()
            != lash_core::EffectReplayOwnership::Runtime
        {
            return Ok(());
        }
        let outcome = lash_core::ToolIntentExecutionOutcome::Executed {
            identity: identity.clone(),
            kind,
            result: value.clone(),
            parent_end,
        };
        self.process_registry()
            .map_err(|error| RealizationFailure::Command(kind, error))?
            .complete_tool_intent_submission(&identity.replay_key, outcome)
            .await
            .map_err(|error| RealizationFailure::Command(kind, error.into()))?;
        Ok(())
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
    ) -> crate::Result<(
        RealizedIntent,
        Option<lash_core::ProcessParentEndPolicy>,
        bool,
    )> {
        let mut parent_end_policy = None;
        let command = match intent {
            lash_core::ToolIntent::StartProcess(intent) => {
                parent_end_policy = Some(intent.on_parent_end);
                let mut request = intent.request;
                request.id = identity.replay_key.clone();
                let env_spec = request.env_spec.clone();
                let observers = request.observers.clone();
                let registration = request.into_registration(None);
                lash_core::ProcessCommand::Start {
                    registration,
                    observers,
                    env_spec,
                    execution_context: Box::new(lash_core::ProcessExecutionContext::default()),
                }
            }
            lash_core::ToolIntent::SignalProcess(intent) => {
                let event_type =
                    lash_core::facade_support::process_signal_event_type(&intent.signal_name)?;
                let request = lash_core::ProcessEventAppendRequest::new(event_type, intent.payload)
                    .with_replay_key(format!(
                        "process:{}:signal.{}:{}",
                        intent.process_id, intent.signal_name, identity.replay_key
                    ));
                lash_core::ProcessCommand::Signal {
                    process_id: intent.process_id,
                    signal_name: intent.signal_name,
                    signal_id: identity.replay_key.clone(),
                    request,
                }
            }
            lash_core::ToolIntent::CancelProcess(intent) => lash_core::ProcessCommand::Cancel {
                process_id: intent.process_id,
                reason: intent.reason,
                replay: Some(lash_core::RuntimeReplay {
                    key: identity.replay_key.clone(),
                    attribution: Some(lash_core::RuntimeReplayAttribution::ToolIntent(
                        identity.clone(),
                    )),
                }),
            },
            lash_core::ToolIntent::EmitProcessEvent(intent) => {
                let request =
                    lash_core::ProcessEventAppendRequest::new(intent.event_type, intent.payload)
                        .with_replay_key(identity.replay_key.clone());
                lash_core::ProcessCommand::EmitEvent {
                    process_id: intent.process_id,
                    request,
                }
            }
            lash_core::ToolIntent::EmitTrigger(intent) => {
                let report = self.emit_recorded_trigger(intent.request).await?;
                // The occurrence's idempotency key, not an effect-journal key,
                // is the dedupe point for a re-submitted trigger emission, so
                // this route never reports a journal replay.
                return Ok((RealizedIntent::Trigger(report), None, false));
            }
        };
        let (result, replayed) = self.run_command(identity, command).await?;
        Ok((RealizedIntent::Process(result), parent_end_policy, replayed))
    }

    /// Emit one recorded trigger declaration through the same router the
    /// runtime intent executor uses.
    async fn emit_recorded_trigger(
        &self,
        request: lash_core::TriggerOccurrenceRequest,
    ) -> crate::Result<lash_core::facade_support::TriggerEmitReport> {
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
        let drivers = self.core.work_driver.drivers().await;
        let router = lash_core::facade_support::TriggerRouter::new(
            store,
            self.core.env.process_registry.clone(),
            drivers.process,
        );
        let scoped = self
            .core
            .env
            .core
            .control
            .effect_host
            .scoped(self.scope.clone())?;
        router
            .emit(request, scoped.controller())
            .await
            .map_err(Into::into)
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
        let mut invocation = lash_core::RuntimeInvocation::effect(
            lash_core::runtime::RuntimeScope::new(&self.session_id),
            format!("tool-intent-ingress:{}", identity.intent_index),
            lash_core::RuntimeEffectKind::Process,
            replay_key.clone(),
        );
        invocation.replay = Some(lash_core::RuntimeReplay {
            key: replay_key,
            attribution: Some(lash_core::RuntimeReplayAttribution::ToolIntent(
                identity.clone(),
            )),
        });
        let realized_now = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let outcome_observer: lash_core::ProcessOutcomeObserver = {
            let realized_now = std::sync::Arc::clone(&realized_now);
            std::sync::Arc::new(move |_| {
                realized_now.store(true, std::sync::atomic::Ordering::SeqCst);
            })
        };
        let outcome = scoped
            .execute_process_effect(
                lash_core::RuntimeEffectEnvelope::new(
                    invocation,
                    lash_core::RuntimeEffectCommand::process(command),
                ),
                lash_core::RuntimeEffectLocalExecutor::processes(
                    registry,
                    self.core.env.process_work_driver.clone(),
                )
                .with_process_env_store(std::sync::Arc::clone(
                    &self.core.env.core.durability.process_env_store,
                ))
                .with_process_outcome_observer(outcome_observer),
            )
            .await
            .map_err(|error| {
                crate::EmbedError::Plugin(lash_core::PluginError::Session(error.to_string()))
            })?;
        let lash_core::RuntimeEffectOutcome::Process { result } = outcome else {
            return Err(crate::EmbedError::Plugin(lash_core::PluginError::Session(
                "tool-intent ingress effect returned a non-process outcome".to_string(),
            )));
        };
        Ok((
            result,
            !realized_now.load(std::sync::atomic::Ordering::SeqCst),
        ))
    }
}
