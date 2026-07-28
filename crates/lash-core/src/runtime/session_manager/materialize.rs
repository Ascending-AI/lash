use super::create_plan::SessionCreatePlan;
use super::*;
use crate::runtime::host::EmbeddedRuntimeHost;

pub(in crate::runtime::session_manager) struct MaterializedSession {
    pub(in crate::runtime::session_manager) runtime: LashRuntime,
    pub(in crate::runtime::session_manager) store_binding:
        Option<Arc<dyn crate::store::RuntimePersistence>>,
}

pub(in crate::runtime::session_manager) async fn materialize_session_create_plan(
    current: &CurrentSessionCapability,
    plan: &SessionCreatePlan,
) -> Result<MaterializedSession, crate::PluginError> {
    let plugins = build_session_plugins(current, plan)?;
    let store_binding = bind_session_store(current, plan).await?;
    // Child-session creation routes through the same assembler as the live open
    // and worker-rebuild paths. A freshly created session has a single path, so
    // it materializes under KeepAll (residency trimming is an open-time concern).
    let mut runtime = LashRuntime::assemble_runtime(
        plan.policy.clone(),
        embedded_host(current),
        plugins,
        crate::runtime::lifecycle::RuntimePersistenceBindings::new(store_binding.clone()),
        current.host.process_registry.clone(),
        plan.initial_runtime_state.clone(),
        crate::Residency::default(),
    )
    .await
    .map_err(|err| crate::PluginError::Session(err.to_string()))?;

    // Managed children execute on the current host, so preserve its work
    // drivers just as live runtime assembly does. The registry alone can
    // record nested work, but without its driver a child turn can await a row
    // that no local worker is ever asked to run.
    runtime.host.process_work_driver = current.host.process_work_driver.clone();
    runtime.host.queued_work_driver = current.host.queued_work_driver.clone();

    runtime.configure_protocol_on_materialize(
        &plan.protocol_request.plugin_options,
        plan.protocol_request.relation.parent_session_id().is_none(),
    )?;
    if let Some(session) = runtime.session.as_mut() {
        session.set_context_overlay(
            plan.context_overlay.tool_providers.clone(),
            plan.context_overlay.prompt_contributions.clone(),
            plan.context_overlay.include_base_tools,
        )?;
    }

    Ok(MaterializedSession {
        runtime,
        store_binding,
    })
}

fn build_session_plugins(
    current: &CurrentSessionCapability,
    plan: &SessionCreatePlan,
) -> Result<Arc<crate::PluginSession>, crate::PluginError> {
    match plan.plugin_source {
        crate::SessionPluginSource::CurrentHostFresh => current
            .plugins
            .host()
            .build_session_with_parent(
                &plan.session_id,
                plan.parent_session_id.clone(),
                None,
                plan.plugin_authority.clone(),
            )
            .map_err(|err| crate::PluginError::Session(err.to_string())),
        crate::SessionPluginSource::CurrentSessionFork => current
            .plugins
            .fork_for_child_session(
                &plan.session_id,
                plan.parent_session_id.clone(),
                plan.plugin_authority.clone(),
            )
            .map_err(|err| crate::PluginError::Session(err.to_string())),
    }
}

async fn bind_session_store(
    current: &CurrentSessionCapability,
    plan: &SessionCreatePlan,
) -> Result<Option<Arc<dyn crate::store::RuntimePersistence>>, crate::PluginError> {
    let Some(factory) = &current.host.session_store_factory else {
        return Ok(None);
    };
    let store = factory
        .create_store(&SessionStoreCreateRequest {
            session_id: plan.session_id.clone(),
            relation: plan.relation.clone(),
            policy: plan.policy.clone(),
        })
        .await
        .map_err(|message| {
            crate::PluginError::Session(child_store_factory_error(
                &plan.session_id,
                plan.parent_session_id.as_deref(),
                message,
            ))
        })?;
    validate_child_store_binding(
        store.as_ref(),
        &plan.session_id,
        plan.parent_session_id.as_deref(),
    )
    .await?;
    Ok(Some(store))
}

fn embedded_host(current: &CurrentSessionCapability) -> EmbeddedRuntimeHost {
    EmbeddedRuntimeHost {
        core: current.host.core.clone(),
        session_store_factory: current.host.session_store_factory.clone(),
        trigger_store: current.host.trigger_store.clone(),
    }
}

fn child_store_guidance(parent_session_id: Option<&str>) -> String {
    let parent = parent_session_id
        .map(|id| format!(" for parent session `{id}`"))
        .unwrap_or_default();
    format!(
        "Managed child sessions require a store for the child session id{parent}. \
         Do not wrap a single pre-opened root store in LashCoreBuilder::store_factory; \
         pass root-only stores with SessionBuilder::store(...) and configure \
         LashCoreBuilder::child_store_factory(...) for managed children."
    )
}

fn child_store_factory_error(
    session_id: &str,
    parent_session_id: Option<&str>,
    message: String,
) -> String {
    format!(
        "failed to create store for child session `{session_id}`: {message}. {}",
        child_store_guidance(parent_session_id)
    )
}

async fn validate_child_store_binding(
    store: &dyn crate::RuntimePersistence,
    session_id: &str,
    parent_session_id: Option<&str>,
) -> Result<(), crate::PluginError> {
    let meta = store.load_session_meta().await.map_err(|err| {
        crate::PluginError::Session(format!(
            "failed to inspect store for child session `{session_id}`: {err}. {}",
            child_store_guidance(parent_session_id)
        ))
    })?;
    if let Some(meta) = meta
        && meta.session_id != session_id
    {
        return Err(crate::PluginError::Session(format!(
            "configured child session store is already bound to session `{}` and cannot be used for child session `{session_id}`. {}",
            meta.session_id,
            child_store_guidance(parent_session_id)
        )));
    }
    Ok(())
}
