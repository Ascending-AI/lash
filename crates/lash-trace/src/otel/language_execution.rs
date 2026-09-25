use super::*;

pub(super) fn language_execution_attributes(
    attrs: &mut Vec<KeyValue>,
    language: &str,
    event: &crate::TraceLanguageExecution,
) {
    use crate::TraceLanguageExecutionPayload as Payload;

    let kind = match &event.payload {
        Payload::ExecutionStarted { .. } => "execution_started",
        Payload::ExecutionFinished { .. } => "execution_finished",
        Payload::NodeStarted { .. } => "node_started",
        Payload::NodeWaiting { .. } => "node_waiting",
        Payload::NodeResumed { .. } => "node_resumed",
        Payload::NodeCancelled { .. } => "node_cancelled",
        Payload::NodeCompleted { .. } => "node_completed",
        Payload::NodeFailed { .. } => "node_failed",
        Payload::BranchSelected { .. } => "branch_selected",
        Payload::ChildStarted { .. } => "child_started",
    };
    attrs.push(KeyValue::new(
        attr::LASH_LANGUAGE_EXECUTION_LANGUAGE,
        language.to_string(),
    ));
    attrs.push(KeyValue::new(attr::LASH_LANGUAGE_EXECUTION_KIND, kind));

    attrs.push(KeyValue::new(
        attr::LASH_LANGUAGE_EXECUTION_EVENT_KEY,
        event.event_key.clone(),
    ));
    attrs.push(KeyValue::new(
        attr::LASH_LANGUAGE_EXECUTION_GRAPH_KEY,
        event.identity.graph_key(),
    ));
    if let Some(attempt) = event.identity.attempt() {
        attrs.push(KeyValue::new(
            attr::LASH_LANGUAGE_EXECUTION_ATTEMPT,
            i64::from(attempt),
        ));
    }
    if let Some(incarnation) = event.identity.incarnation() {
        attrs.push(KeyValue::new(
            attr::LASH_LANGUAGE_EXECUTION_INCARNATION,
            incarnation as i64,
        ));
    }
    if let Some(session_id) = &event.identity.scope.session_id {
        attrs.push(KeyValue::new(
            attr::LASH_LANGUAGE_EXECUTION_SESSION_ID,
            session_id.to_string(),
        ));
    }
    if let Some(turn_id) = &event.identity.scope.turn_id {
        attrs.push(KeyValue::new(
            attr::LASH_LANGUAGE_EXECUTION_TURN_ID,
            turn_id.to_string(),
        ));
    }
    attrs.push(KeyValue::new(
        attr::LASH_LANGUAGE_EXECUTION_SOURCE_IDENTITY,
        event.identity.source_identity.clone(),
    ));
    attrs.push(KeyValue::new(
        attr::LASH_LANGUAGE_EXECUTION_MODULE_REF,
        event.identity.module_ref.clone(),
    ));
    attrs.push(KeyValue::new(
        attr::LASH_LANGUAGE_EXECUTION_ENTRY_KIND,
        event.identity.entry_kind.clone(),
    ));
    push_opt(
        attrs,
        attr::LASH_LANGUAGE_EXECUTION_ENTRY_REF,
        &event.identity.entry_ref,
    );
    attrs.push(KeyValue::new(
        attr::LASH_LANGUAGE_EXECUTION_ENTRY_NAME,
        event.identity.entry_name.clone(),
    ));
    push_opt(
        attrs,
        attr::LASH_LANGUAGE_EXECUTION_ENGINE_EXECUTION_ID,
        &event.identity.engine_execution_id,
    );
    match &event.identity.subject {
        crate::TraceRuntimeSubject::Effect { effect_id, .. } => {
            attrs.push(KeyValue::new(
                attr::LASH_LANGUAGE_EXECUTION_SUBJECT_TYPE,
                "effect",
            ));
            attrs.push(KeyValue::new(
                attr::LASH_LANGUAGE_EXECUTION_EFFECT_ID,
                effect_id.clone(),
            ));
        }
        crate::TraceRuntimeSubject::Process { process_id } => {
            attrs.push(KeyValue::new(
                attr::LASH_LANGUAGE_EXECUTION_SUBJECT_TYPE,
                "process",
            ));
            attrs.push(KeyValue::new(
                attr::LASH_LANGUAGE_EXECUTION_PROCESS_ID,
                process_id.to_string(),
            ));
        }
    }

    match &event.payload {
        Payload::NodeStarted {
            node_id,
            node_kind,
            occurrence,
            call_id,
            ..
        }
        | Payload::NodeCompleted {
            node_id,
            node_kind,
            occurrence,
            call_id,
            ..
        }
        | Payload::NodeFailed {
            node_id,
            node_kind,
            occurrence,
            call_id,
            ..
        } => {
            attrs.push(KeyValue::new(
                attr::LASH_LANGUAGE_EXECUTION_NODE_ID,
                node_id.clone(),
            ));
            attrs.push(KeyValue::new(
                attr::LASH_LANGUAGE_EXECUTION_NODE_KIND,
                node_kind.as_str(),
            ));
            attrs.push(KeyValue::new(
                attr::LASH_LANGUAGE_EXECUTION_OCCURRENCE,
                *occurrence as i64,
            ));
            push_opt(attrs, attr::LASH_LANGUAGE_EXECUTION_CALL_ID, call_id);
        }
        Payload::NodeWaiting {
            node_id,
            node_kind,
            occurrence,
            awaited,
            ..
        } => {
            attrs.push(KeyValue::new(
                attr::LASH_LANGUAGE_EXECUTION_NODE_ID,
                node_id.clone(),
            ));
            attrs.push(KeyValue::new(
                attr::LASH_LANGUAGE_EXECUTION_NODE_KIND,
                node_kind.as_str(),
            ));
            attrs.push(KeyValue::new(
                attr::LASH_LANGUAGE_EXECUTION_OCCURRENCE,
                *occurrence as i64,
            ));
            attrs.push(KeyValue::new(
                "lash.language_execution.wait_kind",
                awaited.kind().as_str(),
            ));
            match awaited {
                crate::TraceNodeAwaited::Sleep { deadline_ms } => {
                    if let Some(deadline_ms) = deadline_ms {
                        attrs.push(KeyValue::new(
                            "lash.language_execution.deadline_ms",
                            *deadline_ms as i64,
                        ));
                    }
                }
                crate::TraceNodeAwaited::Signal { name, key } => {
                    attrs.push(KeyValue::new(
                        "lash.language_execution.awaited_signal",
                        name.clone(),
                    ));
                    attrs.push(KeyValue::new(
                        "lash.language_execution.awaited_key",
                        key.clone(),
                    ));
                }
                crate::TraceNodeAwaited::ChildProcesses { process_ids } => {
                    attrs.push(KeyValue::new(
                        "lash.language_execution.awaited_process_ids",
                        process_ids
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join(","),
                    ));
                }
                crate::TraceNodeAwaited::ToolBatch { batch_id, position } => {
                    attrs.push(KeyValue::new(
                        "lash.language_execution.awaited_batch_id",
                        batch_id.clone(),
                    ));
                    attrs.push(KeyValue::new(
                        "lash.language_execution.awaited_position",
                        *position as i64,
                    ));
                }
                crate::TraceNodeAwaited::EffectGroup {
                    group_key,
                    position,
                    wake,
                } => {
                    attrs.push(KeyValue::new(
                        "lash.language_execution.awaited_group_key",
                        group_key.clone(),
                    ));
                    attrs.push(KeyValue::new(
                        "lash.language_execution.awaited_position",
                        *position as i64,
                    ));
                    attrs.push(KeyValue::new(
                        "lash.language_execution.wake_policy",
                        wake.as_str(),
                    ));
                }
            }
        }
        Payload::NodeResumed {
            node_id,
            node_kind,
            occurrence,
            resolution,
            ..
        } => {
            attrs.push(KeyValue::new(
                attr::LASH_LANGUAGE_EXECUTION_NODE_ID,
                node_id.clone(),
            ));
            attrs.push(KeyValue::new(
                attr::LASH_LANGUAGE_EXECUTION_NODE_KIND,
                node_kind.as_str(),
            ));
            attrs.push(KeyValue::new(
                attr::LASH_LANGUAGE_EXECUTION_OCCURRENCE,
                *occurrence as i64,
            ));
            attrs.push(KeyValue::new(
                "lash.language_execution.wait_resolution",
                resolution.as_str(),
            ));
        }
        Payload::NodeCancelled {
            node_id,
            node_kind,
            occurrence,
            ..
        } => {
            attrs.push(KeyValue::new(
                attr::LASH_LANGUAGE_EXECUTION_NODE_ID,
                node_id.clone(),
            ));
            attrs.push(KeyValue::new(
                attr::LASH_LANGUAGE_EXECUTION_NODE_KIND,
                node_kind.as_str(),
            ));
            attrs.push(KeyValue::new(
                attr::LASH_LANGUAGE_EXECUTION_OCCURRENCE,
                *occurrence as i64,
            ));
        }
        Payload::BranchSelected {
            node_id,
            occurrence,
            edge_id,
            selected,
            ..
        } => {
            attrs.push(KeyValue::new(
                attr::LASH_LANGUAGE_EXECUTION_NODE_ID,
                node_id.clone(),
            ));
            attrs.push(KeyValue::new(
                attr::LASH_LANGUAGE_EXECUTION_EDGE_ID,
                edge_id.clone(),
            ));
            attrs.push(KeyValue::new(
                attr::LASH_LANGUAGE_EXECUTION_BRANCH,
                format!("{selected:?}").to_ascii_lowercase(),
            ));
            attrs.push(KeyValue::new(
                attr::LASH_LANGUAGE_EXECUTION_OCCURRENCE,
                *occurrence as i64,
            ));
        }
        Payload::ChildStarted {
            parent_node_id,
            child,
            ..
        } => {
            attrs.push(KeyValue::new(
                attr::LASH_LANGUAGE_EXECUTION_PARENT_NODE_ID,
                parent_node_id.clone(),
            ));
            if let Some(graph_key) = child.graph_key() {
                attrs.push(KeyValue::new(
                    attr::LASH_LANGUAGE_EXECUTION_CHILD_GRAPH_KEY,
                    graph_key,
                ));
            }
            attrs.push(KeyValue::new(
                attr::LASH_LANGUAGE_EXECUTION_CHILD_PROCESS_ID,
                child.process_id.to_string(),
            ));
            attrs.push(KeyValue::new(
                attr::LASH_LANGUAGE_EXECUTION_CHILD_INCARNATION,
                child.incarnation as i64,
            ));
            if let Some(attempt) = child.attempt {
                attrs.push(KeyValue::new(
                    attr::LASH_LANGUAGE_EXECUTION_CHILD_ATTEMPT,
                    attempt as i64,
                ));
            }
        }
        Payload::ExecutionFinished { status, error, .. } => {
            attrs.push(KeyValue::new(
                attr::LASH_LANGUAGE_EXECUTION_STATUS,
                format!("{status:?}").to_ascii_lowercase(),
            ));
            push_opt(attrs, attr::LASH_LANGUAGE_EXECUTION_ERROR, error);
        }
        Payload::ExecutionStarted { execution_map, .. } => {
            attrs.push(KeyValue::new(
                attr::LASH_LANGUAGE_EXECUTION_NODE_COUNT,
                execution_map.nodes.len() as i64,
            ));
            attrs.push(KeyValue::new(
                attr::LASH_LANGUAGE_EXECUTION_EDGE_COUNT,
                execution_map.edges.len() as i64,
            ));
        }
    }
    if let Payload::NodeFailed { failure, .. } = &event.payload {
        use crate::TraceLanguageExecutionFailure as Failure;
        let (kind, code, message) = match failure {
            Failure::Effect { code, message, .. } => ("effect", code, message),
            Failure::Runtime { code, message } => ("runtime", code, message),
        };
        attrs.push(KeyValue::new(
            attr::LASH_LANGUAGE_EXECUTION_FAILURE_KIND,
            kind,
        ));
        attrs.push(KeyValue::new(
            attr::LASH_LANGUAGE_EXECUTION_FAILURE_CODE,
            code.clone(),
        ));
        attrs.push(KeyValue::new(
            attr::LASH_LANGUAGE_EXECUTION_FAILURE_MESSAGE,
            message.clone(),
        ));
        if let Failure::Effect {
            class,
            replay_key,
            source,
            retry,
            ..
        } = failure
        {
            let class = match class {
                lash_sansio::ToolFailureClass::InvalidRequest => "invalid_request",
                lash_sansio::ToolFailureClass::Io => "io",
                lash_sansio::ToolFailureClass::Unavailable => "unavailable",
                lash_sansio::ToolFailureClass::PermissionDenied => "permission_denied",
                lash_sansio::ToolFailureClass::Timeout => "timeout",
                lash_sansio::ToolFailureClass::Execution => "execution",
                lash_sansio::ToolFailureClass::External => "external",
                lash_sansio::ToolFailureClass::ResourceLimit => "resource_limit",
                lash_sansio::ToolFailureClass::Internal => "internal",
            };
            attrs.push(KeyValue::new(
                attr::LASH_LANGUAGE_EXECUTION_FAILURE_CLASS,
                class,
            ));
            attrs.push(KeyValue::new(
                attr::LASH_LANGUAGE_EXECUTION_FAILURE_REPLAY_KEY,
                replay_key.clone(),
            ));
            let source = match source {
                lash_sansio::ToolFailureSource::Runtime => "runtime",
                lash_sansio::ToolFailureSource::Tool => "tool",
                lash_sansio::ToolFailureSource::Plugin => "plugin",
                lash_sansio::ToolFailureSource::Policy => "policy",
                lash_sansio::ToolFailureSource::Cancellation => "cancellation",
                lash_sansio::ToolFailureSource::UnknownLegacy => "unknown_legacy",
            };
            attrs.push(KeyValue::new(
                attr::LASH_LANGUAGE_EXECUTION_FAILURE_SOURCE,
                source,
            ));
            let retry_name = match retry {
                lash_sansio::ToolRetryStatus::Never => "never",
                lash_sansio::ToolRetryStatus::Safe { after_ms } => {
                    if let Some(after_ms) = after_ms {
                        attrs.push(KeyValue::new(
                            attr::LASH_LANGUAGE_EXECUTION_FAILURE_RETRY_AFTER_MS,
                            *after_ms as i64,
                        ));
                    }
                    "safe"
                }
                lash_sansio::ToolRetryStatus::Exhausted { attempts } => {
                    attrs.push(KeyValue::new(
                        attr::LASH_LANGUAGE_EXECUTION_FAILURE_RETRY_ATTEMPTS,
                        i64::from(*attempts),
                    ));
                    "exhausted"
                }
                lash_sansio::ToolRetryStatus::UnknownLegacy => "unknown_legacy",
            };
            attrs.push(KeyValue::new(
                attr::LASH_LANGUAGE_EXECUTION_FAILURE_RETRY,
                retry_name,
            ));
        }
    }
}
