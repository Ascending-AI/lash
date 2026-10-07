// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

//! FIG-4681: every committed fuzz seed for the remote DTO and plugin message
//! targets decodes under the current wire shapes, and this gate goes red when
//! a seed class stops decoding.
//!
//! cargo-fuzz reads `fuzz/corpus/<target>`; those directories are symlinks
//! into `testdata/fuzz-corpus/<target>` here, so the committed seeds live
//! inside this package and reach the Buck2 test sandbox as ordinary package
//! resources. A seed's filename is `<class>-<slug>`: the class names the
//! decoder the fuzz target runs the seed through. Regenerate after an
//! intentional wire-shape change with the real encoders:
//!
//! ```sh
//! . ./env.sh
//! kiln test //crates/lash-remote-protocol:lash-remote-protocol__unit_test \
//!   --local-test-execution --no-test-cache \
//!   --test_env LASH_REGENERATE=1 \
//!   --test_env "BUILD_WORKSPACE_DIRECTORY=$PWD" \
//!   --test_arg=regenerate_committed_fuzz_corpus --test_arg=--ignored
//! ```

use super::*;
use lash_sansio::{SessionId, ToolCallId, TurnId};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

const REGENERATE_ENV: &str = "LASH_REGENERATE";

/// The decoder one seed class runs: the same calls the matching fuzz target
/// makes on every input.
type Decode = fn(&[u8]) -> Result<(), String>;

struct CorpusClass {
    /// Seed filenames are `<prefix>-<slug>`; the prefix selects the decoder.
    prefix: &'static str,
    decode: Decode,
}

struct CorpusTarget {
    /// The cargo-fuzz target name and its corpus directory under
    /// `testdata/fuzz-corpus/` (reached from `fuzz/corpus/` by symlink).
    dir: &'static str,
    classes: &'static [CorpusClass],
}

const CORPUS_TARGETS: &[CorpusTarget] = &[
    CorpusTarget {
        dir: "remote_wire_dto",
        classes: &[
            CorpusClass {
                prefix: "turn_request",
                decode: |bytes| {
                    RemoteTurnRequest::decode_json(bytes)
                        .map(|_| ())
                        .map_err(|error| error.to_string())
                },
            },
            CorpusClass {
                prefix: "turn_input",
                decode: |bytes| {
                    RemoteTurnInput::decode_json(bytes)
                        .map(|_| ())
                        .map_err(|error| error.to_string())
                },
            },
            CorpusClass {
                prefix: "turn_report",
                decode: |bytes| {
                    RemoteTurnReport::decode_json(bytes)
                        .map(|_| ())
                        .map_err(|error| error.to_string())
                },
            },
        ],
    },
    CorpusTarget {
        dir: "plugin_payload",
        classes: &[CorpusClass {
            prefix: "tool_grants",
            decode: |bytes| {
                let grants = serde_json::from_slice::<Vec<RemoteToolGrant>>(bytes)
                    .map_err(|error| error.to_string())?;
                RemoteToolGrant::validate_all(&grants).map_err(|error| error.to_string())?;
                for grant in &grants {
                    grant
                        .call_path_bindings()
                        .map_err(|error| error.to_string())?;
                }
                Ok(())
            },
        }],
    },
];

/// The crate's source directory in a Buck2 test workspace or Cargo checkout.
fn crate_dir() -> PathBuf {
    std::env::var_os("BUILD_WORKSPACE_DIRECTORY").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")),
        |root| PathBuf::from(root).join("crates/lash-remote-protocol"),
    )
}

fn corpus_dir(target: &str) -> PathBuf {
    crate_dir().join("testdata/fuzz-corpus").join(target)
}

fn class_for_name<'a>(target: &'a CorpusTarget, name: &str) -> Option<&'a CorpusClass> {
    target.classes.iter().find(|class| {
        name.strip_prefix(class.prefix)
            .is_some_and(|rest| rest.starts_with('-'))
    })
}

#[test]
fn committed_fuzz_seeds_decode_under_current_wire_shapes() {
    let mut problems: Vec<String> = Vec::new();
    for target in CORPUS_TARGETS {
        let dir = corpus_dir(target.dir);
        let mut seeds: Vec<(String, PathBuf)> = match std::fs::read_dir(&dir) {
            Ok(entries) => entries
                .map(|entry| {
                    let entry = entry.expect("read corpus entry");
                    (
                        entry.file_name().into_string().expect("UTF-8 seed name"),
                        entry.path(),
                    )
                })
                .collect(),
            Err(error) => {
                problems.push(format!(
                    "{}: cannot read {}: {error}",
                    target.dir,
                    dir.display()
                ));
                continue;
            }
        };
        seeds.sort();
        let mut counts: BTreeMap<&'static str, usize> = target
            .classes
            .iter()
            .map(|class| (class.prefix, 0))
            .collect();
        for (name, path) in seeds {
            if !path.is_file() {
                problems.push(format!(
                    "{}/{name}: corpus entries must be files",
                    target.dir
                ));
                continue;
            }
            let Some(class) = class_for_name(target, &name) else {
                problems.push(format!(
                    "{}/{name}: not a `<class>-<slug>` canonical seed; regenerate with \
                     {REGENERATE_ENV}=1",
                    target.dir
                ));
                continue;
            };
            let bytes = std::fs::read(&path).expect("read seed");
            match (class.decode)(&bytes) {
                Ok(()) => *counts.get_mut(class.prefix).expect("class") += 1,
                Err(error) => problems.push(format!("{}/{name}: {error}", target.dir)),
            }
        }
        for (prefix, count) in counts {
            if count == 0 {
                problems.push(format!("{}: no committed {prefix}-* seed", target.dir));
            }
        }
    }
    assert!(
        problems.is_empty(),
        "fuzz seed corpus decode gate failed; regenerate the committed seeds with \
         {REGENERATE_ENV}=1 (see fuzz_corpus_tests.rs):\n{}",
        problems.join("\n")
    );
}

/// Rewrites the committed corpus with the seeds the current encoders emit.
/// Committed seeds are canonical by definition: regeneration replaces the
/// whole directory contents, so a fuzzer-discovered input worth keeping is
/// re-encoded through the encoders and committed under its class name.
#[test]
#[ignore = "regenerates crates/lash-remote-protocol/testdata/fuzz-corpus"]
fn regenerate_committed_fuzz_corpus() {
    assert_eq!(
        std::env::var(REGENERATE_ENV).as_deref(),
        Ok("1"),
        "set {REGENERATE_ENV}=1 to acknowledge rewriting the committed fuzz corpus"
    );
    for target in CORPUS_TARGETS {
        let dir = corpus_dir(target.dir);
        std::fs::create_dir_all(&dir).expect("create corpus directory");
        for entry in std::fs::read_dir(&dir).expect("read corpus directory") {
            std::fs::remove_file(entry.expect("corpus entry").path()).expect("remove stale seed");
        }
    }
    for (dir, name, bytes) in canonical_seeds() {
        std::fs::write(corpus_dir(dir).join(&name), bytes).expect("write seed");
    }
}

/// Every committed seed, encoded by the same encoders a peer runs.
fn canonical_seeds() -> Vec<(&'static str, String, Vec<u8>)> {
    let negotiated = crate::negotiation::test_negotiated();
    let mut seeds: Vec<(&'static str, String, Vec<u8>)> = Vec::new();
    let mut push_envelope = |name: &str, result: Result<Vec<u8>, serde_json::Error>| {
        seeds.push((
            "remote_wire_dto",
            name.to_string(),
            result.expect("encode envelope seed"),
        ));
    };
    push_envelope(
        "turn_request-minimal",
        seed_turn_request_minimal().encode_json(&negotiated),
    );
    push_envelope(
        "turn_request-options-grants-attachments",
        seed_turn_request_full().encode_json(&negotiated),
    );
    push_envelope(
        "turn_input-text",
        seed_turn_input_text().encode_json(&negotiated),
    );
    push_envelope(
        "turn_input-attachments-trace",
        seed_turn_input_attachments().encode_json(&negotiated),
    );
    push_envelope(
        "turn_report-answered",
        seed_turn_report_answered().encode_json(&negotiated),
    );
    push_envelope(
        "turn_report-cancelled",
        seed_turn_report_cancelled().encode_json(&negotiated),
    );
    push_envelope(
        "turn_report-frame-switch",
        seed_turn_report_frame_switch().encode_json(&negotiated),
    );
    push_envelope(
        "turn_report-full",
        seed_turn_report_full().encode_json(&negotiated),
    );
    seeds.extend(plugin_payload_seeds());
    seeds
}

fn seed_attachment_ref(id: &str) -> RemoteAttachmentRef {
    RemoteAttachmentRef {
        id: id.to_string(),
        media_type: "image/png".to_string(),
        byte_len: 4,
        type_metadata: None,
        label: None,
    }
}

fn seed_tool_grant(name: &str, module: &str, operation: &str) -> RemoteToolGrant {
    RemoteToolGrant {
        id: format!("remote-tool:{name}"),
        name: name.to_string(),
        description: "seed grant".to_string(),
        input_schema: crate::llm::default_remote_input_schema(),
        output_schema: RemoteSchemaContract::default(),
        output_contract: RemoteToolOutputContract::Static,
        examples: Vec::new(),
        argument_projection: None,
        execution_policy: None,
        bindings: BTreeMap::from([(
            "call".to_string(),
            serde_json::json!({
                "module_path": [module],
                "operation": operation,
            }),
        )]),
    }
}

fn seed_turn_request_minimal() -> RemoteTurnRequest {
    RemoteTurnRequest {
        session_id: SessionId::from("session-seed"),
        turn_id: TurnId::from("turn-seed"),
        input: RemoteTurnInput::text("seed turn input"),
        protocol_turn_options: None,
        tool_grants: Vec::new(),
        metadata: HashMap::new(),
    }
}

fn seed_turn_request_full() -> RemoteTurnRequest {
    RemoteTurnRequest {
        session_id: SessionId::from("session-seed"),
        turn_id: TurnId::from("turn-seed"),
        input: seed_turn_input_attachments(),
        protocol_turn_options: Some(RemoteProtocolTurnOptions {
            payload: serde_json::json!({ "answer": "raw" }),
        }),
        tool_grants: vec![seed_tool_grant("search", "tools", "search")],
        metadata: HashMap::from([("origin".to_string(), serde_json::json!("fuzz-corpus"))]),
    }
}

fn seed_turn_input_text() -> RemoteTurnInput {
    RemoteTurnInput::text("seed text")
}

fn seed_turn_input_attachments() -> RemoteTurnInput {
    let mut input = RemoteTurnInput::text("seed text beside attachments");
    input.trace_turn_id = Some(TurnId::from("trace-seed"));
    input.items.push(RemoteInputItem::Attachment {
        source: RemoteAttachmentSource::Inline {
            media_type: "image/png".to_string(),
            data_base64: "AQIDBA==".to_string(),
        },
    });
    input.items.push(RemoteInputItem::Attachment {
        source: RemoteAttachmentSource::Stored {
            attachment_ref: seed_attachment_ref("attachment-seed"),
        },
    });
    input.items.push(RemoteInputItem::Attachment {
        source: RemoteAttachmentSource::ProviderFile {
            provider_scope: RemoteProviderFileScope {
                provider: "provider-seed".to_string(),
                credential_scope: "credential-seed".to_string(),
            },
            id: "provider-file-seed".to_string(),
            media_type: Some("image/png".to_string()),
        },
    });
    input
}

fn seed_llm_call_record() -> RemoteLlmCallRecord {
    RemoteLlmCallRecord {
        call_id: "llm-call-seed".to_string(),
        label: Some("seed".to_string()),
        replay_drops: Vec::new(),
        attempts: vec![RemoteAttemptRecord {
            ordinal: 1,
            outcome: RemoteAttemptOutcome::Completed,
            protocol_position: RemoteProtocolPosition::TerminalObserved,
            retry_budget_consumed: false,
            retry_decision: None,
            error: None,
            evidence: None,
            generation_disposition: None,
            usage: Some(RemoteUsage {
                input_tokens: 3,
                output_tokens: 5,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 0,
            }),
            usage_disposition: RemoteAttemptUsageOutcome::Reported,
        }],
    }
}

fn seed_turn_report() -> RemoteTurnReport {
    RemoteTurnReport {
        session_id: SessionId::from("session-seed"),
        turn_id: TurnId::from("turn-seed"),
        outcome: RemoteTurnOutcome::Finished {
            finish: RemoteTurnFinish::AssistantMessage {
                text: "done".to_string(),
            },
        },
        assistant_output: RemoteAssistantOutput::default(),
        usage: RemoteTurnUsageReport::default(),
        execution: RemoteTurnExecutionMetrics::default(),
        tool_calls: Vec::new(),
        llm_calls: Vec::new(),
        issues: Vec::new(),
        activities: Vec::new(),
        metadata: HashMap::new(),
    }
}

fn seed_turn_report_answered() -> RemoteTurnReport {
    let mut report = seed_turn_report();
    report.assistant_output = RemoteAssistantOutput {
        safe_text: "done".to_string(),
        raw_text: "done".to_string(),
        state: RemoteAssistantOutputState::Usable,
    };
    report.activities = vec![
        RemoteTurnActivity {
            sequence: 1,
            id: "activity-start".to_string(),
            correlation_id: "corr-seed".to_string(),
            event: RemoteTurnEvent::TurnStarted {
                turn_id: TurnId::from("turn-seed"),
            },
        },
        RemoteTurnActivity {
            sequence: 2,
            id: "activity-delta".to_string(),
            correlation_id: "corr-seed".to_string(),
            event: RemoteTurnEvent::AssistantProseDelta {
                text: "done".to_string(),
                block: lash_sansio::llm::types::StreamBlockIdentity::new("text:0", 0),
            },
        },
    ];
    report
}

fn seed_turn_report_cancelled() -> RemoteTurnReport {
    let mut report = seed_turn_report();
    report.outcome = RemoteTurnOutcome::Stopped {
        stop: RemoteTurnStop::Cancelled {
            evidence: RemoteTurnCancellationEvidence {
                request_id: "cancel-seed".to_string(),
                origin: Some("host-seed".to_string()),
                reason: Some("operator stop".to_string()),
                undelivered: RemoteTurnCancelUndeliveredInputPolicy::Defer,
                mode: RemoteTurnCancelMode::AfterStep,
                honoured_after_step: Some(2),
            },
        },
    };
    report
}

fn seed_turn_report_frame_switch() -> RemoteTurnReport {
    let mut report = seed_turn_report();
    report.outcome = RemoteTurnOutcome::AgentFrameSwitch {
        frame_key: "frame-seed".to_string(),
        task: "seed task".to_string(),
    };
    report
}

fn seed_turn_report_full() -> RemoteTurnReport {
    let call_id = ToolCallId::fixture("call-seed");
    let record = seed_llm_call_record();
    let mut report = seed_turn_report();
    report.tool_calls = vec![RemoteToolCallRecord {
        call_id: call_id.clone(),
        provider_call_id: Some("provider-call-seed".to_string()),
        tool_name: "search".to_string(),
        args: serde_json::json!({"query": "seed"}),
        output: RemoteToolCallOutput {
            outcome: RemoteToolCallOutcome::Success(serde_json::json!({"ok": true})),
            control: None,
            view: None,
            projection_value: None,
        },
    }];
    report.llm_calls = vec![record.clone()];
    report.issues = vec![RemoteTurnIssue {
        severity: RemoteTurnIssueSeverity::Advisory,
        kind: RemoteTurnFailureKind::LlmProvider,
        code: None,
        terminal_reason: None,
        message: "seed issue".to_string(),
        raw: None,
        retryable: Some(false),
        provider_failure_kind: Some(RemoteProviderFailureKind::Timeout),
        plugin_failures: Vec::new(),
    }];
    report.activities = vec![
        RemoteTurnActivity {
            sequence: 1,
            id: "activity-tool".to_string(),
            correlation_id: "corr-seed".to_string(),
            event: RemoteTurnEvent::ToolCallStarted {
                call_id: call_id.clone(),
                provider_call_id: Some("provider-call-seed".to_string()),
                name: "search".to_string(),
                args: serde_json::json!({"query": "seed"}),
                graph_key: None,
            },
        },
        RemoteTurnActivity {
            sequence: 2,
            id: "activity-llm".to_string(),
            correlation_id: "corr-seed".to_string(),
            event: RemoteTurnEvent::ModelCallRecorded { record },
        },
    ];
    report.metadata = HashMap::from([("origin".to_string(), serde_json::json!("fuzz-corpus"))]);
    report
}

fn plugin_payload_seeds() -> Vec<(&'static str, String, Vec<u8>)> {
    vec![
        (
            "plugin_payload",
            "tool_grants-minimal".to_string(),
            serde_json::to_vec(&vec![RemoteToolGrant {
                id: "remote-tool:echo".to_string(),
                name: "echo".to_string(),
                description: String::new(),
                input_schema: crate::llm::default_remote_input_schema(),
                output_schema: RemoteSchemaContract::default(),
                output_contract: RemoteToolOutputContract::Static,
                examples: Vec::new(),
                argument_projection: None,
                execution_policy: None,
                bindings: BTreeMap::new(),
            }])
            .expect("encode tool grants"),
        ),
        (
            "plugin_payload",
            "tool_grants-bound-retry".to_string(),
            serde_json::to_vec(&vec![
                RemoteToolGrant {
                    argument_projection: Some(
                        RemoteToolArgumentProjectionPolicy::PreserveProjectedRefsInField {
                            field: "args".to_string(),
                        },
                    ),
                    execution_policy: Some(RemoteExecutionPolicy::repeatable(
                        std::num::NonZeroU32::new(3).expect("nonzero attempt bound"),
                        50,
                        1_000,
                    )),
                    output_contract: RemoteToolOutputContract::FromInputSchema {
                        input_field: "schema".to_string(),
                        default_schema: None,
                    },
                    examples: vec!["search rust".to_string()],
                    ..seed_tool_grant("search", "tools.search", "run")
                },
                seed_tool_grant("read", "tools.fs", "read"),
            ])
            .expect("encode tool grants"),
        ),
    ]
}
