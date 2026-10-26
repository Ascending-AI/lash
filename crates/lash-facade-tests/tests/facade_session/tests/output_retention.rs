//! Oversized output is retained before it enters history (FIG-1643): real
//! turns on the core's own node, over SQLite and over PostgreSQL.
//!
//! A standard turn calls three tools as parallel children — one returns far
//! more than the renderer shows, one fails with a message as long, and one
//! returns a line a plugin's presentation step then extends past the byte
//! policy. An RLM turn prints a large value and finishes with it. After each
//! commit the laws read the committed history back from the store:
//!
//! * **resident size** — the committed frame is bounded, whatever the
//!   outputs weighed;
//! * **exact retention** — every retained block's reference resolves, for a
//!   host, to the complete bytes it stands for;
//! * **rooted** — an attachment sweep with no grace keeps every retained
//!   attachment the commit names.

use super::*;
use lash_core::llm::types::LlmResponse;

/// A policy far below the outputs: 4 KiB inline, a 512-byte witness.
const POLICY: lash_core::OutputRetentionPolicy = lash_core::OutputRetentionPolicy {
    shape: lash_core::ValueShapeLimits::standard(),
    inline_limit_bytes: 4 * 1024,
    witness_bytes: 512,
};

/// The committed frame's ceiling: two renderer cuts (16,000 characters each),
/// one witness, and the frame's own structure. The outputs weigh ~800 KB.
const RESIDENT_CEILING_BYTES: usize = 64 * 1024;

fn bulk_text() -> String {
    (0..8_000)
        .map(|line| format!("bulk output line {line:05}\n"))
        .collect()
}

fn failure_text() -> String {
    "a frame of the failed call's stack\n".repeat(8_000)
}

fn appendix_text() -> String {
    "a plugin appendix line\n".repeat(10_000)
}

struct RetentionTools;

fn retention_tool(name: &str) -> lash_vocabulary::tool_contract::ToolDefinition {
    lash_vocabulary::tool_contract::ToolDefinition::raw(
        format!("tool:{name}"),
        format!("tools.{name}"),
        "A tool whose output is larger than history keeps.",
        serde_json::json!({"type": "object", "properties": {}, "additionalProperties": false}),
        serde_json::json!({"type":"string"}),
    )
    .expect("valid declared tool schemas")
    .with_execution(std::time::Duration::from_secs(120))
}

const TOOLS: [&str; 3] = ["bulk_ok", "bulk_fail", "small_ok"];

#[async_trait]
impl ToolProvider for RetentionTools {
    fn tool_manifests(
        &self,
        _config: &lash_core::OwnerConfigView,
    ) -> std::result::Result<
        Vec<lash_vocabulary::tool_contract::ToolManifest>,
        lash::plugins::PluginError,
    > {
        Ok({
            TOOLS
                .iter()
                .map(|name| retention_tool(name).manifest())
                .collect()
        })
    }

    fn resolve_contract(
        &self,
        name: &str,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_vocabulary::tool_contract::ToolContract>> {
        let Some(("tools", name)) = name.split_once('.') else {
            return None;
        };
        TOOLS
            .contains(&name)
            .then(|| Arc::new(retention_tool(name).contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        match call.name().as_str() {
            "tools.bulk_ok" => lash_core::ToolOutcome::ok(serde_json::json!(bulk_text())).into(),
            "tools.bulk_fail" => {
                lash_core::ToolOutcome::failure(lash_core::ToolFailure::io("dump", failure_text()))
                    .into()
            }
            _ => lash_core::ToolOutcome::ok(serde_json::json!("small")).into(),
        }
    }
}

/// A presentation step that extends `small_ok`'s return past the policy:
/// an addition after the renderer, which only the boundary's own retention
/// bounds.
fn appendix_step() -> lash_core::plugin::ToolPresentationStep {
    Arc::new(|input: lash_core::plugin::ToolPresentationInput| {
        let mut next = input.previous;
        if input.context.tool_name == "tools.small_ok" {
            next.parts
                .push(lash_core::facade_support::ModelToolReturnPart::text(
                    appendix_text(),
                ));
        }
        Box::pin(async move { Ok::<_, lash_execution_contracts::plugin_error::PluginError>(next) })
    })
}

fn tool_calling_provider() -> ProviderHandle {
    let calls = Arc::new(AtomicUsize::new(0));
    lash::testing::TestProvider::builder()
        .kind("output-retention")
        .complete(move |_request| {
            let call = calls.fetch_add(1, Ordering::SeqCst);
            async move {
                let parts = if call == 0 {
                    TOOLS
                        .iter()
                        .map(|name| LlmOutputPart::ToolCall {
                            call_id: format!("call-{name}"),
                            tool_name: format!("tools__{name}"),
                            input_json: "{}".to_string(),
                            replay: None,
                        })
                        .collect()
                } else {
                    vec![LlmOutputPart::Text {
                        text: "done".to_string(),
                        response_meta: None,
                    }]
                };
                Ok(LlmResponse {
                    parts,
                    response_metadata: Default::default(),
                    ..LlmResponse::default()
                })
            }
        })
        .build()
        .into_handle()
}

/// The committed current frame of `session_id`, serialized, and the frame.
async fn committed_frame(
    backend: &lash_execution_contracts::backend::Backend,
    session_id: &str,
) -> (usize, lash_core::store::SessionWindowRead) {
    let window = lash_core::SessionHistoryStore::load_session_window(
        backend.session_store_factory().as_ref(),
        &SessionId::fixture(session_id),
        lash_core::store::WindowSelector::Current,
    )
    .await
    .expect("read the committed frame")
    .expect("the session has a committed head");
    let bytes = serde_json::to_vec(&window.window)
        .expect("the frame serializes")
        .len();
    (bytes, window)
}

/// Sweeps the deployment's attachments with no grace: a retained attachment
/// the commit names survives it.
async fn sweep_without_grace(backend: &lash_execution_contracts::backend::Backend) {
    lash_core::facade_support::reclaim_unreferenced_attachments(
        backend.session_store_factory().as_ref(),
        backend.attachment_store().as_ref(),
        lash_core::AttachmentReclamationPolicy::new(0, lash_core::EmptyRootSetPolicy::Refuse),
    )
    .await
    .expect("sweep the attachments");
}

async fn stored_text(
    backend: &lash_execution_contracts::backend::Backend,
    reference: &lash_core::AttachmentRef,
) -> String {
    let stored = backend
        .attachment_store()
        .get(&reference.id, 32 * 1024 * 1024)
        .await
        .expect("the retained attachment survives the sweep");
    assert_eq!(stored.bytes.len() as u64, reference.byte_len);
    String::from_utf8(stored.bytes).expect("retained text")
}

async fn oversized_tool_output_is_retained_before_it_enters_history(
    backend: lash_execution_contracts::backend::Backend,
) -> Result<()> {
    const SESSION: &str = "standard-output-retention";
    let core = explicit_ephemeral_facets(LashCore::standard_builder(backend.clone()))
        .data_retention(lash::DataRetention {
            attachments: lash::persistence::AttachmentPolicy {
                output_retention: POLICY,
                ..lash::persistence::AttachmentPolicy::standard()
            },
            ..lash::DataRetention::standard()
        })
        .serve_test_llm_profile(tool_calling_provider(), mock_llm_profile_spec())
        .tools(lash::tool_provider_key!("tools"), Arc::new(RetentionTools))
        .plugin(Arc::new(StaticPluginFactory::new(
            lash_core::plugin::PluginDeclaration::initial("output-retention-appendix"),
            lash_core::plugin::PluginSpec::new()
                .with_presentation_step(lash::hook_key!("presentation-step-1"), appendix_step()),
        )))
        .build(lash_facade_tests::runtime_lease_owner())?;
    let session = created_session(
        &core,
        lash_vocabulary::identity::SessionId::parse(SESSION).expect("nonblank host identity"),
    )
    .await
    .open()
    .await?;
    let output = session
        .send(TurnInput::text("call the tools"))
        .output()
        .await?;
    assert_eq!(output.assistant_message(), Some("done"));

    let (resident, window) = committed_frame(&backend, SESSION).await;
    assert!(
        resident <= RESIDENT_CEILING_BYTES,
        "the committed frame holds {resident} bytes; history must stay bounded"
    );
    let retained: std::collections::BTreeMap<String, lash_core::RetainedOutput> = window
        .window
        .read_model()
        .messages
        .iter()
        .flat_map(|message| message.parts.iter())
        .filter_map(|part| {
            let tool = part.tool_name()?.to_string();
            let retained = part.retained_outputs().next()?.clone();
            Some((tool, retained))
        })
        .collect();
    assert_eq!(
        retained.keys().map(String::as_str).collect::<Vec<_>>(),
        vec!["tools__bulk_fail", "tools__bulk_ok", "tools__small_ok"],
        "every oversized result is retained: {retained:#?}"
    );

    sweep_without_grace(&backend).await;
    assert_eq!(
        stored_text(&backend, &retained["tools__bulk_ok"].reference).await,
        bulk_text()
    );
    assert_eq!(
        stored_text(&backend, &retained["tools__bulk_fail"].reference).await,
        failure_text()
    );
    let appended = stored_text(&backend, &retained["tools__small_ok"].reference).await;
    assert!(appended.contains("small"), "{}", &appended[..64]);
    assert!(appended.ends_with(&appendix_text()));
    assert!(
        retained["tools__small_ok"].witness.len() <= 512,
        "the plugin addition's witness is bounded by the policy"
    );
    Ok(())
}

#[cfg(feature = "rlm")]
async fn oversized_rlm_print_and_final_value_are_retained_before_they_enter_history(
    backend: lash_execution_contracts::backend::Backend,
) -> Result<()> {
    const SESSION: &str = "rlm-output-retention";
    let core = explicit_ephemeral_facets(rlm_core_builder_over(backend.clone()))
        .data_retention(lash::DataRetention {
            attachments: lash::persistence::AttachmentPolicy {
                output_retention: POLICY,
                ..lash::persistence::AttachmentPolicy::standard()
            },
            ..lash::DataRetention::standard()
        })
        .serve_test_llm_profile(
            queued_text_provider(vec![typescript_block(
                r#"
const rows = [];
for (let i = 0; i < 3000; i++) {
  rows.push({ index: i, text: "a row the cell prints and finishes with" });
}
console.log(rows);
await control.finish({ rows });"#,
            )]),
            mock_llm_profile_spec(),
        )
        .build(lash_facade_tests::runtime_lease_owner())?;
    let session = created_session(
        &core,
        lash_vocabulary::identity::SessionId::parse(SESSION).expect("nonblank host identity"),
    )
    .await
    .open()
    .await?;
    let output = session
        .send(TurnInput::text("print the rows"))
        .output()
        .await?;
    // The host is answered with the whole value: retention bounds history,
    // not the turn's answer.
    let value = output
        .finished()
        .map(|(_, value)| value)
        .cloned()
        .expect("the turn finishes with a value");
    assert_eq!(value["rows"].as_array().map(Vec::len), Some(3000));

    let (resident, window) = committed_frame(&backend, SESSION).await;
    assert!(
        resident <= RESIDENT_CEILING_BYTES,
        "the committed frame holds {resident} bytes; history must stay bounded"
    );
    let frame = serde_json::to_value(&window.window).expect("the frame serializes");
    let mut prints = Vec::new();
    let mut finals = Vec::new();
    collect_retained(&frame, &mut prints, &mut finals);
    let ([print], [finished]) = (prints.as_slice(), finals.as_slice()) else {
        panic!("one retained print and one retained final value: {prints:#?} {finals:#?}");
    };

    for retained in [print, finished] {
        let edges = backend
            .session_store_factory()
            .attachment_referrers(&retained.output.reference.id)
            .await
            .expect("read the retained output's referrers");
        assert!(
            edges.contains(&lash_core::ArtifactReferrer::Session(
                session.session_id().clone()
            )),
            "the commit holds each retained code output on the session: {edges:?}"
        );
    }
    sweep_without_grace(&backend).await;
    assert_eq!(
        stored_text(&backend, &finished.output.reference).await,
        serde_json::to_string(&value).expect("encode the final value")
    );
    let archived: Vec<serde_json::Value> =
        serde_json::from_str(&stored_text(&backend, &print.output.reference).await)
            .expect("the retained archive is the print values");
    // The TypeScript console prints structured data as compact JSON text;
    // retaining that observation must keep every row of the original value.
    let printed: serde_json::Value =
        serde_json::from_str(archived[0].as_str().expect("console text"))
            .expect("the whole printed value");
    assert_eq!(printed, value["rows"]);
    for retained in [print, finished] {
        assert!(retained.output.witness.len() <= 512);
    }
    Ok(())
}

/// Every cell's retained prints (`prints_retained`) and retained finish value
/// (a `finished` result's `retained`) anywhere in `value`, the committed frame.
#[cfg(feature = "rlm")]
fn collect_retained(
    value: &serde_json::Value,
    prints: &mut Vec<lash_core::RetainedValue>,
    finals: &mut Vec<lash_core::RetainedValue>,
) {
    match value {
        serde_json::Value::Object(entries) => {
            for (key, entry) in entries {
                let decoded = |retained: &serde_json::Value| {
                    serde_json::from_value::<lash_core::RetainedValue>(retained.clone())
                        .expect("a retained value decodes")
                };
                match key.as_str() {
                    "prints_retained" if !entry.is_null() => prints.push(decoded(entry)),
                    // A cell `control.finish` ended holds the call's value.
                    "result"
                        if entry["kind"] == "controlled"
                            && entry["value"]["control"]["type"] == "finish"
                            && entry["value"]["control"]["value"]["retained"].is_object() =>
                    {
                        finals.push(decoded(&entry["value"]["control"]["value"]["retained"]));
                    }
                    _ => collect_retained(entry, prints, finals),
                }
            }
        }
        serde_json::Value::Array(entries) => {
            for entry in entries {
                collect_retained(entry, prints, finals);
            }
        }
        _ => {}
    }
}

/// A backend over a fresh PostgreSQL store set. An explicitly selected
/// PostgreSQL test requires the service URL.
async fn postgres_backend() -> (
    lash_execution_contracts::backend::Backend,
    Box<dyn std::any::Any + Send>,
) {
    let (stores, held) = postgres_store_set().await;
    (lash_conformance::backend_over(stores), held)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oversized_tool_output_is_retained_before_it_enters_history_on_sqlite() -> Result<()> {
    oversized_tool_output_is_retained_before_it_enters_history(sqlite_memory_store_backend().await)
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
async fn oversized_tool_output_is_retained_before_it_enters_history_on_postgres() -> Result<()> {
    let (backend, _held) = postgres_backend().await;
    oversized_tool_output_is_retained_before_it_enters_history(backend).await
}

#[cfg(feature = "rlm")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oversized_rlm_print_and_final_value_are_retained_before_they_enter_history_on_sqlite()
-> Result<()> {
    oversized_rlm_print_and_final_value_are_retained_before_they_enter_history(
        sqlite_memory_store_backend().await,
    )
    .await
}

#[cfg(feature = "rlm")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
async fn oversized_rlm_print_and_final_value_are_retained_before_they_enter_history_on_postgres()
-> Result<()> {
    let (backend, _held) = postgres_backend().await;
    oversized_rlm_print_and_final_value_are_retained_before_they_enter_history(backend).await
}

#[cfg(feature = "rlm")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn many_subcap_prints_in_one_step_land_one_bounded_archive_on_sqlite() -> Result<()> {
    let backend = sqlite_memory_store_backend().await;
    let core = explicit_ephemeral_facets(rlm_core_builder_over(backend.clone()))
        .data_retention(lash::DataRetention {
            attachments: lash::persistence::AttachmentPolicy {
                output_retention: POLICY,
                ..lash::persistence::AttachmentPolicy::standard()
            },
            ..lash::DataRetention::standard()
        })
        .serve_test_llm_profile(
            queued_text_provider(vec![
                typescript_block(
                    r#"
for (let i = 0; i < 200; i++) {
  console.log({ index: i, text: "small printed value é🙂" });
}
"#,
                ),
                typescript_block("await control.finish(200);"),
            ]),
            mock_llm_profile_spec(),
        )
        .build(lash_facade_tests::runtime_lease_owner())?;
    let session = created_session(
        &core,
        lash_vocabulary::identity::SessionId::parse("aggregate-prints")
            .expect("nonblank host identity"),
    )
    .await
    .open()
    .await?;
    session
        .send(TurnInput::text("print the batch"))
        .output()
        .await?;
    let (resident, window) = committed_frame(&backend, "aggregate-prints").await;
    let frame = serde_json::to_value(&window.window).expect("frame");
    fn archives(value: &serde_json::Value, found: &mut Vec<serde_json::Value>) {
        match value {
            serde_json::Value::Object(fields) => {
                for (key, value) in fields {
                    if key == "prints_retained" && !value.is_null() {
                        found.push(value.clone());
                    } else {
                        archives(value, found);
                    }
                }
            }
            serde_json::Value::Array(values) => {
                for value in values {
                    archives(value, found);
                }
            }
            _ => {}
        }
    }
    let mut found = Vec::new();
    archives(&frame, &mut found);
    assert_eq!(
        found.len(),
        1,
        "one aggregate archive, even though every print is below the cap"
    );
    assert!(
        resident <= 16 * 1024,
        "aggregate history grew to {resident} bytes"
    );
    let retained: lash_core::RetainedValue =
        serde_json::from_value(found.pop().unwrap()).expect("archive");
    assert!(retained.output.witness.len() <= POLICY.witness_bytes as usize);
    sweep_without_grace(&backend).await;
    let values: Vec<serde_json::Value> =
        serde_json::from_str(&stored_text(&backend, &retained.output.reference).await)
            .expect("full print values");
    assert_eq!(values.len(), 200);
    for (i, value) in values.iter().enumerate() {
        let printed: serde_json::Value =
            serde_json::from_str(value.as_str().expect("console text"))
                .expect("the whole printed value");
        assert_eq!(
            printed,
            serde_json::json!({"index": i, "text": "small printed value é🙂"})
        );
    }
    Ok(())
}

/// ADR 0147: an RLM cell sees its frame and its variables, and nothing else.
/// The request built after a cell that printed past its render budget and
/// bound variables declares no `rlm.history` operation, carries no transcript
/// paragraph or item schema, labels no variable with the cell that bound it,
/// and addresses no history entry in any message: the cut states what was
/// shown and its total size.
#[cfg(feature = "rlm")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_rlm_request_carries_no_history_surface_on_sqlite() -> Result<()> {
    let requests = Arc::new(StdMutex::new(Vec::<LlmRequest>::new()));
    let captured = Arc::clone(&requests);
    let responses = Arc::new(StdMutex::new(std::collections::VecDeque::from([
        typescript_block("const note = \"x\".repeat(12000);\nconsole.log(note);\nconst total = 3;"),
        typescript_block("await control.finish(total);"),
    ])));
    let provider = lash_core::testing::TestProvider::builder()
        .kind("history-surface")
        .complete(move |request| {
            captured.lock_recover().push(request.clone());
            let text = responses
                .lock_recover()
                .pop_front()
                .expect("a scripted cell");
            async move {
                Ok(LlmResponse {
                    parts: vec![text],
                    ..Default::default()
                })
            }
        })
        .build()
        .into_handle();
    let backend = sqlite_memory_store_backend().await;
    let core = explicit_ephemeral_facets(rlm_core_builder_over(backend))
        .serve_test_llm_profile(provider, mock_llm_profile_spec())
        .build(lash_facade_tests::runtime_lease_owner())?;
    let session = created_session(
        &core,
        lash_vocabulary::identity::SessionId::parse("history-surface")
            .expect("nonblank host identity"),
    )
    .await
    .open()
    .await?;
    let output = session
        .send(TurnInput::text("print, bind, then finish"))
        .output()
        .await?;
    assert_eq!(
        output.finished().map(|(_, value)| value),
        Some(&serde_json::json!(3))
    );
    let requests = requests.lock_recover();
    assert_eq!(requests.len(), 2, "one request per cell");
    let request = serde_json::to_string(&requests[1]).expect("the request encodes");
    assert!(request.contains("[cut: "), "the print was cut: {request}");
    assert!(
        request.contains("`total`"),
        "the variables are listed: {request}"
    );
    for surface in [
        "rlm.history",
        "Transcript (",
        "HistoryItem",
        "— from cell",
        "history.at(",
        "output_archive",
        "read_output",
        "readOutput",
    ] {
        assert!(
            !request.contains(surface),
            "the request carries `{surface}`: {request}"
        );
    }
    Ok(())
}
