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
                ..Default::default()
            },
        }),
        originator: RemoteProcessOriginator::Host { scope: None },
        wake_target: None,
        grant: None,
        event_types: Vec::new(),
    };

    request.validate()?;
    // docs:end:remote-process-start-request
    Ok(())
}
