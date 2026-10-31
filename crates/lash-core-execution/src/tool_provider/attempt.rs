//! The services and recorded launch facts of one admitted tool attempt.
use super::*;

impl<'run> AttemptContext<'run> {
    /// Immutable history of the run that admitted this call, when it has one.
    pub fn read_history(&self) -> Option<&crate::ReadHistory> {
        self.history.as_deref()
    }

    /// Trusted origin of callable code retained by the execution admitting this attempt.
    pub fn callable_origin(
        &self,
    ) -> Result<lash_vocabulary::package::CallableOrigin, ResolveTargetError> {
        let environment = self.execution_environment().ok_or_else(|| {
            ResolveTargetError::Refused(
                "inline callable has no retained execution environment".into(),
            )
        })?;
        Ok(lash_vocabulary::package::CallableOrigin {
            publications: environment
                .publications
                .iter()
                .map(lash_vocabulary::package::PublicationRecord::key)
                .collect(),
            authored: environment.authored.clone(),
        })
    }

    /// Request already-linked engine code under its retained defining origin.
    ///
    /// The payload carries the defining Link and authenticated origin identities;
    /// the receiving engine validates and retains them from current store bytes.
    /// The attempt's consumer context accompanies those facts, without copying
    /// unrelated namespace advertisements into the durable start declaration.
    /// Unlinked source instead uses an explicit `InlineCode::Unlinked` request
    /// with the namespace snapshot needed to fulfil its Imports.
    pub fn retained_inline_request(
        &self,
        payload: serde_json::Value,
    ) -> Result<lash_execution_contracts::process::InlineRequest, ResolveTargetError> {
        let packages = self
            .routes
            .as_ref()
            .map(crate::SurfaceTools::packages)
            .ok_or_else(|| {
                ResolveTargetError::Refused("inline start has no recorded package surface".into())
            })?;
        let context = packages.consumer.clone().ok_or_else(|| {
            ResolveTargetError::Refused("inline start has no authenticated consumer context".into())
        })?;
        Ok(lash_execution_contracts::process::InlineRequest {
            code: lash_execution_contracts::process::InlineCode::Engine { payload },
            snapshot: Default::default(),
            context,
            attachment: lash_kernel_value::package::Attachment {
                members: Default::default(),
                packages: Default::default(),
                deferred: Default::default(),
            },
        })
    }

    /// The immutable recorded execution that admitted this attempt.
    pub fn execution_environment(
        &self,
    ) -> Option<&lash_core_store::tool_surface::ExecutionEnvironment> {
        self.linked_execution
            .as_ref()
            .map(|execution| execution.environment.as_ref())
            .or_else(|| {
                self.enclosing_env
                    .as_ref()
                    .map(|parent| parent.env.execution())
            })
    }

    /// One injected clock snapshot before the provider callback. A committed timer
    /// retains its absolute deadline; replay never recomputes this snapshot.
    pub fn admission_now(&self) -> lash_durable::DurableInstant {
        self.admission_now
    }

    /// Where this attempt's body stands in the trace: under its tool span.
    /// Spans and points opened here are replay-safe: a body the journal
    /// serves never runs, and one that runs holds the attempt's right.
    pub fn trace(&self) -> &crate::trace::ExtensionTrace {
        &self.trace
    }

    /// The annotations of this attempt's own tool span (§7.4 rule 2).
    pub fn tool_span(&self) -> crate::trace::ToolSpanAttributes {
        self.trace.tool_span()
    }

    /// The W3C context (trace id, span id, flags, tracestate) of this
    /// attempt's own `execute_tool` span, equal to the context the exporter
    /// gives that span: the parent for OpenTelemetry work the body starts
    /// itself, with no record sink. `None` when the call is untraced.
    pub fn tool_span_context(&self) -> Option<lash_trace::TraceCarrier> {
        self.trace.tool_span_context()
    }

    pub(crate) fn with_trace(mut self, trace: crate::trace::ExtensionTrace) -> Self {
        self.trace = trace;
        self
    }

    /// The spec the running turn's recorded shape resolved, read from the
    /// run's recorded preparation and never rebuilt from live config
    /// (FIG-6021). A tool that switches the agent frame passes it back to
    /// run the follow-on under the same shape, or states another spec.
    /// `None` outside a turn.
    pub fn run_spec(&self) -> Option<&crate::RunSpec> {
        self.resolved_run.as_deref().map(crate::ResolvedRun::spec)
    }

    /// The running turn's recorded shape, read-only (FIG-6021): the snapshot
    /// it resolved against, its spec, the config it executes under and its
    /// recorded render, as its recorded preparation holds them and never
    /// rebuilt from live config. `None` outside a turn.
    pub fn resolved_run(&self) -> Option<&crate::ResolvedRun> {
        self.resolved_run.as_deref()
    }

    /// The capability refs the attempt runs with, by slot (D18): the running
    /// turn's recorded run's, or inside a process its environment's. Empty
    /// outside both.
    pub fn run_capabilities(
        &self,
    ) -> &std::collections::BTreeMap<crate::SlotId, crate::CapabilityRef> {
        static NONE: std::collections::BTreeMap<crate::SlotId, crate::CapabilityRef> =
            std::collections::BTreeMap::new();
        if let Some(run) = self.resolved_run.as_deref() {
            return run.capabilities();
        }
        self.enclosing_env
            .as_deref()
            .map_or(&NONE, |enclosing| enclosing.env.capabilities())
    }

    pub fn fleet_format(&self) -> crate::FleetFormat {
        self.fleet_format
    }

    pub fn definition_engines(&self) -> &crate::ProcessEngineRegistry {
        &self.definition_engines
    }

    /// The logical run this attempt runs under: the starting turn in a
    /// process's recorded provenance, or a session turn's admitted scope
    /// (FIG-3607 item 6). Neither requires a live read. `None` for a
    /// host-started process or an operation outside a turn. The full turn
    /// address of a process's starting run is on
    /// [`process_spawn_provenance`](Self::process_spawn_provenance).
    pub fn logical_run(&self) -> Option<lash_vocabulary::identity::TurnId> {
        match self.process_spawn_provenance.as_ref() {
            Some(provenance) => provenance.run.as_ref().map(|run| run.turn_id.clone()),
            None => self.parent_scope.scope().logical_run(),
        }
    }

    /// The start context a child start declared by this attempt draws its
    /// lifetime from (FIG-3607 R2): materialized from the admitted scope the
    /// enclosing execution runs under and the lineage of the process it runs
    /// inside, with no live reads. A plugin resolves its [`LifetimePolicy`]
    /// against it and declares the decision; realization records this same
    /// context's ancestry beside it.
    ///
    /// [`LifetimePolicy`]: lash_execution_contracts::process::LifetimePolicy
    pub fn start_cx(&self) -> Result<lash_execution_contracts::process::StartCx, PluginError> {
        lash_execution_contracts::process::StartCx::materialize(
            &self.parent_scope,
            self.process_lineage.as_ref(),
        )
        .map_err(|error| PluginError::Session(error.to_string()))
    }

    pub(crate) fn from_tool_context(
        context: &ToolContext<'run>,
        execution_scope_id: String,
        completion: AttemptCompletionSupport,
    ) -> Self {
        let phase_probe = context
            .runtime_execution_context
            .as_ref()
            .and_then(crate::RuntimeExecutionContext::attempt_phase_probe);
        let start_identity =
            lash_execution_contracts::tool_intent::derive_tool_intent_identity_under(
                &context.owner.runtime_owner(),
                &execution_scope_id,
                &context.call_id,
                0,
                context.parent_invocation.as_ref(),
            );
        Self {
            history: context
                .runtime_execution_context
                .as_ref()
                .map(crate::RuntimeExecutionContext::read_history),
            admission_now: context
                .admission_now
                .unwrap_or_else(|| context.effect_controller.now()),
            live: live::bind(context),
            fleet_format: context
                .runtime_execution_context
                .as_ref()
                .map(crate::RuntimeExecutionContext::fleet_format)
                .unwrap_or_else(crate::FleetFormat::current),
            definition_engines: context
                .runtime_dispatch
                .as_ref()
                .map(|dispatch| dispatch.process_engines.clone())
                .unwrap_or_default(),
            parent_scope: context.effect_controller.admitted_scope().clone(),
            owner: context.owner.clone(),
            execution_scope_id,
            sessions: AttemptSessionReads {
                owner: context.owner.runtime_owner(),
                sessions: Arc::clone(&context.sessions),
                facts: context.facts.clone(),
                tool_catalog: context
                    .runtime_dispatch
                    .as_ref()
                    .map(|dispatch| Arc::clone(&dispatch.tool_catalog)),
            },
            processes: AttemptProcesses {
                start_identity,
                can_start: completion.key().is_ok(),
                owner: context.owner.runtime_owner(),
                processes: Arc::clone(&context.processes),
            },
            cancellation_token: context.cancellation_token.clone(),
            enclosing_process: context.enclosing_process.clone(),
            attachment_store: Arc::clone(&context.attachment_store),
            direct_completions: context.direct_completions.clone(),
            parent_invocation: context.parent_invocation.clone().map(Box::new),
            prepared_payload: context.prepared_payload.clone(),
            tool_execution_binding: context.tool_execution_binding.clone(),
            config: context.config(),
            call_id: context.call_id.clone(),
            attempt_number: context.attempt_number,
            max_attempts: context.max_attempts,
            process_spawn_provenance: context
                .runtime_execution_context
                .as_ref()
                .and_then(|runtime| runtime.process_spawn_provenance()),
            process_lineage: context.process_lineage(),
            enclosing_env: context
                .runtime_execution_context
                .as_ref()
                .and_then(crate::RuntimeExecutionContext::enclosing_env),
            execution_scope: context.effect_controller.execution_scope().clone(),
            completion,
            phase_probe,
            tool_execution_route: context.tool_execution_route.clone(),
            tool_catalog: context
                .runtime_dispatch
                .as_ref()
                .map(|dispatch| Arc::clone(&dispatch.tool_catalog)),
            routes: context
                .runtime_dispatch
                .as_ref()
                .map(|dispatch| dispatch.routes.clone()),
            linked_execution: context
                .runtime_execution_context
                .as_ref()
                .and_then(|cx| cx.linked_execution().cloned()),
            resolution_pins: context
                .runtime_execution_context
                .as_ref()
                .and_then(|cx| cx.linked_execution())
                .map(|execution| execution.resolutions.lock_recover().clone())
                .unwrap_or_default(),
            resolved_run: context
                .runtime_execution_context
                .as_ref()
                .and_then(crate::RuntimeExecutionContext::resolved_run)
                .cloned(),
            held_launches: Arc::default(),
            trace: crate::trace::ExtensionTrace::untraced(),
        }
    }

    pub(crate) fn execution_route(&self) -> &ToolExecutionRoute {
        &self.tool_execution_route
    }

    /// Integrator class 3: who this recorded attempt runs for.
    pub fn owner(&self) -> &crate::ExecutionOwner {
        &self.owner
    }
    /// Integrator class 3 identity for the session that owns this recorded
    /// attempt, or [`PluginError::NotASessionRuntime`] inside a process.
    pub fn session_id(&self) -> Result<&SessionId, PluginError> {
        self.owner.require_session("attempt_session_id")
    }
    /// Integrator class 3 durable turn or process scope used for intent identity.
    pub fn execution_scope_id(&self) -> &str {
        &self.execution_scope_id
    }
    /// Integrator class 3 agent-frame identity that authorized this provider
    /// attempt, or [`PluginError::NotASessionRuntime`] inside a process.
    pub fn agent_frame_id(&self) -> Result<&crate::FrameNodeId, PluginError> {
        self.owner.require_agent_frame("attempt_agent_frame_id")
    }
    /// Integrator class 3 controller-free session reads for this attempt.
    pub fn sessions(&self) -> AttemptSessionReads {
        self.sessions.clone()
    }
    /// Process reads and child declarations attributed to this call.
    pub fn processes(&self) -> AttemptProcesses {
        self.processes.clone()
    }
    /// Integrator class 3 read of the tool catalog this attempt was dispatched
    /// against: the tools its caller can call. A body that compiles a program
    /// links it against them (FIG-3116). `None` outside a runtime dispatch.
    pub fn tool_catalog(&self) -> Option<&Arc<crate::ToolCatalog>> {
        self.tool_catalog.as_ref()
    }
    /// Integrator class 3 cooperative cancellation token supplied by the attempt host.
    pub fn cancellation_token(&self) -> Option<&tokio_util::sync::CancellationToken> {
        self.cancellation_token.as_ref()
    }
    /// Integrator class 3 process this attempt executes inside, if any.
    pub fn enclosing_process(&self) -> Option<&ProcessId> {
        self.enclosing_process.as_ref()
    }
    /// Integrator class 3 attachment capability for durable tool output.
    pub fn attachments(&self) -> ToolAttachmentClient {
        ToolAttachmentClient {
            store: Arc::clone(&self.attachment_store),
        }
    }
    /// Integrator class 3 direct-completion client attributed to this attempt call.
    ///
    /// The attempt invocation travels with the client so the completion runs on
    /// the journal-free branch: the controller already owns one entry for this
    /// whole attempt, and a second entry emitted from inside the body would be
    /// left unre-issued by redrive.
    pub fn direct_completions(&self) -> ToolDirectCompletionClient<'run> {
        ToolDirectCompletionClient {
            owner: self.owner.runtime_owner(),
            call_id: self.call_id.clone(),
            direct_completions: self.direct_completions.clone(),
            parent_invocation: self.parent_invocation.as_deref().cloned(),
        }
    }
    /// Integrator class 3 payload sealed by the provider's prepare phase.
    pub fn prepared_payload(&self) -> &serde_json::Value {
        &self.prepared_payload
    }
    /// The execution binding of the deferred match this call runs on, its
    /// readability already checked against the provider's declared formats;
    /// `None` for a call that carries none.
    pub fn tool_execution_binding(&self) -> Option<&lash_vocabulary::providers::ExecutionBinding> {
        self.tool_execution_binding.as_ref()
    }

    /// The run's resolved config, which a provider executes the call under.
    pub fn config(&self) -> &crate::plugin::OwnerConfigView {
        &self.config
    }
    /// Lash's identity for this logical call (ADR 0117): the key a tool
    /// keys its idempotency on.
    ///
    /// Repetition follows the recorded execution policy (ADR 0132 §7 and
    /// the [facade tool policy](https://github.com/Ascending-AI/lash/blob/main/crates/lash/src/lib.rs#L386)).
    /// `Once` never restarts a started attempt whose outcome is missing and
    /// never retries a reported failure; missing-outcome ambiguity settles as
    /// `Interrupted`. `Repeatable` may rerun or retry only within its bounds
    /// while both the recorded and current policies permit repetition.
    /// Every permitted run of one logical call sees this same id; every other
    /// call sees a different one, even when the model's provider repeats its own
    /// call id. A service the tool calls deduplicates on it. A tool that
    /// wants a fresh key per attempt combines it with
    /// [`attempt_number`](Self::attempt_number) itself.
    pub fn call_id(&self) -> &lash_protocol_types::ToolCallId {
        &self.call_id
    }
    /// Integrator class 3 one-based retry attempt number.
    pub fn attempt_number(&self) -> u32 {
        self.attempt_number
    }
    /// Integrator class 3 retry ceiling sealed for this invocation.
    pub fn max_attempts(&self) -> u32 {
        self.max_attempts
    }
    /// The enclosing process's full recorded provenance: its originator,
    /// caused-by invocation and starting turn. A resumed attempt reads the
    /// same record. A child start uses this record's originator and inherits
    /// its run through the admitted lineage; Lash stamps the child's own
    /// caused-by from its declaring invocation. No execution scope or host
    /// input replaces the recorded starting turn.
    ///
    /// `None` means this attempt runs outside a process, so a child takes
    /// the declaring session's originator.
    pub fn process_spawn_provenance(
        &self,
    ) -> Option<&lash_execution_contracts::process::ProcessProvenance> {
        self.process_spawn_provenance.as_ref()
    }
    /// The definition `target` names, read and verified before a start
    /// policy runs (D8): its engine's authoritative signature, a supplied
    /// signature claim verified against it, and the payload a start with
    /// `args` runs.
    ///
    /// # Errors
    ///
    /// [`ResolveTargetError::Refused`] for a definition that is not stored
    /// or a claim it refutes, a tool error; [`ResolveTargetError::Fault`]
    /// for a store fault.
    pub async fn resolve_start_target(
        &self,
        target: &lash_execution_contracts::process::ProcessDefinitionTarget,
        args: serde_json::Map<String, serde_json::Value>,
    ) -> Result<crate::ResolvedStartTarget, ResolveTargetError> {
        let engines = &self.definition_engines;
        let ports = engines.artifact_ports().ok_or_else(|| {
            ResolveTargetError::Fault(PluginError::Session(
                "a definition start requires the runtime's artifact stores".into(),
            ))
        })?;
        let definition_id = target.definition_id();
        let resolved = ports
            .read_definition(engines, definition_id)
            .await
            .map_err(ResolveTargetError::Fault)?
            .ok_or_else(|| {
                ResolveTargetError::Refused(format!(
                    "process definition `{definition_id}` is not stored"
                ))
            })?;
        if let lash_execution_contracts::process::ProcessDefinitionTarget::Definition(claimed) =
            target
        {
            engines
                .verify_definition_claim(&resolved.draft, claimed)
                .await
                .map_err(|error| ResolveTargetError::Refused(error.to_string()))?;
        }
        let payload = resolved
            .start_payload(&args)
            .map_err(|error| ResolveTargetError::Refused(error.to_string()))?;
        let engine_kind = resolved.draft.engine_kind().as_str().to_owned();
        let linked = engines
            .inspect_code(&engine_kind, &payload)
            .await
            .map_err(|error| ResolveTargetError::Fault(error.into()))?;
        Ok(crate::ResolvedStartTarget {
            definition_id: resolved.id().clone(),
            engine_kind,
            signature: resolved.definition.signature.clone(),
            payload,
            args,
            linked,
        })
    }

    /// The function `payload` carries inline for the `engine_kind` engine,
    /// checked by that engine before a start policy runs (D8, FIG-6382): the
    /// definition a start of it with `args` runs, its signature and the
    /// effects it performs. Nothing is written to a store: the function is
    /// published only when its declared start is realized, after this
    /// attempt's outcome commits. It is linked under the starter's package
    /// set ([`Self::starting_packages`]), which the start records.
    ///
    /// # Errors
    ///
    /// [`ResolveTargetError::Refused`] for an engine that starts no inline
    /// function or a payload it refuses, a tool error;
    /// [`ResolveTargetError::Fault`] for a retryable fault.
    pub async fn resolve_inline_start(
        &self,
        engine_kind: &str,
        request: lash_execution_contracts::process::InlineRequest,
        args: serde_json::Map<String, serde_json::Value>,
    ) -> Result<crate::ResolvedInlineStart, ResolveTargetError> {
        let refused_or_fault = |error: PluginError| match error.class() {
            lash_execution_contracts::plugin_error::PluginErrorClass::Terminal => {
                ResolveTargetError::Refused(error.to_string())
            }
            _ => ResolveTargetError::Fault(error),
        };
        let inline = self
            .definition_engines
            .inline_starts(engine_kind)
            .map_err(refused_or_fault)?
            .inspect(&request)
            .await
            .map_err(|error| refused_or_fault(error.into()))?;
        let resolved = lash_execution_contracts::process::ResolvedProcessDefinition {
            definition: lash_execution_contracts::process::ProcessDefinition::new(
                inline.draft.id(),
                inline.signature,
            ),
            draft: inline.draft,
        };
        let start_payload = resolved
            .start_payload(&args)
            .map_err(|error| ResolveTargetError::Refused(error.to_string()))?;
        Ok(crate::ResolvedInlineStart {
            target: crate::ResolvedStartTarget {
                definition_id: resolved.id().clone(),
                engine_kind: engine_kind.to_owned(),
                signature: resolved.definition.signature,
                payload: start_payload,
                args,
                linked: inline.linked,
            },
            request,
        })
    }

    /// What a start policy decides `target`'s start with (§3.6): the
    /// starting turn's recorded run, the catalog this attempt was offered,
    /// the grants its cell's committed deferred matches recorded and,
    /// inside a process, the enclosing process's environment.
    pub fn start_policy_cx<'a>(
        &'a self,
        target: &'a crate::ResolvedStartTarget,
        label: Option<&'a str>,
    ) -> crate::ProcessEnvCx<'a> {
        crate::ProcessEnvCx::new(
            &self.owner,
            target,
            label,
            self.resolved_run.as_deref(),
            self.routes.as_ref(),
            self.enclosing_env.as_deref(),
            &self.definition_engines,
        )
        .with_execution(
            self.linked_execution
                .as_ref()
                .map(|execution| execution.environment.as_ref()),
            &self.resolution_pins,
        )
    }

    /// Hold the environment `choice` states under this attempt's execution
    /// claim, checking its links under `choice.fits`, its serving components,
    /// and its settings against `target`'s engine before sealing the launch
    /// a declaration carries (D9, D10). `Restrict` derives the enclosing env.
    ///
    /// # Errors
    ///
    /// [`HoldEnvError::Refused`](crate::HoldEnvError::Refused) for a typed
    /// refusal, a tool error; [`HoldEnvError::Fault`](crate::HoldEnvError::Fault)
    /// for a retryable fault.
    pub async fn declare_launch(
        &self,
        choice: &crate::ProcessStartChoice,
        target: &crate::ResolvedStartTarget,
    ) -> Result<lash_execution_contracts::process::DeclaredDefinitionLaunch, crate::HoldEnvError>
    {
        let launch = self.declare_target_launch(choice, target).await?;
        Ok(
            lash_execution_contracts::process::model::launch::held_definition_launch(
                target.definition_id.clone(),
                launch,
            ),
        )
    }

    /// [`declare_launch`](Self::declare_launch) for a start of the function
    /// `start` carries inline: the launch binds that function, so a
    /// declaration made with it starts exactly it.
    ///
    /// # Errors
    ///
    /// As [`declare_launch`](Self::declare_launch).
    pub async fn declare_inline_launch(
        &self,
        choice: &crate::ProcessStartChoice,
        start: &crate::ResolvedInlineStart,
    ) -> Result<lash_execution_contracts::process::DeclaredInlineLaunch, crate::HoldEnvError> {
        let launch = self.declare_target_launch(choice, &start.target).await?;
        Ok(
            lash_execution_contracts::process::model::launch::held_inline_launch(
                start.target.engine_kind.clone(),
                start.request.clone(),
                launch,
            ),
        )
    }

    pub(super) async fn declare_target_launch(
        &self,
        choice: &crate::ProcessStartChoice,
        target: &crate::ResolvedStartTarget,
    ) -> Result<lash_execution_contracts::process::DeclaredLaunch, crate::HoldEnvError> {
        let launch = crate::runtime::process::hold_launch_performing(
            &self.definition_engines,
            &self.execution_scope,
            choice,
            &target.engine_kind,
            &target.payload,
            &target.linked,
        )
        .await?;
        self.held_launches.lock_recover().push(launch.env.clone());
        Ok(lash_execution_contracts::process::model::launch::held_launch(launch))
    }

    /// [`declare_launch`](Self::declare_launch) for a start of the `kind`
    /// engine with `payload`.
    pub(crate) async fn declare_engine_launch(
        &self,
        choice: &crate::ProcessStartChoice,
        kind: &str,
        payload: &serde_json::Value,
    ) -> Result<lash_execution_contracts::process::DeclaredLaunch, crate::HoldEnvError> {
        let launch = crate::runtime::process::hold_launch(
            &self.definition_engines,
            &self.execution_scope,
            choice,
            kind,
            payload,
        )
        .await?;
        self.held_launches.lock_recover().push(launch.env.clone());
        Ok(lash_execution_contracts::process::model::launch::held_launch(launch))
    }

    /// Whether this attempt held `env` through
    /// [`declare_launch`](Self::declare_launch).
    pub(crate) fn held_launch(&self, env: &crate::ProcessEnvRef) -> bool {
        self.held_launches.lock_recover().contains(env)
    }

    /// Integrator class 3 decode of the sealed payload into a provider-owned type.
    pub fn decode_prepared_payload<T>(&self) -> Result<T, serde_json::Error>
    where
        T: serde::de::DeserializeOwned,
    {
        serde_json::from_value(self.prepared_payload.clone())
    }
    /// Integrator class 3 named, attempt-attributed runtime phase for fault probes.
    pub fn named_phase(&self, phase: &'static str) -> crate::runtime::RuntimeNamedPhase {
        crate::runtime::RuntimeNamedPhase::begin(self.phase_probe.clone(), phase)
    }
    /// The host key of this call's completion wait: the tool completion wait
    /// its round pinned when it admitted the call, which a host resolves
    /// through [`resolve_host`](crate::runtime::actor::waits::resolve_host).
    /// A rerun of the call is handed the same key.
    ///
    /// # Errors
    ///
    /// A typed refusal when the capability is missing: the tool did not
    /// declare that its attempt may defer, or the call runs where no
    /// completion wait is pinned for it.
    pub fn completion_key(
        &self,
    ) -> Result<lash_execution_contracts::waits::PinnedKey, crate::RuntimeError> {
        self.completion.key()
    }
    /// The identity of the intent this attempt declares at `intent_index`,
    /// derived from its [`call_id`](Self::call_id).
    pub fn intent_identity(&self, intent_index: u32) -> crate::ToolIntentIdentity {
        lash_execution_contracts::tool_intent::derive_tool_intent_identity_under(
            &self.owner.runtime_owner(),
            &self.execution_scope_id,
            &self.call_id,
            intent_index,
            self.parent_invocation.as_deref(),
        )
    }
}

#[cfg(test)]
mod work_laws {
    use {
        lash_kernel_value::names::ExportName, lash_kernel_value::names::Name,
        lash_kernel_value::names::PackagePath, lash_kernel_value::names::Segment,
    };
    use {lash_kernel_value::package::*, lash_vocabulary::package::*};

    /// FIG-7135/C10: a linked callable already carries retained provenance
    /// by identity. Its declaration never copies unrelated package members.
    #[test]
    fn linked_start_declaration_bytes_do_not_grow_with_advertised_members() {
        let context = PublicationContext {
            domain: PublicationDomain("store".into()),
            scope: PublicationScope::Host,
            publisher: PublisherInstance("library".into()),
            root: Segment::new("library").unwrap(),
        };
        let payload = serde_json::json!({
            "kind": "callable",
            "value": {"link": "retained-link", "function": "captured-callable", "origin": {"authored": ["retained-link"]}}
        });
        let declaration = |members: usize| {
            let package = Package {
                path: PackagePath::parse("library").unwrap(),
                members: (0..members)
                    .map(|n| {
                        (
                            Segment::new(format!("member{n}")).unwrap(),
                            Member {
                                visibility: Visibility::Public,
                                kind: MemberKind::Export {
                                    link: LinkId::from_bytes([7; 32]),
                                    export: ExportName {
                                        name: Name::new("run"),
                                        member: None,
                                    },
                                },
                            },
                        )
                    })
                    .collect(),
            };
            let record = PublicationRecord {
                context: context.clone(),
                path: package.path.clone(),
                content: PublishedContent::Package(package.identity()),
                code_links: Default::default(),
                imported_publications: Vec::new(),
            };
            let mut attempt = crate::testing::ToolCallFixture::mock().attempt("linked-start-size");
            attempt.routes = Some(crate::SurfaceTools::default().with_packages(
                lash_core_store::tool_surface::RecordedPackages {
                    consumer: Some(context.clone()),
                    snapshot: NamespaceSnapshot {
                        advertisements: vec![AcceptedAdvertisement {
                            publication: record,
                            advertisement: PackageAdvertisement::Enumerated(package),
                        }],
                    },
                    routes: Vec::new(),
                },
            ));
            let request = attempt.retained_inline_request(payload.clone()).unwrap();
            assert_eq!(request.context, context);
            assert_eq!(
                request.code,
                lash_execution_contracts::process::InlineCode::Engine {
                    payload: payload.clone()
                }
            );
            let launch = lash_execution_contracts::process::model::launch::held_inline_launch(
                lash_protocol_types::LASH_VM_ENGINE_KIND.into(),
                request,
                lash_execution_contracts::process::model::launch::held_launch(
                    lash_execution_contracts::process::EngineLaunch::new(
                        crate::ProcessEnvRef::new("linked-env"),
                        None,
                    ),
                ),
            );
            let start = lash_execution_contracts::process::ProcessStartDeclaration::inline(
                Default::default(),
                launch,
                lash_execution_contracts::process::ProcessOriginator::Session {
                    session_id: "test-session".into(),
                    agent_frame_id: None,
                },
                lash_execution_contracts::process::Lifetime::Detached,
            );
            let intents = lash_execution_contracts::tool_intent::ToolIntents::v3(vec![
                lash_execution_contracts::tool_intent::ToolIntent::StartProcess(Box::new(
                    lash_execution_contracts::tool_intent::StartProcessIntent {
                        owner: attempt.owner().runtime_owner(),
                        declaration: start,
                    },
                )),
            ]);
            let value = serde_json::to_value(&intents).unwrap();
            eprintln!(
                "members={members} declaration_bytes={}",
                serde_json::to_vec(&value).unwrap().len()
            );
            intents.declared_canonical_bytes().unwrap()
        };
        let small = declaration(1);
        let large = declaration(2_048);
        assert_eq!(
            small, large,
            "linked start declarations carry code and provenance identities, not namespace contents"
        );
        assert!(large <= lash_execution_contracts::tool_intent::TOOL_INTENT_MAX_CANONICAL_BYTES);
    }
}
