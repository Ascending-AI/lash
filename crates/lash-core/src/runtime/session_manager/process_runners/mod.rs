use super::*;

#[cfg(test)]
mod context_tests;
mod control;
mod session;

impl RuntimeSessionServices {
    /// The tools an engine process's linked calls run on, and their
    /// catalog: each tool its env links an effect to, as this node's
    /// components serve it under the env's config, when it still projects
    /// to the surface the link retains.
    pub(in crate::runtime) fn process_step_surface(
        &self,
    ) -> Result<
        (crate::SurfaceTools, Arc<crate::ToolCatalog>),
        lash_execution_contracts::plugin_error::PluginError,
    > {
        let (process_id, engine) = self.current.engine_process()?;
        let core = &self.current.host.core;
        let (routes, catalog) = engine.composition.linked_tools(
            &engine.env,
            lash_vocabulary::identity::RuntimeOwner::Process(process_id.clone()),
            &core.process_engines.components().host().extensions,
            core.process_engines.engine_kinds(),
        )?;
        Ok((routes, Arc::new(catalog)))
    }

    /// The tools `process`'s steps run (ADR 0132 §10): its catalog, the
    /// round tools that pin and run a catalog tool as a turn's round does,
    /// under `cx`, the process actor's claimed context. They run inside
    /// `process`: what a tool reads off the process it runs in, and what the
    /// children it starts inherit (its lineage, originator and environment),
    /// are the process's committed facts, read once when the activation
    /// builds its tools.
    ///
    /// # Errors
    ///
    /// The catalog does not resolve.
    pub(in crate::runtime) async fn process_step_tools(
        &self,
        cx: crate::ActorContext,
        process: &lash_execution_contracts::process::ProcessRecord,
    ) -> Result<crate::runtime::ProcessStepTools, lash_execution_contracts::plugin_error::PluginError>
    {
        use lash_core_execution::core_internal::RuntimeExecutionContextRuntimeOps as _;
        let (routes, catalog) = self.process_step_surface()?;
        let (_, engine) = self.current.engine_process()?;
        let env = Arc::clone(&engine.env);
        let environment = Arc::new(env.execution().clone());
        let execution = lash_core_store::resolutions::ExecutionId::process(&process.id);
        let mut pins = std::collections::BTreeMap::new();
        let resolution_store = cx.backend().stores().execution_resolutions();
        for use_ in environment.uses.keys() {
            if let Some(record) = resolution_store
                .load(&lash_core_store::resolutions::ResolutionKey {
                    execution: execution.clone(),
                    use_: *use_,
                })
                .await
                .map_err(lash_execution_contracts::plugin_error::registry_error)?
            {
                pins.insert(*use_, record.entry);
            }
        }
        engine.resolutions.lock_recover().extend(pins.clone());
        let dispatch = self.process_step_dispatch(routes, Arc::clone(&catalog), cx)?;
        let core = &self.current.host.core;
        let context = crate::RuntimeExecutionContext::new(
            dispatch,
            Arc::clone(&core.durability.process_env_store),
            Arc::clone(&core.durability.attachment_store),
            Arc::new(crate::ChronologicalProjection::default()),
            crate::TurnContext::default(),
        )
        .with_tool_live(core.tool_live.clone())
        .with_tool_fault_retry(core.control.pacing.tool_fault_retry)
        .with_turn_phase_probe(self.current.turn_phase_probe.clone())
        .with_fleet_format(core.session_store_factory().fleet_format())
        .with_tool_material_store(core.backend().tool_material_store())
        .with_process_work(self.current.host.work.process_wiring().cloned())
        .with_linked_execution(lash_core_execution::session::LinkedExecution {
            execution,
            environment: Arc::clone(&environment),
            resolutions: Arc::clone(&engine.resolutions),
        })
        .with_process_execution(process, Some(env))?;
        let tools = context
            .round_tools(crate::EffectOpener::Process {
                process_id: process.id.clone(),
            })
            .map_err(|error| {
                lash_execution_contracts::plugin_error::PluginError::Session(error.to_string())
            })?;
        Ok(crate::runtime::ProcessStepTools {
            catalog,
            environment,
            resolutions: Arc::clone(&engine.resolutions),
            reads: engine
                .composition
                .effect_reads(Arc::clone(&engine.env), Arc::clone(&core.clock)),
            tools,
            context: Arc::new(self.clone()),
        })
    }

    /// The dispatch an engine process's tool steps run on (ADR 0132 §10):
    /// its catalog, over the composition only its environment's components
    /// make, under `effect_controller`, the process actor's claimed context.
    /// The execution context that runs inside the process sets the lineage
    /// and originator its children start under (FIG-3607 R1).
    fn process_step_dispatch<'run>(
        &self,
        routes: crate::SurfaceTools,
        catalog: Arc<crate::ToolCatalog>,
        effect_controller: crate::ActorContext,
    ) -> Result<
        Arc<crate::tool_dispatch::ToolDispatchContext<'run>>,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        let services = Arc::new(self.clone());
        let direct_completions = services.direct_completion_client(effect_controller.clone(), None);
        let (_, engine) = self.current.engine_process()?;
        let owner = self.current.execution_owner()?;
        let core = &self.current.host.core;
        Ok(Arc::new(crate::tool_dispatch::ToolDispatchContext {
            fleet_format: core.session_store_factory().fleet_format(),
            composition: Arc::clone(&engine.composition.composition),
            config: crate::plugin::OwnerConfigView::components(Arc::clone(&engine.env)),
            execution_budgets: core.control.execution_budgets.clone(),
            state: engine.state.clone(),
            tools: Arc::new(routes.clone()),
            routes,
            tool_catalog: catalog,
            sessions: services.state_service(),
            session_lifecycle: services.lifecycle_service(),
            session_graph: services.graph_service(),
            processes: services.model_tool_process_service(),
            process_engines: services.process_engines().clone(),
            effect_controller,
            direct_completions,
            parent_invocation: None,
            observation_call_key: None,
            facts: crate::plugin::ExecutionFacts::of_process(&engine.env),
            owner,
            // A process's steps have no host lane: the dispatch observes
            // nowhere.
            observer: crate::engine::NullObservationSink::arc(),
            attachment_store: Arc::clone(&core.durability.attachment_store),
            turn_context: crate::TurnContext::default(),
            clock: Arc::clone(&core.clock),
            process_lineage: None,
            process_originator: None,
        }))
    }
}

#[async_trait::async_trait]
impl lash_core_execution::runtime::process::steps::ProcessStepContext for RuntimeSessionServices {
    /// How this node serves the engine process's linked call of `effect`,
    /// pinned to `contract` (D3), judged now.
    fn step_link(
        &self,
        effect: &lash_vocabulary::names::OperationUse,
    ) -> Result<crate::plugin::StepLink, lash_execution_contracts::plugin_error::PluginError> {
        let (_, engine) = self.current.engine_process()?;
        Ok(engine.composition.step_link(&engine.env, effect))
    }

    async fn resolve_use(
        &self,
        cx: &crate::ActorContext,
        process: &lash_execution_contracts::process::ProcessRecord,
        use_: lash_vocabulary::names::OperationUse,
    ) -> Result<
        lash_core_execution::runtime::actor::resolutions::CommittedUse,
        lash_core_execution::runtime::actor::resolutions::ResolutionError,
    > {
        use lash_core_execution::runtime::actor::resolutions::{FirstUse, ResolutionError};
        use lash_core_store::tool_surface::UseBinding;
        use lash_execution_contracts::process::linking::{
            AdmissionRefusal, EnginePortError, OperationNeed,
        };
        let (_, engine) = self
            .current
            .engine_process()
            .map_err(|error| ResolutionError::Corrupt(error.to_string()))?;
        let environment = engine.env.execution();
        let Some(UseBinding::Deferred(grant)) = environment.uses.get(&use_) else {
            return Err(EnginePortError::Refused(AdmissionRefusal::MissingRoute { use_ }).into());
        };
        let lash_execution_contracts::process::ProcessInput::Engine { kind, payload, .. } =
            process.input.as_ref()
        else {
            return Err(ResolutionError::Corrupt(
                "a session turn cannot resolve a code use".into(),
            ));
        };
        let inspection = self
            .current
            .host
            .core
            .process_engines
            .inspect_code(kind, payload)
            .await?;
        let mut need = inspection.operations.get(&use_).cloned();
        if need.is_none() {
            let runtime = self
                .current
                .host
                .core
                .process_engines
                .require(kind)
                .map_err(|error| ResolutionError::Corrupt(error.to_string()))?;
            for callable in &environment.retained_code {
                let retained = runtime
                    .inspect_link(
                        callable.link,
                        &lash_execution_contracts::process::linking::ReachSelector::Callable {
                            function: callable.function,
                        },
                    )
                    .await?;
                if let Some(found) = retained.operations.get(&use_) {
                    need = Some(found.clone());
                    break;
                }
            }
        }
        let Some(OperationNeed::Deferred {
            name,
            grant: retained,
        }) = need
        else {
            return Err(EnginePortError::Refused(AdmissionRefusal::MissingRoute { use_ }).into());
        };
        if retained.as_ref() != grant {
            return Err(EnginePortError::Refused(AdmissionRefusal::MissingRoute { use_ }).into());
        }
        let composition = &engine.composition.composition;
        let provider = composition.deferred_package_provider(&grant.provider);
        let handle = lash_vocabulary::providers::ToolProviderHandle::from_name(
            environment.registry,
            &grant.provider,
        )
        .map_err(|error| ResolutionError::Corrupt(error.to_string()))?;
        let owner = self
            .current
            .execution_owner()
            .map_err(|error| ResolutionError::Corrupt(error.to_string()))?;
        let config = crate::plugin::OwnerConfigView::components(Arc::clone(&engine.env));
        let tx = cx.begin().await?;
        let store = cx.backend().stores().execution_resolutions();
        FirstUse {
            held: None,
            store: store.as_ref(),
            owner: lash_durable::resolutions::ResolutionOwner {
                actor: cx.actor().clone(),
                epoch: cx.epoch(),
                expected_revision: tx.revision(),
            },
            execution: lash_core_store::resolutions::ExecutionId::process(&process.id),
            environment,
            capabilities: cx.capabilities(),
            provider: provider.map(|provider| provider.as_ref()),
            context: crate::DeferredResolveContext::new(
                &handle,
                &owner,
                None,
                engine.env.capabilities(),
                &config,
            ),
            validator: composition.as_ref(),
            clock: cx.clock().as_ref(),
            cancel: cx.cancel(),
        }
        .resolve(use_, name.clone())
        .await
        .inspect(|committed| {
            let record = match committed {
                lash_core_execution::runtime::actor::resolutions::CommittedUse::Ready(record)
                | lash_core_execution::runtime::actor::resolutions::CommittedUse::Parked {
                    record,
                    ..
                } => record,
            };
            engine
                .resolutions
                .lock_recover()
                .insert(use_, record.entry.clone());
        })
    }

    /// How this node serves the engine process's call of the tool
    /// `matched` resolved a granted tool's call path to (P6), and whether it
    /// reads the match's execution binding (S8), judged now.
    fn matched_link(
        &self,
        matched: &crate::DeferredMatch,
    ) -> Result<
        (crate::plugin::StepLink, Result<(), String>),
        lash_execution_contracts::plugin_error::PluginError,
    > {
        let (_, engine) = self.current.engine_process()?;
        Ok((
            engine.composition.matched_link(matched),
            engine.composition.check_matched_binding(matched),
        ))
    }

    /// Resolve the call the engine process's granted tool at `effect` names
    /// for `input`, through the deferred provider its grant links (P6).
    async fn resolve_dispatch(
        &self,
        effect: &lash_vocabulary::names::OperationUse,
        input: &serde_json::Value,
    ) -> Result<
        Result<crate::DeferredMatch, lash_core_execution::DispatchRefusal>,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        let (_, engine) = self.current.engine_process()?;
        let owner = self.current.execution_owner()?;
        engine
            .composition
            .resolve_dispatch(&engine.env, &owner, effect, input)
            .await
    }

    /// The arguments of the engine process's call of the tool `matched`
    /// resolved, as the grant at `effect` names them again for `input`.
    fn dispatch_args(
        &self,
        effect: &lash_vocabulary::names::OperationUse,
        input: &serde_json::Value,
        matched: &crate::DeferredMatch,
    ) -> Result<
        Result<serde_json::Value, lash_core_execution::DispatchRefusal>,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        let (_, engine) = self.current.engine_process()?;
        engine
            .composition
            .dispatch_args(&engine.env, effect, input, matched)
    }

    /// The claim requirements an engine process carries while it holds tool
    /// steps (§6.2): its environment's components' state/config format tokens.
    fn requirements(
        &self,
    ) -> Result<
        std::collections::BTreeSet<lash_vocabulary::claims::ObligationToken>,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        let (_, engine) = self.current.engine_process()?;
        let mut requires =
            crate::plugin::ProcessComponentDeclaration::env_requirements(&engine.env);
        for entry in engine.env.components() {
            let namespace = crate::plugin::ExecutionState::owner_state(
                engine.state.as_ref(),
                entry.id().as_str(),
            );
            requires.insert(lash_vocabulary::claims::ObligationToken::for_component(
                entry.id(),
                namespace.format_version,
            ));
        }
        Ok(requires)
    }
}
