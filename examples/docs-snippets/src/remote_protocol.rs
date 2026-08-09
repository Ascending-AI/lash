//! Compiled sources for the Rust snippets on `docs/remote-protocol.html`.

fn remote_turn_request(
    chat_id: String,
    turn_id: String,
    idempotency_key: String,
    trace_turn_id: String,
) -> anyhow::Result<()> {
    // docs:start:remote-turn-request
    use lash::remote::REMOTE_PROTOCOL_VERSION;
    use lash::remote::turn_input::{RemoteInputItem, RemoteTurnInput, RemoteTurnRequest};

    let request = RemoteTurnRequest {
        protocol_version: REMOTE_PROTOCOL_VERSION,
        session_id: chat_id.clone(),
        turn_id: turn_id.clone(),
        idempotency_key: Some(idempotency_key.clone()),
        input: RemoteTurnInput {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            items: vec![RemoteInputItem::Text {
                text: "Summarize this task.".to_string(),
            }],
            protocol_turn_options: None,
            trace_turn_id: Some(trace_turn_id.clone()),
            prompt_layer: None,
        },
        tool_grants: Vec::new(),
        metadata: Default::default(),
    };

    request.validate()?;
    // docs:end:remote-turn-request
    Ok(())
}

fn remote_process_start_request() -> anyhow::Result<()> {
    // docs:start:remote-process-start-request
    use std::collections::BTreeMap;

    use lash::remote::REMOTE_PROTOCOL_VERSION;
    use lash::remote::processes::{
        RemoteProcessExecutionEnvSpec, RemoteProcessExecutionPolicy, RemoteProcessInput,
        RemoteProcessModelLimits, RemoteProcessModelSpec, RemoteProcessOriginator,
        RemoteProcessPluginOptions, RemoteProcessStartRequest, RemoteRecoveryDisposition,
        RemoteTurnBudget,
    };
    use serde_json::json;

    let request = RemoteProcessStartRequest {
        protocol_version: REMOTE_PROTOCOL_VERSION,
        id: "process-01".to_string(),
        input: RemoteProcessInput::External {
            metadata: json!({ "source": "scheduler" }),
        },
        disposition: RemoteRecoveryDisposition::ExternallyOwned,
        max_attempts: None,
        env_spec: Some(RemoteProcessExecutionEnvSpec {
            plugin_options: RemoteProcessPluginOptions {
                plugins: BTreeMap::from([(
                    "snapshot-tools".to_string(),
                    json!({ "snapshot_ref": "tool-authority:sha256:abc123" }),
                )]),
            },
            policy: RemoteProcessExecutionPolicy {
                provider_id: "example-provider".to_string(),
                model: RemoteProcessModelSpec {
                    id: "example-model".to_string(),
                    variant: Default::default(),
                    capability: Default::default(),
                    limits: RemoteProcessModelLimits {
                        context_window_tokens: 128_000,
                        output_token_capacity: Some(8_192),
                    },
                },
                ..RemoteProcessExecutionPolicy::new(RemoteTurnBudget::Unbounded)
            },
        }),
        originator: RemoteProcessOriginator::Host { scope: None },
        identity: None,
        wake_session_id: None,
        observers: Vec::new(),
        event_types: Vec::new(),
    };

    request.validate()?;
    // docs:end:remote-process-start-request
    Ok(())
}

#[cfg(test)]
mod asserted_process_examples {
    use std::collections::BTreeMap;

    use lash::remote::REMOTE_PROTOCOL_VERSION;
    use lash::remote::processes::{
        RemoteAbandonEvidence, RemoteAbandonRequest, RemoteAbandonWriter, RemoteObservedProcess,
        RemoteObservedProcessEvent, RemotePersistProcessEnvRequest, RemotePersistProcessEnvResult,
        RemoteProcessAwaitOutput, RemoteProcessAwaitRequest, RemoteProcessAwaitResult,
        RemoteProcessCancelRequest, RemoteProcessCancelResult, RemoteProcessDefinitionIdentity,
        RemoteProcessEvent, RemoteProcessEventSemantics, RemoteProcessEventSemanticsSpec,
        RemoteProcessEventType, RemoteProcessEventsRequest, RemoteProcessEventsResponse,
        RemoteProcessExecutionEnvRef, RemoteProcessExecutionEnvSpec, RemoteProcessExecutionPolicy,
        RemoteProcessExternalRef, RemoteProcessInput, RemoteProcessListFilter,
        RemoteProcessListResponse, RemoteProcessModelLimits, RemoteProcessModelSpec,
        RemoteProcessOriginator, RemoteProcessPluginOptions, RemoteProcessProvenance,
        RemoteProcessSignalRequest, RemoteProcessSignalResult, RemoteProcessStartRequest,
        RemoteProcessStartResult, RemoteProcessStarted, RemoteProcessStatus,
        RemoteProcessStatusFilter, RemoteProcessSummary, RemoteProcessTerminalSemantics,
        RemoteProcessTerminalSpec, RemoteProcessValueSelector, RemoteProcessWaitKind,
        RemoteProcessWaitState, RemoteProcessWake, RemoteProcessWakeSpec, RemoteProcessWorkItem,
        RemoteProcessWorkSnapshot, RemoteRecoveryDisposition, RemoteRuntimeEffectKind,
        RemoteRuntimeInvocation, RemoteRuntimeReplay, RemoteRuntimeScope, RemoteRuntimeSubject,
        RemoteSessionScope, RemoteToolFailureClass, RemoteTurnBudget,
    };
    use lash::remote::turn_result::RemoteCausalRef;
    use lash_remote_protocol::processes::{RemoteProcessIdentity, RemoteProcessRecord};
    use serde_json::json;

    fn process_identity() -> RemoteProcessIdentity {
        RemoteProcessIdentity {
            kind: "report-export".to_string(),
            label: Some("Nightly invoice export".to_string()),
            definition: Some(RemoteProcessDefinitionIdentity {
                value: json!({ "workflow": "invoice-export", "revision": 7 }),
            }),
        }
    }

    fn process_env_ref() -> RemoteProcessExecutionEnvRef {
        RemoteProcessExecutionEnvRef::parse(format!(
            "{}{}",
            RemoteProcessExecutionEnvRef::PREFIX,
            "a".repeat(64)
        ))
        .expect("canonical process environment reference")
    }

    fn running_record() -> RemoteProcessRecord {
        RemoteProcessRecord {
            process_id: "invoice-export".to_string(),
            input: RemoteProcessInput::Engine {
                kind: "report-export".to_string(),
                payload: json!({ "format": "csv", "rows": 12 }),
            },
            disposition: RemoteRecoveryDisposition::Rerunnable,
            max_attempts: Some(3),
            identity: process_identity(),
            event_types: vec![progress_event_type()],
            provenance: process_provenance(),
            env_ref: Some(process_env_ref()),
            created_at_ms: 1_720_000_000_000,
            updated_at_ms: 1_720_000_000_100,
            external_ref: Some(RemoteProcessExternalRef {
                backend: "restate".to_string(),
                id: "invocation-778".to_string(),
                metadata: Some(json!({ "region": "eu-central-1" })),
            }),
            first_started: Some(RemoteProcessStarted {
                owner: json!({
                    "owner_id": "worker-berlin",
                    "incarnation_id": "boot-9",
                    "liveness": { "type": "opaque" }
                }),
                fencing_token: 1,
                attempt: 1,
                started_at_ms: 1_720_000_000_010,
            }),
            abandon_request: None,
            wait: Some(approval_wait()),
            status: RemoteProcessStatus::Waiting,
            outcome: None,
        }
    }

    fn process_provenance() -> RemoteProcessProvenance {
        RemoteProcessProvenance {
            originator: RemoteProcessOriginator::Session {
                session_id: "session-finance".to_string(),
            },
            caused_by: Some(RemoteCausalRef::TriggerOccurrence {
                occurrence_id: "occurrence-42".to_string(),
                subscription_id: Some("subscription-nightly".to_string()),
                subscription_incarnation: Some("incarnation-blue".to_string()),
                subscription_revision: Some(7),
            }),
        }
    }

    fn approval_wait() -> RemoteProcessWaitState {
        RemoteProcessWaitState {
            kind: RemoteProcessWaitKind::Signal {
                name: "approval".to_string(),
                event_type: "signal.approval".to_string(),
                key: "process:invoice-export:signal.approval:1".to_string(),
                ordinal: 1,
            },
            since_ms: 1_720_000_000_050,
        }
    }

    fn progress_event_type() -> RemoteProcessEventType {
        RemoteProcessEventType {
            name: "progress".to_string(),
            payload_schema: json!({
                "type": "object",
                "required": ["completed_rows", "total_rows"]
            }),
            semantics: RemoteProcessEventSemanticsSpec {
                terminal: None,
                wake: Some(RemoteProcessWakeSpec {
                    when: Some(RemoteProcessValueSelector::Present(
                        "/completed_rows".to_string(),
                    )),
                    input: RemoteProcessValueSelector::Template {
                        template: "Exported {{completed}} of {{total}} rows".to_string(),
                        fields: BTreeMap::from([
                            (
                                "completed".to_string(),
                                RemoteProcessValueSelector::Pointer("/completed_rows".to_string()),
                            ),
                            (
                                "total".to_string(),
                                RemoteProcessValueSelector::Pointer("/total_rows".to_string()),
                            ),
                        ]),
                    },
                }),
            },
        }
    }

    fn runtime_invocation() -> RemoteRuntimeInvocation {
        RemoteRuntimeInvocation {
            scope: RemoteRuntimeScope {
                session_id: "session-finance".to_string(),
                turn_id: Some("turn-17".to_string()),
                turn_index: Some(4),
                protocol_iteration: Some(2),
            },
            subject: RemoteRuntimeSubject::ProcessEvent {
                process_id: "invoice-export".to_string(),
                sequence: 3,
                event_type: "signal.approval".to_string(),
            },
            caused_by: Some(RemoteCausalRef::Process {
                process_id: "invoice-export".to_string(),
            }),
            replay: Some(RemoteRuntimeReplay {
                key: "invoice-export:signal:approval:1".to_string(),
            }),
        }
    }

    fn signal_event() -> RemoteProcessEvent {
        RemoteProcessEvent {
            process_id: "invoice-export".to_string(),
            sequence: 3,
            event_type: "signal.approval".to_string(),
            payload: json!({ "approved": true, "reviewer": "ops@example.com" }),
            invocation: Some(runtime_invocation()),
            semantics: RemoteProcessEventSemantics {
                terminal: None,
                wake: Some(RemoteProcessWake {
                    input: "Continue the approved invoice export.".to_string(),
                }),
            },
            occurred_at_ms: 1_720_000_000_200,
        }
    }

    fn observed_process() -> RemoteObservedProcess {
        RemoteObservedProcess {
            process_id: "invoice-export".to_string(),
            graph_key: "process:invoice-export".to_string(),
            kind: "report-export".to_string(),
            identity: process_identity(),
            lifecycle: RemoteProcessStatus::Waiting,
            status_label: "waiting".to_string(),
            terminal: false,
            disposition: RemoteRecoveryDisposition::Rerunnable,
            error: None,
            created_at_ms: 1_720_000_000_000,
            updated_at_ms: 1_720_000_000_100,
            first_started: running_record().first_started,
            lease_holder: Some(json!({
                "owner_id": "worker-berlin",
                "incarnation_id": "boot-9",
                "liveness": { "type": "opaque" }
            })),
            lease_expires_at_ms: Some(1_720_000_060_000),
            abandon_request: None,
            input: RemoteProcessInput::Engine {
                kind: "report-export".to_string(),
                payload: json!({ "format": "csv", "rows": 12 }),
            },
            originator: RemoteProcessOriginator::Session {
                session_id: "session-finance".to_string(),
            },
            env_ref: Some(process_env_ref()),
            caused_by: process_provenance().caused_by,
            external_ref: running_record().external_ref,
            wait: Some(approval_wait()),
            child_session_id: None,
            label: "Nightly invoice export".to_string(),
        }
    }

    #[test]
    fn remote_process_start_contract_validates_captured_environment_and_identity() {
        let scope = RemoteSessionScope::new("session-finance");
        RemoteSessionScope::validate(&scope, "RemoteSessionScope").expect("valid session scope");
        assert_eq!(scope.session_id, "session-finance");
        assert!(scope.agent_frame_id.is_none());

        let env_ref = process_env_ref();
        RemoteProcessExecutionEnvRef::validate(&env_ref, "RemoteProcessExecutionEnvRef")
            .expect("valid process environment reference");
        assert!(
            env_ref
                .as_str()
                .starts_with(RemoteProcessExecutionEnvRef::PREFIX)
        );
        assert!(RemoteProcessExecutionEnvRef::parse("process-env:sha256:not-a-digest").is_err());

        let plugin_options = RemoteProcessPluginOptions {
            plugins: BTreeMap::from([(
                "snapshot-tools".to_string(),
                json!({ "snapshot_ref": "tool-authority:sha256:abc123" }),
            )]),
        };
        assert!(!plugin_options.is_empty());
        let expected_budget = RemoteTurnBudget::Bounded(
            std::num::NonZeroUsize::new(8).expect("non-zero turn budget"),
        );
        let policy = RemoteProcessExecutionPolicy {
            model: RemoteProcessModelSpec {
                id: "example-model".to_string(),
                variant: Default::default(),
                capability: Default::default(),
                limits: RemoteProcessModelLimits {
                    context_window_tokens: 128_000,
                    output_token_capacity: Some(8_192),
                },
            },
            provider_id: "example-provider".to_string(),
            session_id: Some("process-session-invoice-export".to_string()),
            autonomous: true,
            turn_budget: expected_budget,
            prompt: Default::default(),
            generation: Default::default(),
        };
        assert_eq!(policy.turn_budget, expected_budget);
        assert_eq!(policy.prompt, Default::default());
        assert_eq!(policy.generation, Default::default());
        let env_spec = RemoteProcessExecutionEnvSpec {
            plugin_options,
            policy,
        };
        RemoteProcessExecutionEnvSpec::validate(&env_spec, "RemoteProcessExecutionEnvSpec")
            .expect("valid captured environment");
        assert_eq!(env_spec.policy.turn_budget, expected_budget);

        let request = RemoteProcessStartRequest {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            id: "invoice-export".to_string(),
            input: RemoteProcessInput::Engine {
                kind: "report-export".to_string(),
                payload: json!({ "format": "csv", "rows": 12 }),
            },
            disposition: RemoteRecoveryDisposition::Rerunnable,
            max_attempts: Some(3),
            env_spec: Some(env_spec.clone()),
            originator: RemoteProcessOriginator::Session {
                session_id: "session-finance".to_string(),
            },
            identity: Some(process_identity()),
            wake_session_id: Some("session-finance".to_string()),
            observers: vec!["session-finance".to_string(), "session-ops".to_string()],
            event_types: vec![progress_event_type()],
        };
        RemoteProcessStartRequest::validate(&request).expect("valid remote process start request");
        RemoteProcessOriginator::validate(&request.originator, "RemoteProcessOriginator")
            .expect("valid process originator");
        RemoteProcessEventType::validate(&request.event_types[0], "RemoteProcessEventType")
            .expect("valid declared process event");
        RemoteProcessEventSemanticsSpec::validate(
            &request.event_types[0].semantics,
            "RemoteProcessEventSemanticsSpec",
        )
        .expect("valid declared event semantics");
        RemoteProcessWakeSpec::validate(
            request.event_types[0]
                .semantics
                .wake
                .as_ref()
                .expect("declared wake projection"),
            "RemoteProcessWakeSpec",
        )
        .expect("valid declared wake projection");
        RemoteProcessProvenance::validate(&running_record().provenance, "RemoteProcessProvenance")
            .expect("valid process provenance");
        let request_json = serde_json::to_value(&request).expect("start request serializes");
        assert_eq!(request_json["id"], "invoice-export");
        assert_eq!(request_json["input"]["type"], "engine");
        assert_eq!(request_json["input"]["payload"]["rows"], 12);
        assert_eq!(request_json["disposition"], "rerunnable");
        assert_eq!(request_json["max_attempts"], 3);
        assert_eq!(request_json["originator"]["type"], "session");
        assert_eq!(request_json["wake_session_id"], "session-finance");
        assert_eq!(request_json["observers"].as_array().unwrap().len(), 2);
        assert_eq!(request_json["event_types"][0]["name"], "progress");
        assert_eq!(
            request_json["event_types"][0]["semantics"]["wake"]["when"]["present"],
            "/completed_rows"
        );
        assert_eq!(
            request_json["event_types"][0]["semantics"]["wake"]["input"]["template"]["template"],
            "Exported {{completed}} of {{total}} rows"
        );
        assert_eq!(
            request_json["event_types"][0]["semantics"]["wake"]["input"]["template"]["fields"]["completed"]
                ["pointer"],
            "/completed_rows"
        );
        assert_eq!(
            request_json["env_spec"]["policy"]["model"]["limits"]["context_window_tokens"],
            128_000
        );
        assert_eq!(
            request_json["env_spec"]["plugin_options"]["plugins"]["snapshot-tools"]["snapshot_ref"],
            "tool-authority:sha256:abc123"
        );
        assert_eq!(
            request_json["env_spec"]["policy"]["provider_id"],
            "example-provider"
        );
        assert_eq!(
            request_json["env_spec"]["policy"]["session_id"],
            "process-session-invoice-export"
        );
        assert_eq!(request_json["env_spec"]["policy"]["autonomous"], true);
        assert_eq!(
            request_json["env_spec"]["policy"]["turn_budget"],
            json!({ "bounded": 8 })
        );
        assert_eq!(
            request_json["env_spec"]["policy"]["model"]["id"],
            "example-model"
        );
        assert_eq!(
            request_json["env_spec"]["policy"]["model"]["limits"]["output_token_capacity"],
            8_192
        );
        let mut invalid_request = request.clone();
        invalid_request.max_attempts = Some(0);
        assert!(RemoteProcessStartRequest::validate(&invalid_request).is_err());

        let persist = RemotePersistProcessEnvRequest {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            env_spec,
        };
        RemotePersistProcessEnvRequest::validate(&persist)
            .expect("valid environment persist request");
        let persisted = RemotePersistProcessEnvResult {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            env_ref: env_ref.clone(),
        };
        RemotePersistProcessEnvResult::validate(&persisted)
            .expect("valid environment persist result");
        assert_eq!(persisted.env_ref, env_ref);

        let start_result = RemoteProcessStartResult {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            record: running_record(),
            summary: Some(RemoteProcessSummary {
                handle_type: "process".to_string(),
                id: "invoice-export".to_string(),
                process_id: "invoice-export".to_string(),
                kind: "report-export".to_string(),
                label: Some("Nightly invoice export".to_string()),
                definition: Some(RemoteProcessDefinitionIdentity {
                    value: json!({ "workflow": "invoice-export", "revision": 7 }),
                }),
                status: RemoteProcessStatus::Waiting,
            }),
        };
        RemoteProcessStartResult::validate(&start_result).expect("valid process start result");
        RemoteProcessSummary::validate(
            start_result.summary.as_ref().expect("start summary"),
            "RemoteProcessSummary",
        )
        .expect("valid process handle summary");
        RemoteProcessDefinitionIdentity::validate(
            start_result
                .summary
                .as_ref()
                .and_then(|summary| summary.definition.as_ref())
                .expect("summary definition"),
            "RemoteProcessDefinitionIdentity",
        )
        .expect("valid process definition identity");
        RemoteProcessExternalRef::validate(
            start_result
                .record
                .external_ref
                .as_ref()
                .expect("external backend reference"),
            "RemoteProcessExternalRef",
        )
        .expect("valid external backend reference");
        RemoteProcessWaitState::validate(
            start_result.record.wait.as_ref().expect("process wait"),
            "RemoteProcessWaitState",
        )
        .expect("valid signal wait");
        let start_json = serde_json::to_value(&start_result).expect("start result serializes");
        assert_eq!(start_json["record"]["status"], "waiting");
        assert_eq!(start_json["record"]["wait"]["kind"]["kind"], "signal");
        assert_eq!(start_json["summary"]["__handle__"], "process");
        assert_eq!(start_json["summary"]["definition"]["value"]["revision"], 7);
    }

    #[test]
    fn remote_process_work_projection_preserves_events_and_runtime_provenance() {
        let snapshot = RemoteProcessWorkSnapshot {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            session_id: "session-finance".to_string(),
            visible_process_ids: vec!["invoice-export".to_string()],
            items: vec![RemoteProcessWorkItem {
                process: observed_process(),
                events: vec![RemoteObservedProcessEvent {
                    sequence: 2,
                    event_type: "progress".to_string(),
                    occurred_at_ms: 1_720_000_000_150,
                    payload: json!({ "completed_rows": 8, "total_rows": 12 }),
                }],
                kind: "report-export".to_string(),
                label: "Nightly invoice export".to_string(),
            }],
        };
        RemoteObservedProcess::validate(&snapshot.items[0].process, "RemoteObservedProcess")
            .expect("valid observed process");
        RemoteObservedProcessEvent::validate(
            &snapshot.items[0].events[0],
            "RemoteObservedProcessEvent",
        )
        .expect("valid observed process event");
        RemoteProcessWorkItem::validate(&snapshot.items[0], "RemoteProcessWorkItem")
            .expect("valid observed work item");
        RemoteProcessWorkSnapshot::validate(&snapshot).expect("valid visible work snapshot");
        let snapshot_json = serde_json::to_value(&snapshot).expect("work snapshot serializes");
        assert_eq!(
            snapshot_json["visible_process_ids"],
            json!(["invoice-export"])
        );
        assert_eq!(
            snapshot_json["items"][0]["process"]["status_label"],
            "waiting"
        );
        assert_eq!(snapshot_json["items"][0]["process"]["terminal"], false);
        let projected_process = &snapshot_json["items"][0]["process"];
        assert_eq!(projected_process["process_id"], "invoice-export");
        assert_eq!(projected_process["graph_key"], "process:invoice-export");
        assert_eq!(projected_process["kind"], "report-export");
        assert_eq!(
            projected_process["identity"]["label"],
            "Nightly invoice export"
        );
        assert_eq!(
            projected_process["identity"]["definition"]["value"]["revision"],
            7
        );
        assert_eq!(projected_process["lifecycle"], "waiting");
        assert_eq!(projected_process["disposition"], "rerunnable");
        assert_eq!(projected_process["created_at_ms"], 1_720_000_000_000_u64);
        assert_eq!(projected_process["updated_at_ms"], 1_720_000_000_100_u64);
        assert_eq!(projected_process["first_started"]["attempt"], 1);
        assert_eq!(
            projected_process["lease_holder"]["owner_id"],
            "worker-berlin"
        );
        assert_eq!(
            projected_process["lease_expires_at_ms"],
            1_720_000_060_000_u64
        );
        assert_eq!(projected_process["input"]["kind"], "report-export");
        assert_eq!(projected_process["input"]["payload"]["rows"], 12);
        assert_eq!(
            projected_process["originator"]["session_id"],
            "session-finance"
        );
        assert_eq!(
            projected_process["caused_by"]["occurrence_id"],
            "occurrence-42"
        );
        assert_eq!(projected_process["external_ref"]["id"], "invocation-778");
        assert_eq!(projected_process["wait"]["kind"]["ordinal"], 1);
        assert_eq!(projected_process["label"], "Nightly invoice export");
        assert!(projected_process.get("error").is_none());
        assert!(projected_process.get("abandon_request").is_none());
        assert!(projected_process.get("child_session_id").is_none());
        assert_eq!(
            snapshot_json["items"][0]["events"][0]["payload"]["completed_rows"],
            8
        );
        assert_eq!(snapshot_json["items"][0]["label"], "Nightly invoice export");

        let signal = RemoteProcessSignalRequest {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            process_id: "invoice-export".to_string(),
            signal_name: "approval".to_string(),
            signal_id: "approval-1".to_string(),
            payload: json!({ "approved": true, "reviewer": "ops@example.com" }),
            replay_key: Some("invoice-export:signal:approval:1".to_string()),
        };
        RemoteProcessSignalRequest::validate(&signal).expect("valid signal request");
        assert_eq!(signal.payload["approved"], true);
        assert_eq!(
            signal.replay_key.as_deref(),
            Some("invoice-export:signal:approval:1")
        );
        let signal_result = RemoteProcessSignalResult {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            event: signal_event(),
        };
        RemoteRuntimeScope::validate(
            &signal_result
                .event
                .invocation
                .as_ref()
                .expect("runtime invocation")
                .scope,
            "RemoteRuntimeScope",
        )
        .expect("valid runtime scope");
        RemoteRuntimeSubject::validate(
            &signal_result
                .event
                .invocation
                .as_ref()
                .expect("runtime invocation")
                .subject,
            "RemoteRuntimeSubject",
        )
        .expect("valid runtime subject");
        RemoteRuntimeInvocation::validate(
            signal_result
                .event
                .invocation
                .as_ref()
                .expect("runtime invocation"),
            "RemoteRuntimeInvocation",
        )
        .expect("valid runtime invocation");
        RemoteProcessWake::validate(
            signal_result
                .event
                .semantics
                .wake
                .as_ref()
                .expect("wake projection"),
            "RemoteProcessWake",
        )
        .expect("valid wake projection");
        RemoteProcessEventSemantics::validate(
            &signal_result.event.semantics,
            "RemoteProcessEventSemantics",
        )
        .expect("valid projected event semantics");
        RemoteProcessEvent::validate(&signal_result.event, "RemoteProcessEvent")
            .expect("valid signal event");
        RemoteProcessSignalResult::validate(&signal_result).expect("valid signal result");
        let signal_json = serde_json::to_value(&signal_result).expect("signal result serializes");
        assert_eq!(signal_json["event"]["sequence"], 3);
        assert_eq!(signal_json["event"]["process_id"], "invoice-export");
        assert_eq!(signal_json["event"]["event_type"], "signal.approval");
        assert_eq!(signal_json["event"]["payload"]["approved"], true);
        assert_eq!(
            signal_json["event"]["occurred_at_ms"],
            1_720_000_000_200_u64
        );
        assert_eq!(
            signal_json["event"]["invocation"]["subject"]["type"],
            "process_event"
        );
        assert_eq!(
            signal_json["event"]["invocation"]["scope"]["turn_id"],
            "turn-17"
        );
        assert_eq!(signal_json["event"]["invocation"]["scope"]["turn_index"], 4);
        assert_eq!(
            signal_json["event"]["invocation"]["scope"]["protocol_iteration"],
            2
        );
        assert_eq!(
            signal_json["event"]["invocation"]["replay"]["key"],
            "invoice-export:signal:approval:1"
        );
        assert_eq!(
            signal_json["event"]["semantics"]["wake"]["input"],
            "Continue the approved invoice export."
        );

        let events_request = RemoteProcessEventsRequest {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            process_id: "invoice-export".to_string(),
            after_sequence: 2,
        };
        RemoteProcessEventsRequest::validate(&events_request).expect("valid event-tail request");
        let events_response = RemoteProcessEventsResponse {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            process_id: "invoice-export".to_string(),
            events: vec![signal_event()],
        };
        RemoteProcessEventsResponse::validate(&events_response).expect("valid event-tail response");
        assert_eq!(events_response.events[0].event_type, "signal.approval");
        assert_eq!(
            events_response.events[0].payload["reviewer"],
            "ops@example.com"
        );
    }

    #[test]
    fn remote_process_controls_cover_filters_terminal_outputs_and_inputs() {
        let list_filter = RemoteProcessListFilter {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            definition: Some(RemoteProcessDefinitionIdentity {
                value: json!({ "workflow": "invoice-export", "revision": 7 }),
            }),
            status: RemoteProcessStatusFilter::Waiting,
            waiting: Some(true),
            originator_id: Some("session-finance".to_string()),
            identity_kind: Some("report-export".to_string()),
            identity_label: Some("Nightly invoice export".to_string()),
            caused_by_occurrence_id: Some("occurrence-42".to_string()),
            caused_by_subscription_id: Some("subscription-nightly".to_string()),
            created_at_start_ms: Some(1_720_000_000_000),
            created_at_end_ms: Some(1_720_000_001_000),
        };
        RemoteProcessListFilter::validate(&list_filter).expect("valid process list filter");
        let list_json = serde_json::to_value(&list_filter).expect("list filter serializes");
        assert_eq!(list_json["status"], "waiting");
        assert_eq!(list_json["waiting"], true);
        assert_eq!(list_json["definition"]["value"]["revision"], 7);
        assert_eq!(list_json["originator_id"], "session-finance");
        assert_eq!(list_json["identity_kind"], "report-export");
        assert_eq!(list_json["identity_label"], "Nightly invoice export");
        assert_eq!(list_json["caused_by_occurrence_id"], "occurrence-42");
        assert_eq!(
            list_json["caused_by_subscription_id"],
            "subscription-nightly"
        );
        assert_eq!(list_json["created_at_start_ms"], 1_720_000_000_000_u64);
        assert_eq!(list_json["created_at_end_ms"], 1_720_000_001_000_u64);
        let list_response = RemoteProcessListResponse {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            records: vec![observed_process()],
        };
        RemoteProcessListResponse::validate(&list_response).expect("valid process list response");
        assert_eq!(list_response.records[0].process_id, "invoice-export");
        assert_eq!(
            list_response.records[0].lease_holder.as_ref().unwrap()["owner_id"],
            "worker-berlin"
        );

        let await_request = RemoteProcessAwaitRequest {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            process_id: "invoice-export".to_string(),
        };
        RemoteProcessAwaitRequest::validate(&await_request).expect("valid process await request");
        let await_result = RemoteProcessAwaitResult {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            process_id: "invoice-export".to_string(),
            output: RemoteProcessAwaitOutput::Success {
                value: json!({ "artifact": "invoices.csv", "rows": 12 }),
                control: None,
            },
        };
        RemoteProcessAwaitResult::validate(&await_result).expect("valid process await result");
        let await_json = serde_json::to_value(&await_result).expect("await result serializes");
        assert_eq!(await_json["output"]["type"], "success");
        assert_eq!(await_json["output"]["value"]["artifact"], "invoices.csv");

        let cancel_request = RemoteProcessCancelRequest {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            process_id: "invoice-export".to_string(),
            reason: Some("operator requested cancellation".to_string()),
        };
        RemoteProcessCancelRequest::validate(&cancel_request).expect("valid cancellation request");
        let cancel_result = RemoteProcessCancelResult {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            process_id: "invoice-export".to_string(),
            status: RemoteProcessStatus::Cancelled,
            record: None,
        };
        RemoteProcessCancelResult::validate(&cancel_result).expect("valid cancellation result");
        assert!(cancel_result.status.is_terminal());
        assert_eq!(cancel_result.process_id, "invoice-export");

        let terminal_specs = [
            RemoteProcessTerminalSpec {
                status: RemoteProcessStatus::Completed,
                await_output: Some(RemoteProcessValueSelector::Payload),
            },
            RemoteProcessTerminalSpec {
                status: RemoteProcessStatus::Failed,
                await_output: Some(RemoteProcessValueSelector::Const(json!({
                    "code": "export_failed"
                }))),
            },
        ];
        for terminal in &terminal_specs {
            RemoteProcessTerminalSpec::validate(terminal, "RemoteProcessTerminalSpec")
                .expect("valid terminal projection");
            if let Some(selector) = &terminal.await_output {
                RemoteProcessValueSelector::validate(selector, "RemoteProcessValueSelector")
                    .expect("valid terminal value projection");
            }
        }
        assert_eq!(terminal_specs[0].status, RemoteProcessStatus::Completed);
        assert!(matches!(
            terminal_specs[0].await_output,
            Some(RemoteProcessValueSelector::Payload)
        ));

        let failure = RemoteProcessAwaitOutput::Failure {
            class: RemoteToolFailureClass::External,
            code: "export_failed".to_string(),
            message: "the batch service rejected the export".to_string(),
            raw: Some(json!({ "retryable": false })),
            control: None,
        };
        RemoteProcessAwaitOutput::validate(&failure, "RemoteProcessAwaitOutput")
            .expect("valid failure output");
        let cancelled = RemoteProcessAwaitOutput::Cancelled {
            message: "operator cancelled the export".to_string(),
            raw: Some(json!({ "completed_rows": 8 })),
            control: None,
        };
        RemoteProcessAwaitOutput::validate(&cancelled, "RemoteProcessAwaitOutput")
            .expect("valid cancellation output");
        let abandoned = RemoteProcessAwaitOutput::Abandoned {
            evidence: RemoteAbandonEvidence {
                writer: RemoteAbandonWriter::OwnerDrain,
                owner: Some(json!({ "owner_id": "worker-berlin" })),
                epoch_ms: 1_720_000_060_000,
            },
            control: None,
        };
        RemoteProcessAwaitOutput::validate(&abandoned, "RemoteProcessAwaitOutput")
            .expect("valid abandonment output");
        let no_longer_retained = RemoteProcessAwaitOutput::NoLongerRetained {
            terminal_label: "completed".to_string(),
            pruned_at_ms: 1_720_086_400_000,
        };
        RemoteProcessAwaitOutput::validate(&no_longer_retained, "RemoteProcessAwaitOutput")
            .expect("valid retention information output");
        assert_eq!(serde_json::to_value(&failure).unwrap()["class"], "external");
        assert_eq!(
            serde_json::to_value(&cancelled).unwrap()["raw"]["completed_rows"],
            8
        );
        assert_eq!(
            serde_json::to_value(&abandoned).unwrap()["evidence"]["writer"],
            "owner_drain"
        );
        assert_eq!(
            serde_json::to_value(&no_longer_retained).unwrap()["terminal_label"],
            "completed"
        );

        let terminal_semantics = RemoteProcessTerminalSemantics {
            status: RemoteProcessStatus::Failed,
            outcome: failure,
        };
        let event_semantics = RemoteProcessEventSemantics {
            terminal: Some(terminal_semantics),
            wake: None,
        };
        RemoteProcessEventSemantics::validate(&event_semantics, "RemoteProcessEventSemantics")
            .expect("valid terminal semantics");
        assert_eq!(
            serde_json::to_value(event_semantics).unwrap()["terminal"]["status"],
            "failed"
        );

        let abandon_request = RemoteAbandonRequest {
            requested_by: "operator@example.com".to_string(),
            requested_at_ms: 1_720_000_050_000,
            reason: Some("maintenance drain".to_string()),
        };
        assert_eq!(abandon_request.reason.as_deref(), Some("maintenance drain"));
        assert_eq!(abandon_request.requested_by, "operator@example.com");
        assert_eq!(abandon_request.requested_at_ms, 1_720_000_050_000);

        let input_shapes = [
            RemoteProcessInput::ToolCall {
                prepared_tool_call: json!({ "tool_name": "export", "args": {} }),
            },
            RemoteProcessInput::External {
                metadata: json!({ "source": "scheduler" }),
            },
        ];
        for input in &input_shapes {
            RemoteProcessInput::validate(input, "RemoteProcessInput")
                .expect("valid kernel process input");
        }
        assert_eq!(
            serde_json::to_value(&input_shapes[0]).unwrap()["type"],
            "tool_call"
        );
        assert_eq!(
            serde_json::to_value(&input_shapes[1]).unwrap()["metadata"]["source"],
            "scheduler"
        );
    }

    #[test]
    fn remote_process_vocabularies_serialize_stable_wire_labels() {
        let runtime_subjects = [
            RemoteRuntimeSubject::Effect {
                effect_id: "effect-9".to_string(),
                kind: RemoteRuntimeEffectKind::Process,
            },
            RemoteRuntimeSubject::Process {
                process_id: "invoice-export".to_string(),
            },
            RemoteRuntimeSubject::SessionNode {
                node_id: "node-17".to_string(),
            },
        ];
        for subject in &runtime_subjects {
            RemoteRuntimeSubject::validate(subject, "RemoteRuntimeSubject")
                .expect("valid runtime subject");
        }
        assert_eq!(
            serde_json::to_value(&runtime_subjects[0]).unwrap()["kind"],
            "process"
        );
        assert_eq!(
            serde_json::to_value(&runtime_subjects[1]).unwrap()["process_id"],
            "invoice-export"
        );

        let effect_kinds = [
            RemoteRuntimeEffectKind::LlmCall,
            RemoteRuntimeEffectKind::Direct,
            RemoteRuntimeEffectKind::ToolAttempt,
            RemoteRuntimeEffectKind::ToolBatch,
            RemoteRuntimeEffectKind::Process,
            RemoteRuntimeEffectKind::ExecCode,
            RemoteRuntimeEffectKind::Checkpoint,
            RemoteRuntimeEffectKind::SyncExecutionEnvironment,
            RemoteRuntimeEffectKind::Sleep,
            RemoteRuntimeEffectKind::AwaitEvent,
            RemoteRuntimeEffectKind::PeekAwaitEvent,
        ];
        assert!(matches!(effect_kinds[4], RemoteRuntimeEffectKind::Process));
        assert_eq!(
            serde_json::to_value(effect_kinds).expect("effect kinds serialize"),
            json!([
                "llm_call",
                "direct",
                "tool_attempt",
                "tool_batch",
                "process",
                "exec_code",
                "checkpoint",
                "sync_execution_environment",
                "sleep",
                "await_event",
                "peek_await_event"
            ])
        );

        let failure_classes = [
            RemoteToolFailureClass::InvalidRequest,
            RemoteToolFailureClass::Io,
            RemoteToolFailureClass::Unavailable,
            RemoteToolFailureClass::PermissionDenied,
            RemoteToolFailureClass::Timeout,
            RemoteToolFailureClass::Execution,
            RemoteToolFailureClass::External,
            RemoteToolFailureClass::ResourceLimit,
            RemoteToolFailureClass::Internal,
        ];
        assert_eq!(
            serde_json::to_value(failure_classes).expect("failure classes serialize"),
            json!([
                "invalid_request",
                "io",
                "unavailable",
                "permission_denied",
                "timeout",
                "execution",
                "external",
                "resource_limit",
                "internal"
            ])
        );

        let statuses = [
            RemoteProcessStatus::Running,
            RemoteProcessStatus::Waiting,
            RemoteProcessStatus::Completed,
            RemoteProcessStatus::Failed,
            RemoteProcessStatus::Cancelled,
            RemoteProcessStatus::Abandoned,
        ];
        assert_eq!(
            statuses
                .iter()
                .filter(|status| status.is_terminal())
                .count(),
            4
        );
        assert_eq!(
            serde_json::to_value(statuses).expect("process statuses serialize"),
            json!([
                "running",
                "waiting",
                "completed",
                "failed",
                "cancelled",
                "abandoned"
            ])
        );
        let filters = [
            RemoteProcessStatusFilter::Running,
            RemoteProcessStatusFilter::Waiting,
            RemoteProcessStatusFilter::Completed,
            RemoteProcessStatusFilter::Failed,
            RemoteProcessStatusFilter::Cancelled,
            RemoteProcessStatusFilter::Abandoned,
            RemoteProcessStatusFilter::Any,
        ];
        assert_eq!(
            serde_json::to_value(filters).expect("process filters serialize"),
            json!([
                "running",
                "waiting",
                "completed",
                "failed",
                "cancelled",
                "abandoned",
                "any"
            ])
        );
    }
}

#[cfg(test)]
mod asserted_tool_examples {
    use lash::remote::tools::{
        RemoteToolActivation, RemoteToolArgumentProjectionPolicy, RemoteToolGrant,
        RemoteToolOutputContract, RemoteToolRegistry, RemoteToolRetryPolicy,
        assert_remote_tool_registry_reopenable,
    };
    use lash::remote::{REMOTE_PROTOCOL_VERSION, RemoteProtocolError};
    use lash::tools::{
        DeferredToolGrant as ToolGrant, LASHLANG_TOOL_BINDING_KEY, LashlangToolBinding,
        PLUGIN_TOOL_SOURCE_ID, RemoteToolGrantLashlangExt, ToolDefinition, ToolExecutionGrant,
        ToolId,
    };

    #[derive(Clone)]
    struct ExampleRegistry(Vec<RemoteToolGrant>);

    impl RemoteToolRegistry for ExampleRegistry {
        fn grants(&self) -> Vec<RemoteToolGrant> {
            self.0.clone()
        }
    }

    fn remote_grant(name: &str, operation: &str) -> RemoteToolGrant {
        serde_json::from_value(serde_json::json!({
            "protocol_version": REMOTE_PROTOCOL_VERSION,
            "id": format!("remote-tool:{name}"),
            "name": name,
            "description": "Search the host knowledge base.",
            "input_schema": {
                "canonical": {
                    "type": "object",
                    "properties": { "query": { "type": "string" } },
                    "required": ["query"]
                }
            },
            "output_schema": {
                "canonical": {
                    "type": "object",
                    "properties": { "matches": { "type": "array" } },
                    "required": ["matches"]
                }
            },
            "output_contract": {
                "kind": "from_input_schema",
                "input_field": "result_schema",
                "default_schema": { "type": "array" }
            },
            "examples": [format!(r#"{operation}({{ query: \"release notes\" }})"#)],
            "activation": "internal",
            "argument_projection": {
                "kind": "preserve_projected_refs_in_field",
                "field": "query"
            },
            "retry_policy": {
                "type": "safe",
                "max_attempts": 3,
                "base_delay_ms": 25,
                "max_delay_ms": 250
            },
            "bindings": {}
        }))
        .expect("the host-authored grant must satisfy the remote wire schema")
    }

    #[test]
    fn remote_tool_grants_reopen_with_stable_authority_and_project_into_execution_grants() {
        let binding = LashlangToolBinding::new(["knowledge", "docs"], "search");
        let grant = remote_grant("search_docs", "search").with_lashlang_binding(binding);
        assert_eq!(grant.protocol_version, REMOTE_PROTOCOL_VERSION);
        assert_eq!(grant.id, "remote-tool:search_docs");
        assert_eq!(grant.name, "search_docs");
        assert_eq!(grant.description, "Search the host knowledge base.");
        assert_eq!(grant.input_schema.canonical["required"][0], "query");
        assert_eq!(grant.output_schema.canonical["required"][0], "matches");
        assert_eq!(grant.examples.len(), 1);
        assert_eq!(grant.activation, Some(RemoteToolActivation::Internal));
        assert_eq!(
            grant.argument_projection,
            Some(
                RemoteToolArgumentProjectionPolicy::PreserveProjectedRefsInField {
                    field: "query".to_string(),
                }
            )
        );
        assert_eq!(
            grant.retry_policy,
            Some(RemoteToolRetryPolicy::Safe {
                max_attempts: 3,
                base_delay_ms: 25,
                max_delay_ms: 250,
            })
        );
        let RemoteToolOutputContract::FromInputSchema {
            input_field,
            default_schema,
        } = &grant.output_contract
        else {
            panic!("the remote grant must preserve its dynamic output contract");
        };
        assert_eq!(input_field, "result_schema");
        assert_eq!(default_schema.as_ref().unwrap()["type"], "array");
        assert!(grant.bindings.contains_key(LASHLANG_TOOL_BINDING_KEY));
        let decoded_binding = RemoteToolGrantLashlangExt::lashlang_binding(&grant)
            .expect("the binding must decode")
            .expect("the binding must be present");
        assert_eq!(decoded_binding.module_path, ["knowledge", "docs"]);
        assert_eq!(decoded_binding.operation.as_deref(), Some("search"));
        assert_eq!(
            grant.binding_call_path(LASHLANG_TOOL_BINDING_KEY).unwrap(),
            "knowledge.docs.search"
        );
        assert_eq!(
            grant.call_path_bindings().unwrap(),
            ["knowledge.docs.search"]
        );
        grant.validate().expect("the complete grant must validate");
        RemoteToolGrant::validate_all(std::slice::from_ref(&grant))
            .expect("the registry must have unique authority");

        let registry = ExampleRegistry(vec![grant.clone()]);
        RemoteToolRegistry::validate_registry(&registry).expect("the registry must validate");
        assert_eq!(RemoteToolRegistry::grants(&registry).len(), 1);
        assert_remote_tool_registry_reopenable(&registry, &registry)
            .expect("an unchanged registry must reopen");

        let changed = ExampleRegistry(vec![
            remote_grant("search_docs", "lookup")
                .with_lashlang_binding(LashlangToolBinding::new(["knowledge", "docs"], "lookup")),
        ]);
        let mismatch = assert_remote_tool_registry_reopenable(&registry, &changed)
            .expect_err("a changed call path must fail reopen validation");
        let RemoteProtocolError::RemoteToolRegistryReopenMismatch {
            before_call_paths,
            after_call_paths,
        } = mismatch
        else {
            panic!("call-path drift must report a reopen mismatch");
        };
        assert_eq!(before_call_paths, ["knowledge.docs.search"]);
        assert_eq!(after_call_paths, ["knowledge.docs.lookup"]);

        let missing = grant
            .binding_call_path("missing.binding")
            .expect_err("missing authority must fail closed");
        let RemoteProtocolError::MissingToolBinding { tool_name, binding } = missing else {
            panic!("missing authority must identify the binding");
        };
        assert_eq!(tool_name, "search_docs");
        assert_eq!(binding, "missing.binding");

        let mut invalid = grant.clone();
        invalid.id.clear();
        let RemoteProtocolError::InvalidToolGrant { tool_name, message } = invalid
            .validate()
            .expect_err("blank tool ids must be rejected")
        else {
            panic!("invalid grants must retain host-visible details");
        };
        assert_eq!(tool_name, "search_docs");
        assert!(message.contains("id cannot be empty"));

        let definition = ToolDefinition::try_from(&grant)
            .expect("a validated remote grant must project into a local definition");
        assert_eq!(definition.id().as_str(), "remote-tool:search_docs");
        assert_eq!(definition.name(), "search_docs");
        assert_eq!(
            definition.manifest.activation,
            lash::tools::ToolActivation::Internal
        );
        assert_eq!(definition.contract.examples.len(), 1);

        let tool_id = ToolId::new("remote-tool:search_docs");
        assert_eq!(ToolId::as_str(&tool_id), definition.id().as_str());
        let tool_grant = ToolGrant::new(definition.clone())
            .with_source_id(PLUGIN_TOOL_SOURCE_ID)
            .with_execution_binding(serde_json::json!({ "tenant": "acme" }));
        assert_eq!(tool_grant.definition.name(), "search_docs");
        assert_eq!(tool_grant.source_id.as_deref(), Some(PLUGIN_TOOL_SOURCE_ID));
        assert_eq!(tool_grant.execution_binding["tenant"], "acme");

        let direct_execution_grant =
            ToolExecutionGrant::new(definition.manifest(), definition.contract());
        assert_eq!(direct_execution_grant.manifest.name, "search_docs");
        assert_eq!(direct_execution_grant.contract.examples.len(), 1);
        let execution_grant = ToolExecutionGrant::from_definition(definition)
            .with_source_id(PLUGIN_TOOL_SOURCE_ID)
            .with_execution_binding(serde_json::json!({ "tenant": "acme" }));
        assert_eq!(
            execution_grant.source_id.as_deref(),
            Some(PLUGIN_TOOL_SOURCE_ID)
        );
        assert_eq!(execution_grant.execution_binding["tenant"], "acme");

        assert_eq!(
            serde_json::to_value([RemoteToolActivation::Always, RemoteToolActivation::Internal,])
                .unwrap(),
            serde_json::json!(["always", "internal"])
        );
        assert_eq!(
            serde_json::to_value([
                RemoteToolArgumentProjectionPolicy::MaterializeProjectedValues,
                RemoteToolArgumentProjectionPolicy::PreserveProjectedRefsInField {
                    field: "content".to_string(),
                },
            ])
            .unwrap(),
            serde_json::json!([
                { "kind": "materialize_projected_values" },
                { "kind": "preserve_projected_refs_in_field", "field": "content" }
            ])
        );
        assert_eq!(
            serde_json::to_value([
                RemoteToolOutputContract::Static,
                RemoteToolOutputContract::FromInputSchema {
                    input_field: "schema".to_string(),
                    default_schema: Some(serde_json::json!({ "type": "string" })),
                },
            ])
            .unwrap(),
            serde_json::json!([
                { "kind": "static" },
                { "kind": "from_input_schema", "input_field": "schema", "default_schema": { "type": "string" } }
            ])
        );
        assert_eq!(
            serde_json::to_value([
                RemoteToolRetryPolicy::Never,
                RemoteToolRetryPolicy::Safe {
                    max_attempts: 2,
                    base_delay_ms: 10,
                    max_delay_ms: 100,
                },
                RemoteToolRetryPolicy::Idempotent {
                    max_attempts: 4,
                    base_delay_ms: 20,
                    max_delay_ms: 200,
                },
            ])
            .unwrap()[2],
            serde_json::json!({
                "type": "idempotent",
                "max_attempts": 4,
                "base_delay_ms": 20,
                "max_delay_ms": 200
            })
        );
    }

    #[test]
    fn remote_io_failure_class_has_a_stable_wire_label() {
        let class = lash::remote::processes::RemoteToolFailureClass::Io;
        assert_eq!(serde_json::to_value(class).unwrap(), "io");
    }

    #[test]
    fn explicit_turn_budget_surfaces_are_observable() {
        let remote_budget = lash::remote::processes::RemoteTurnBudget::Bounded(
            std::num::NonZeroUsize::new(8).expect("non-zero turn budget"),
        );
        let minimal_policy =
            lash::remote::processes::RemoteProcessExecutionPolicy::new(remote_budget);
        assert_eq!(minimal_policy.turn_budget, remote_budget);
        let minimal_env =
            lash::remote::processes::RemoteProcessExecutionEnvSpec::new(remote_budget);
        assert_eq!(minimal_env.policy.turn_budget, remote_budget);

        let core_budget = lash::TurnBudget::bounded(8);
        assert!(matches!(
            core_budget,
            lash::TurnBudget::Bounded(limit) if limit.get() == 8
        ));
        let persisted = lash::persistence::PersistedSessionConfig {
            provider_id: String::new(),
            model: lash::ModelSpec::default(),
            turn_budget: core_budget,
        };
        assert_eq!(persisted.turn_budget, core_budget);

        let cap_code = lash::runtime::RuntimeErrorCode::ManagedTurnConcurrencyLimitExceeded;
        assert!(cap_code.is_retryable());
        let cap_error = lash::plugins::PluginError::Runtime(lash::runtime::RuntimeError::new(
            cap_code,
            "managed-turn cap reached",
        ));
        assert!(matches!(
            cap_error,
            lash::plugins::PluginError::Runtime(ref error) if error.is_retryable()
        ));
    }
}
