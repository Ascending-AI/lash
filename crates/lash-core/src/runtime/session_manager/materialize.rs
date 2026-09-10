use super::create_plan::SessionCreatePlan;
use super::*;
use crate::runtime::host::EmbeddedRuntimeHost;

pub(in crate::runtime::session_manager) struct MaterializedSession {
    pub(in crate::runtime::session_manager) runtime: LashRuntime,
    pub(in crate::runtime::session_manager) store_binding:
        Arc<dyn crate::store::RuntimePersistence>,
}

pub(in crate::runtime::session_manager) async fn materialize_session_create_plan(
    current: &CurrentSessionCapability,
    plan: &SessionCreatePlan,
) -> Result<MaterializedSession, crate::PluginError> {
    let plugins = build_session_plugins(current, plan)?;
    let store_binding = bind_session_store(current, plan).await?;
    // Session creation routes through the same assembler as live open and
    // worker-rebuild paths. A freshly created session has a single path, so it
    // materializes under KeepAll (residency trimming is an open-time concern).
    let mut runtime = LashRuntime::assemble_runtime(
        plan.policy.clone(),
        embedded_host(current),
        plugins,
        crate::runtime::lifecycle::RuntimePersistenceBindings::new(Some(store_binding.clone())),
        current.host.work.clone(),
        crate::runtime::lifecycle::RuntimeSessionAssembly::new(
            plan.initial_runtime_state.clone(),
            plan.relation.clone(),
            current.runtime_lease_owner.clone(),
        ),
    )
    .await
    .map_err(|err| crate::PluginError::Session(err.to_string()))?;

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
        crate::SessionPluginSource::CurrentHostFresh => {
            current.plugins.host().build_session_with_parent(
                &plan.session_id,
                plan.parent_session_id.clone(),
                plan.plugin_config.clone(),
            )
        }
        crate::SessionPluginSource::CurrentSessionFork => current.plugins.fork_for_child_session(
            &plan.session_id,
            plan.parent_session_id.clone(),
            plan.plugin_config.clone(),
        ),
    }
}

async fn bind_session_store(
    current: &CurrentSessionCapability,
    plan: &SessionCreatePlan,
) -> Result<Arc<dyn crate::store::RuntimePersistence>, crate::PluginError> {
    let Some(factory) = &current.host.session_store_factory else {
        return Err(crate::PluginError::MissingSessionStore {
            session_id: plan.session_id.clone(),
        });
    };
    let store = factory
        .create_store(&SessionStoreCreateRequest {
            session_id: plan.session_id.clone(),
            relation: plan.relation.clone(),
            pending_observer_intents: plan.pending_observer_intents.clone(),
            policy: plan.policy.clone(),
        })
        .await
        .map_err(|message| {
            crate::PluginError::Session(session_creation_store_factory_error(
                &plan.session_id,
                message.to_string(),
            ))
        })?;
    validate_created_session_store_binding(store.as_ref(), &plan.session_id).await?;
    Ok(store)
}

fn embedded_host(current: &CurrentSessionCapability) -> EmbeddedRuntimeHost {
    EmbeddedRuntimeHost {
        core: current.host.core.clone(),
        session_store_factory: current.host.session_store_factory.clone(),
        trigger_store: current.host.trigger_store.clone(),
    }
}

fn session_creation_store_guidance() -> &'static str {
    "A session-creation factory must return a distinct store bound to the requested session id. \
     Do not wrap a single pre-opened store in LashCoreBuilder::store_factory; pass that exact \
     store with SessionBuilder::store(...) and configure \
     LashCoreBuilder::session_creation_store_factory(...) for sessions created from a running session."
}

fn session_creation_store_factory_error(session_id: &SessionId, message: String) -> String {
    format!(
        "failed to create store for session `{session_id}`: {message}. {}",
        session_creation_store_guidance()
    )
}

async fn validate_created_session_store_binding(
    store: &dyn crate::RuntimePersistence,
    session_id: &SessionId,
) -> Result<(), crate::PluginError> {
    let meta = store.load_session_meta().await.map_err(|err| {
        crate::PluginError::Session(format!(
            "failed to inspect store for session `{session_id}`: {err}. {}",
            session_creation_store_guidance()
        ))
    })?;
    if let Some(meta) = meta
        && &meta.session_id != session_id
    {
        return Err(crate::PluginError::Session(format!(
            "configured session-creation store is already bound to session `{}` and cannot be used for session `{session_id}`. {}",
            meta.session_id,
            session_creation_store_guidance()
        )));
    }
    Ok(())
}
