use super::*;
use lash::messages::{MessageOrigin, TurnOutputSource};
use lash::persistence::SessionNodeProjection as _;

const MODEL: &str = "standard-transcript-law";

struct Echo;

fn definition() -> lash::tools::ToolDefinition {
    lash::tools::ToolDefinition::raw(
        "tool:transcript_echo",
        "transcript_echo",
        "Returns the round it was asked for.",
        json!({"type": "object"}),
        json!({"type": "object"}),
    )
    .expect("valid schemas")
    .with_execution(std::time::Duration::from_secs(120))
}

#[async_trait]
impl lash::tools::ToolProvider for Echo {
    fn tool_manifests(&self) -> Vec<lash::tools::ToolManifest> {
        vec![definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash::tools::ToolContract>> {
        (name == "transcript_echo").then(|| Arc::new(definition().contract()))
    }

    async fn execute(&self, call: lash::tools::ToolCall<'_>) -> lash::tools::ToolAttemptOutcome {
        lash::tools::ToolOutcome::ok(call.args.clone()).into()
    }
}

/// A real Standard turn, committed through send() on SQLite memory. The
/// second round mixes an invalid call with a valid call, exercising both
/// refused-tools and executed-results messages as well as reasoning/reply.
async fn two_round_turn() -> (lash::persistence::SessionReadView, TurnId) {
    use lash::direct::LlmOutputPart;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let stores = lash::sqlite::SqliteStoreSet::memory().await.expect("store");
    let backend = lash::durable::DurableBackendBuilder::new(Arc::new(stores))
        .build()
        .expect("backend");
    let next = AtomicUsize::new(0);
    let provider = lash::testing::TestProvider::builder()
        .kind(MODEL)
        .complete(move |_request| {
            let round = next.fetch_add(1, Ordering::SeqCst);
            async move {
                let mut parts = vec![LlmOutputPart::Reasoning {
                    text: format!("Reasoning for round {round}."),
                    replay: None,
                }];
                if round < 2 {
                    if round == 1 {
                        parts.push(LlmOutputPart::ToolCall {
                            call_id: "refused".into(),
                            tool_name: "transcript_echo".into(),
                            input_json: "{".into(),
                            replay: None,
                        });
                    }
                    parts.push(LlmOutputPart::ToolCall {
                        call_id: format!("round-{round}"),
                        tool_name: "transcript_echo".into(),
                        input_json: json!({"round": round}).to_string(),
                        replay: None,
                    });
                } else {
                    assert_eq!(round, 2, "exactly two tool rounds");
                    parts.push(LlmOutputPart::Text {
                        text: "Both rounds finished.".into(),
                        response_meta: None,
                    });
                }
                Ok(lash::provider::LlmResponse {
                    parts,
                    ..Default::default()
                })
            }
        })
        .build()
        .into_handle();
    let core = lash::LashCore::standard_builder(backend)
        .serve_test_llm_profile(
            provider,
            lash::LlmProfileMetadata::builder(MODEL)
                .context_window_tokens(200_000)
                .build()
                .expect("profile"),
        )
        .tools(Arc::new(Echo))
        .commit_budget(lash::CommitBudget::bounded(16 * 1024 * 1024, 4096))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .build(lash::persistence::LeaseOwnerIdentity::opaque(MODEL, "boot"))
        .expect("core");
    let session = core
        .session(lash::SessionId::from(MODEL))
        .create(lash::SessionCreation::root(lash::SessionSpec::new(
            MODEL,
            lash::TurnBudget::Unbounded,
            lash::MaxToolCalls::new(8),
        )))
        .await
        .expect("session");
    let turn_id = TurnId::from("standard-transcript-turn");
    let output = tokio::time::timeout(
        Duration::from_secs(60),
        session
            .send(lash::TurnInput::text("Run two rounds."))
            .id(turn_id.clone())
            .output(),
    )
    .await
    .expect("turn watchdog")
    .expect("turn answers");
    assert!(output.is_success(), "{output:?}");
    let view = session.read().await.expect("read").expect("committed head");
    core.shutdown().await.expect("shutdown");
    (view, turn_id)
}

/// ADR 0129: the protocol records typed provenance on every output message,
/// including reasoning, executed results, refused results and the reply.
#[tokio::test]
async fn every_standard_message_names_its_turn() {
    let (view, turn_id) = two_round_turn().await;
    let messages: Vec<_> = view
        .session_graph()
        .nodes
        .iter()
        .filter_map(|node| node.message())
        .collect();
    assert_eq!(messages.len(), 7, "input, two calls, three results, reply");
    for message in &messages[1..] {
        assert_eq!(
            message.origin,
            Some(MessageOrigin::TurnOutput {
                turn_id: turn_id.clone(),
                source: TurnOutputSource::Plugin {
                    plugin_id: "standard_protocol".into()
                },
            }),
            "the committed message {} must name its turn",
            message.id
        );
    }
}

/// ADR 0129: the shared production renderer harness preserves the prose,
/// typed provenance and order of real Standard rows after both tool rounds.
#[tokio::test]
async fn shared_harness_preserves_standard_turn_prose_and_order() {
    use std::io::Write as _;

    let (view, _) = two_round_turn().await;
    let rows = crate::ChatRow::all(view.transcript().expect("decode committed nodes").entries());
    println!(
        "FIG-5298 rows={}",
        serde_json::to_string(&rows).expect("serialize rows")
    );
    let fake_dom = include_str!("../../../tests/fake_dom.mjs");
    let harness = include_str!("../../../tests/transcript_projection_harness.mjs");
    let script = format!(
        "{}\n{}\n{}",
        fake_dom.replace("export ", ""),
        harness
            .replace("import { createFakeDocument } from './fake_dom.mjs';", "")
            .replace("export ", ""),
        "const input = JSON.parse((await import('node:fs')).readFileSync(0, 'utf8'));\n\
         verifyTranscriptSurface('workbench', input.asset, input.rows);\n\
         console.log('shared ADR 0129 harness passed');"
    );
    let node = std::env::var_os("LASH_WORKBENCH_TEST_NODE").unwrap_or_else(|| "node".into());
    let mut child = std::process::Command::new(node)
        .args(["--input-type=module", "-e", &script])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("start shared harness");
    let input = json!({"asset": include_str!("../../../assets/timeline.js"), "rows": rows});
    child
        .stdin
        .take()
        .expect("harness stdin")
        .write_all(
            serde_json::to_string(&input)
                .expect("serialize canonical rows")
                .as_bytes(),
        )
        .expect("send canonical rows");
    let output = child.wait_with_output().expect("shared harness finishes");
    assert!(
        output.status.success(),
        "shared harness: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
