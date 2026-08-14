//! Host front-door admission for durable tool intents.

use lash_core::facade_support::ScopedEffectControllerFacadeOps;

/// Typed idempotency key for one host-submitted tool intent.
///
/// This is ADR 0051's host-facade class. Its identity is exactly
/// `(session_id, execution_scope_id, tool_call_id, intent_index)`; the replay
/// key is derived by Lash and validated again at submission.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct ToolIntentIngressKey(lash_core::ToolIntentIdentity);

impl ToolIntentIngressKey {
    /// Derive the only valid key for an identity quadruple.
    ///
    /// This is ADR 0051's host-facade class.
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
    /// This is ADR 0051's host-facade class. Construction does not trust the
    /// embedded replay key; submission returns `MalformedKey` if it was forged
    /// or corrupted.
    pub fn from_identity(identity: lash_core::ToolIntentIdentity) -> Self {
        Self(identity)
    }

    /// Read the validated identity fields carried by this key.
    ///
    /// This is ADR 0051's host-facade class.
    pub fn identity(&self) -> &lash_core::ToolIntentIdentity {
        &self.0
    }
}

/// Typed refusal returned before a host submission is admitted.
///
/// This is ADR 0051's host-facade class. Refusals are data rather than string
/// errors so transports can preserve malformed and foreign-key distinctions.
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
}

/// Admission result for one host-submitted intent.
///
/// This is ADR 0051's host-facade class. On controller-owned, key-addressed
/// tiers, repeating the same key returns the first typed `outcome` with
/// `replayed: true` and cannot realize a conflicting payload twice.
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

/// Session-and-scope-bound host front door for durable intent realization.
///
/// This is ADR 0051's host-facade class and THE sanctioned way for a host to
/// submit a `ToolIntent` outside a turn. Leaf tool bodies cannot construct this
/// value; they return typed `ToolIntents` to the attempt coordinator instead.
#[derive(Clone)]
pub struct ToolIntentIngress {
    core: crate::LashCore,
    session_id: String,
    scope: lash_core::ExecutionScope,
}

enum RealizationFailure {
    Refused(ToolIntentIngressRefusal),
    Command(lash_core::ToolIntentKind, crate::EmbedError),
}

enum RuntimeDuplicateProbe {
    Process {
        process_id: String,
    },
    Event {
        process_id: String,
        replay_key: String,
    },
}

impl crate::LashCore {
    /// Bind the sanctioned host ingress for durable tool intents to one actual
    /// session and execution scope.
    ///
    /// This is ADR 0051's host-facade class: hosts submit durable leaf-style
    /// declarations here when no tool attempt owns the call. Tool bodies return
    /// [`crate::tools::ToolIntents`] instead and never call this front door.
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
    ///
    /// This is ADR 0051's host-facade class.
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
    /// This is ADR 0051's host-facade class. Validation happens before any
    /// process command. Admission and realization use the identity-derived
    /// replay key at the configured effect host, so a crash redrives the same
    /// command frame rather than creating a second realization. On a
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
        if let Some(refusal) = self.validate(&key, &intent) {
            return ToolIntentIngressOutcome::Refused { refusal };
        }
        let identity = key.0;
        let (outcome, replayed) = match self.realize(&identity, intent).await {
            Ok((result, parent_end, replayed)) => (
                lash_core::ToolIntentExecutionOutcome::Executed {
                    identity,
                    kind: result.0,
                    result: result.1,
                    parent_end,
                },
                replayed,
            ),
            Err(RealizationFailure::Refused(refusal)) => {
                return ToolIntentIngressOutcome::Refused { refusal };
            }
            Err(RealizationFailure::Command(kind, error)) => (
                lash_core::ToolIntentExecutionOutcome::Refused {
                    identity: Some(identity.clone()),
                    intent_index: identity.intent_index,
                    kind,
                    refusal: lash_core::ToolIntentRefusalReason::CommandFailed {
                        code: "tool_intent_ingress_realization_failed".to_string(),
                        message: error.to_string(),
                    },
                },
                false,
            ),
        };
        ToolIntentIngressOutcome::Admitted { outcome, replayed }
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
        let duplicate_probe = self.runtime_duplicate_probe(identity, &intent);
        if let Some(probe) = duplicate_probe.as_ref()
            && self
                .runtime_duplicate_exists(probe)
                .await
                .map_err(|error| RealizationFailure::Command(kind, error))?
        {
            return Err(RealizationFailure::Refused(
                ToolIntentIngressRefusal::DuplicateIdentity { kind },
            ));
        }
        let (result, parent_end_policy, replayed) = match self.realize_inner(identity, intent).await
        {
            Ok(realized) => realized,
            Err(error) => {
                if let Some(probe) = duplicate_probe.as_ref()
                    && self
                        .runtime_duplicate_exists(probe)
                        .await
                        .map_err(|probe_error| RealizationFailure::Command(kind, probe_error))?
                {
                    return Err(RealizationFailure::Refused(
                        ToolIntentIngressRefusal::DuplicateIdentity { kind },
                    ));
                } else {
                    return Err(RealizationFailure::Command(kind, error));
                }
            }
        };
        let recorded_kind = match &result {
            lash_core::ProcessEffectOutcome::Start { .. } => {
                Some(lash_core::ToolIntentKind::StartProcess)
            }
            lash_core::ProcessEffectOutcome::Signal { .. } => {
                Some(lash_core::ToolIntentKind::SignalProcess)
            }
            lash_core::ProcessEffectOutcome::Cancel { .. } => {
                Some(lash_core::ToolIntentKind::CancelProcess)
            }
            lash_core::ProcessEffectOutcome::EmitEvent { .. } => {
                Some(lash_core::ToolIntentKind::EmitProcessEvent)
            }
            _ => None,
        };
        let Some(recorded_kind) = recorded_kind else {
            return Err(RealizationFailure::Command(
                kind,
                crate::EmbedError::Plugin(lash_core::PluginError::Session(
                    "tool-intent ingress received the wrong process outcome".to_string(),
                )),
            ));
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
                (
                    serde_json::to_value(summary).unwrap_or(serde_json::Value::Null),
                    Some(parent_end),
                )
            }
            lash_core::ProcessEffectOutcome::Signal { event }
            | lash_core::ProcessEffectOutcome::EmitEvent { event, .. } => (
                serde_json::to_value(*event).unwrap_or(serde_json::Value::Null),
                None,
            ),
            lash_core::ProcessEffectOutcome::Cancel { record } => (
                serde_json::to_value(lash_core::ProcessCancelSummary::from_record(*record))
                    .unwrap_or(serde_json::Value::Null),
                None,
            ),
            _ => {
                return Err(RealizationFailure::Command(
                    kind,
                    crate::EmbedError::Plugin(lash_core::PluginError::Session(
                        "tool-intent ingress received the wrong process outcome".to_string(),
                    )),
                ));
            }
        };
        Ok(((kind, value), parent_end, replayed))
    }

    async fn realize_inner(
        &self,
        identity: &lash_core::ToolIntentIdentity,
        intent: lash_core::ToolIntent,
    ) -> crate::Result<(
        lash_core::ProcessEffectOutcome,
        Option<lash_core::ProcessParentEndPolicy>,
        bool,
    )> {
        let mut parent_end_policy = None;
        let command = match intent {
            lash_core::ToolIntent::StartProcess(intent) => {
                parent_end_policy = Some(intent.on_parent_end);
                let mut request = intent.request;
                request.id = identity.replay_key.clone();
                let env_ref = match request.env_spec.as_ref() {
                    Some(env_spec) => Some(
                        lash_core::runtime::persist_process_execution_env(
                            self.core.env.core.durability.process_env_store.as_ref(),
                            env_spec,
                        )
                        .await?,
                    ),
                    None => None,
                };
                let observers = request.observers.clone();
                let registration = request.into_registration(env_ref);
                lash_core::ProcessCommand::Start {
                    registration,
                    observers,
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
        };
        let (result, replayed) = self.run_command(identity, command).await?;
        Ok((result, parent_end_policy, replayed))
    }

    fn runtime_duplicate_probe(
        &self,
        identity: &lash_core::ToolIntentIdentity,
        intent: &lash_core::ToolIntent,
    ) -> Option<RuntimeDuplicateProbe> {
        if self.core.env.core.control.effect_host.replay_ownership()
            != lash_core::EffectReplayOwnership::Runtime
        {
            return None;
        }
        match intent {
            lash_core::ToolIntent::StartProcess(_) => Some(RuntimeDuplicateProbe::Process {
                process_id: identity.replay_key.clone(),
            }),
            lash_core::ToolIntent::SignalProcess(intent) => Some(RuntimeDuplicateProbe::Event {
                process_id: intent.process_id.clone(),
                replay_key: format!(
                    "process:{}:signal.{}:{}",
                    intent.process_id, intent.signal_name, identity.replay_key
                ),
            }),
            lash_core::ToolIntent::EmitProcessEvent(intent) => Some(RuntimeDuplicateProbe::Event {
                process_id: intent.process_id.clone(),
                replay_key: identity.replay_key.clone(),
            }),
            lash_core::ToolIntent::CancelProcess(_) => None,
        }
    }

    async fn runtime_duplicate_exists(&self, probe: &RuntimeDuplicateProbe) -> crate::Result<bool> {
        let registry = self.process_registry()?;
        match probe {
            RuntimeDuplicateProbe::Process { process_id } => {
                Ok(registry.get_process(process_id).await?.is_some())
            }
            RuntimeDuplicateProbe::Event {
                process_id,
                replay_key,
            } => Ok(registry
                .events_after(process_id, 0)
                .await?
                .iter()
                .any(|event| event.invocation.replay_key() == Some(replay_key.as_str()))),
        }
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
            identity.replay_key.clone(),
        );
        invocation.replay = Some(lash_core::RuntimeReplay {
            key: identity.replay_key.clone(),
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
