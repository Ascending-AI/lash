use super::*;
use crate::facade_support::SessionGraphFacadeOps;

pub(in crate::runtime::session_manager) struct SessionCreatePlan {
    pub(in crate::runtime::session_manager) session_id: String,
    pub(in crate::runtime::session_manager) relation: SessionRelation,
    pub(in crate::runtime::session_manager) parent_session_id: Option<String>,
    pub(in crate::runtime::session_manager) policy: SessionPolicy,
    pub(in crate::runtime::session_manager) initial_runtime_state: RuntimeSessionState,
    pub(in crate::runtime::session_manager) plugin_config: crate::plugin::SessionCreationConfig,
    pub(in crate::runtime::session_manager) plugin_source: crate::SessionPluginSource,
    pub(in crate::runtime::session_manager) context_overlay: crate::SessionContextOverlay,
    pub(in crate::runtime::session_manager) protocol_request: SessionCreateRequest,
    pub(in crate::runtime::session_manager) usage_source: Option<String>,
}

pub(in crate::runtime::session_manager) async fn resolve_session_create_plan(
    managed: &ManagedSessionCapability,
    current: &CurrentSessionCapability,
    mut request: SessionCreateRequest,
) -> Result<SessionCreatePlan, crate::PluginError> {
    let session_id = request
        .session_id
        .take()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    request.session_id = Some(session_id.clone());
    if session_id == current.session_id || managed.registry.lock().await.contains_key(&session_id) {
        return Err(crate::PluginError::Session(format!(
            "session `{session_id}` already exists"
        )));
    }

    let parent_session_id = request.relation.parent_session_id().map(ToOwned::to_owned);
    let start_state = resolve_start_state(managed, current, &request, &session_id).await?;
    let policy = resolve_session_policy(current, &request, &start_state, &session_id);
    request.policy = Some(policy.clone());
    let initial_runtime_state = build_runtime_state(
        session_id.clone(),
        &request,
        start_state,
        &policy,
        current.host.core.clock.as_ref(),
    )
    .map_err(|error| crate::PluginError::Session(error.to_string()))?;
    let plugin_config = crate::plugin::SessionCreationConfig {
        authority: crate::plugin::SessionAuthorityContext {
            tool_access: request.tool_access.clone(),
            subagent: request.subagent.clone(),
            plugin_options: request.plugin_options.clone(),
        },
        protocol_turn_options: initial_runtime_state.protocol_turn_options.clone(),
    };

    let relation = request
        .relation
        .clone()
        .with_observer_intent(request.observed_processes.clone());
    Ok(SessionCreatePlan {
        session_id,
        relation,
        parent_session_id,
        policy,
        initial_runtime_state,
        plugin_config,
        plugin_source: request.plugin_source,
        context_overlay: request.context_overlay.clone(),
        usage_source: request.usage_source.clone(),
        protocol_request: request,
    })
}

async fn resolve_start_state(
    managed: &ManagedSessionCapability,
    current: &CurrentSessionCapability,
    request: &SessionCreateRequest,
    session_id: &str,
) -> Result<RuntimeSessionState, crate::PluginError> {
    match &request.start {
        SessionStartPoint::Empty => Ok(RuntimeSessionState {
            session_id: session_id.to_string(),
            ..RuntimeSessionState::new(current.policy.clone())
        }),
        SessionStartPoint::CurrentSession => Ok(current.snapshot.to_runtime_state()),
        SessionStartPoint::ExistingSession { session_id } => current
            .resident_state_by_id(managed, session_id)
            .await
            .ok_or_else(|| crate::PluginError::Session(format!("unknown session `{session_id}`"))),
        SessionStartPoint::Snapshot { snapshot } => {
            let mut state = current
                .resident_state_by_id(managed, &snapshot.session_id)
                .await
                .unwrap_or_else(|| RuntimeSessionState::from_snapshot((**snapshot).clone()));
            state.apply_snapshot(snapshot);
            Ok(state)
        }
    }
}

fn resolve_session_policy(
    current: &CurrentSessionCapability,
    request: &SessionCreateRequest,
    start_state: &RuntimeSessionState,
    session_id: &str,
) -> SessionPolicy {
    let mut policy = request
        .policy
        .clone()
        .unwrap_or_else(|| match &request.start {
            SessionStartPoint::Empty => current.policy.clone(),
            _ => start_state.policy.clone(),
        });
    if request.relation.parent_session_id().is_some() {
        policy.session_id = Some(session_id.to_string());
    }
    policy
}

fn build_runtime_state(
    session_id: String,
    request: &SessionCreateRequest,
    mut base: RuntimeSessionState,
    policy: &SessionPolicy,
    clock: &dyn crate::Clock,
) -> Result<RuntimeSessionState, crate::StoreError> {
    let inherited_nodes = match &request.start {
        SessionStartPoint::Empty => Vec::new(),
        _ => current_frame_node_drafts(&base),
    };
    base.session_id = session_id;
    base.head_revision = 0;
    base.checkpoint_components.complete_for_new_session()?;
    base.policy = policy.clone();
    base.session_graph = crate::SessionGraph::default();
    base.agent_frames.clear();
    base.current_frame_node_id = None;
    base.persisted_node_ids.clear();
    base.reset_initial_agent_frame_with_clock(
        crate::AgentFrameAssignment::from_session_request(request, policy.clone()),
        base.protocol_turn_options.clone(),
        clock,
    );
    let inherited_namespace = format!("create-session:{}:inherited", base.session_id);
    base.session_graph.append_node_drafts_at(
        &inherited_namespace,
        inherited_nodes,
        clock.timestamp_rfc3339(),
    );
    let draft_namespace = format!("create-session:{}", base.session_id);
    append_session_nodes_to_state_with_clock(
        &mut base,
        &request.initial_nodes,
        &draft_namespace,
        clock,
    );
    Ok(base)
}

fn current_frame_node_drafts(
    state: &RuntimeSessionState,
) -> Vec<crate::session_graph::SessionNodeDraft> {
    let active_path = state.session_graph.active_path_nodes();
    let start = state
        .current_frame_node_id
        .as_deref()
        .and_then(|frame_node_id| {
            active_path
                .iter()
                .position(|node| node.node_id == frame_node_id)
        })
        .map_or(0, |index| index + 1);
    active_path[start..]
        .iter()
        .filter_map(|node| match &node.payload {
            crate::SessionNodePayload::Event { event } => {
                Some(crate::session_graph::SessionNodeDraft::event(event.clone()))
            }
            crate::SessionNodePayload::Plugin { plugin_type, body } => {
                Some(crate::session_graph::SessionNodeDraft::plugin(
                    plugin_type.clone(),
                    body.to_owned(),
                ))
            }
            crate::SessionNodePayload::FrameOpen { .. } => None,
        })
        .collect()
}
