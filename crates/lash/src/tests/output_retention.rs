//! Oversized output is retained before it enters history (FIG-1643): real
//! turns on the Restate double, over SQLite and over PostgreSQL.
//!
//! A standard turn calls three tools as parallel children — one returns far
//! more than the renderer shows, one fails with a message as long, and one
//! returns a line a plugin's presentation step then extends past the byte
//! policy. An RLM turn prints a large value and finishes with it. After each
//! commit the laws read the committed history back from the store:
//!
//! * **resident size** — the committed frame is bounded, whatever the
//!   outputs weighed;
//! * **exact retrieval** — every retained block's reference resolves to the
//!   complete bytes it stands for;
//! * **rooted** — an attachment sweep with no grace keeps every retained
//!   attachment the commit names.

#![allow(clippy::disallowed_methods)]

use super::*;
use lash_core::llm::types::LlmResponse;

/// A policy far below the outputs: 4 KiB inline, a 512-byte witness.
const POLICY: lash_core::OutputRetentionPolicy = lash_core::OutputRetentionPolicy {
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

fn retention_tool(name: &str) -> lash_core::ToolDefinition {
    test_tool_definition_with_tool_binding(
        lash_core::ToolDefinition::raw(
            format!("tool:{name}"),
            name,
            "A tool whose output is larger than history keeps.",
            serde_json::json!({"type": "object", "properties": {}, "additionalProperties": false}),
            serde_json::json!({}),
        ),
        name,
    )
}

const TOOLS: [&str; 3] = ["bulk_ok", "bulk_fail", "small_ok"];

#[async_trait]
impl ToolProvider for RetentionTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        TOOLS
            .iter()
            .map(|name| retention_tool(name).manifest())
            .collect()
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        TOOLS
            .contains(&name)
            .then(|| Arc::new(retention_tool(name).contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        match call.name() {
            "bulk_ok" => lash_core::ToolOutcome::ok(serde_json::json!(bulk_text())).into(),
            "bulk_fail" => {
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
        if input.context.tool_name == "small_ok" {
            next.parts
                .push(lash_core::facade_support::ModelToolReturnPart::text(
                    appendix_text(),
                ));
        }
        Box::pin(async move { Ok::<_, lash_core::PluginError>(next) })
    })
}

fn tool_calling_provider() -> ProviderHandle {
    let calls = Arc::new(AtomicUsize::new(0));
    crate::testing::TestProvider::builder()
        .kind("output-retention")
        .complete(move |_request| {
            let call = calls.fetch_add(1, Ordering::SeqCst);
            async move {
                let parts = if call == 0 {
                    TOOLS
                        .iter()
                        .map(|name| LlmOutputPart::ToolCall {
                            call_id: format!("call-{name}"),
                            tool_name: (*name).to_string(),
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
    backend: &lash_core::Backend,
    session_id: &str,
) -> (usize, lash_core::store::SessionWindowRead) {
    let window = lash_core::SessionHistoryStore::load_session_window(
        backend.session_store_factory().as_ref(),
        &SessionId::from(session_id),
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
async fn sweep_without_grace(backend: &lash_core::Backend) {
    lash_core::facade_support::reclaim_unreferenced_attachments(
        backend.session_store_factory().as_ref(),
        backend.attachment_store().as_ref(),
        lash_core::AttachmentReclamationPolicy {
            grace_period_ms: 0,
            empty_root_set: lash_core::EmptyRootSetPolicy::Refuse,
        },
    )
    .await
    .expect("sweep the attachments");
}

async fn stored_text(backend: &lash_core::Backend, reference: &lash_core::AttachmentRef) -> String {
    let stored = backend
        .attachment_store()
        .get(&reference.id)
        .await
        .expect("the retained attachment survives the sweep");
    assert_eq!(stored.bytes.len() as u64, reference.byte_len);
    String::from_utf8(stored.bytes).expect("retained text")
}

async fn oversized_tool_output_is_retained_before_it_enters_history(
    backend: lash_core::Backend,
) -> Result<()> {
    const SESSION: &str = "standard-output-retention";
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        backend.clone(),
        crate::TurnBudget::Unbounded,
    ))
    .output_retention(POLICY)
    .provider(tool_calling_provider())
    .model(mock_model_spec())
    .tools(Arc::new(RetentionTools))
    .plugin(Arc::new(StaticPluginFactory::new(
        "output-retention-appendix",
        lash_core::plugin::PluginSpec::new().with_presentation_step(appendix_step()),
    )))
    .build(crate::testing::runtime_lease_owner())?;
    let session = core.session(SESSION).open().await?;
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
        vec!["bulk_fail", "bulk_ok", "small_ok"],
        "every oversized result is retained: {retained:#?}"
    );

    sweep_without_grace(&backend).await;
    assert_eq!(
        stored_text(&backend, &retained["bulk_ok"].reference).await,
        bulk_text()
    );
    assert_eq!(
        stored_text(&backend, &retained["bulk_fail"].reference).await,
        format!("[Tool execution failed]\n{}", failure_text())
    );
    let appended = stored_text(&backend, &retained["small_ok"].reference).await;
    assert!(appended.contains("small"), "{}", &appended[..64]);
    assert!(appended.ends_with(&appendix_text()));
    assert!(
        retained["small_ok"].witness.len() <= 512,
        "the plugin addition's witness is bounded by the policy"
    );
    Ok(())
}

#[cfg(feature = "rlm")]
async fn oversized_rlm_print_and_final_value_are_retained_before_they_enter_history(
    backend: lash_core::Backend,
) -> Result<()> {
    const SESSION: &str = "rlm-output-retention";
    let core = explicit_ephemeral_facets(super::rlm_core_builder_over(backend.clone()))
        .output_retention(POLICY)
        .provider(super::queued_text_provider(vec![super::typescript_block(
            r#"
const rows = [];
for (let i = 0; i < 3000; i++) {
  rows.push({ index: i, text: "a row the cell prints and finishes with" });
}
print(rows);
finish({ rows });"#,
        )]))
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session(SESSION).open().await?;
    let output = session
        .send(TurnInput::text("print the rows"))
        .output()
        .await?;
    // The host is answered with the whole value: retention bounds history,
    // not the turn's answer.
    let value = output
        .final_value()
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

    sweep_without_grace(&backend).await;
    assert_eq!(
        stored_text(&backend, &finished.reference).await,
        serde_json::to_string(&value).expect("encode the final value")
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&stored_text(&backend, &print.reference).await)
            .expect("the retained print is JSON"),
        value["rows"]
    );
    for retained in [print, finished] {
        assert!(retained.witness.len() <= 512);
    }
    Ok(())
}

/// Every retained print (`retained`) and final value (`final_output_retained`)
/// anywhere in `value`, the committed frame.
#[cfg(feature = "rlm")]
fn collect_retained(
    value: &serde_json::Value,
    prints: &mut Vec<lash_core::RetainedOutput>,
    finals: &mut Vec<lash_core::RetainedOutput>,
) {
    match value {
        serde_json::Value::Object(entries) => {
            for (key, entry) in entries {
                let decoded = || {
                    serde_json::from_value::<lash_core::RetainedOutput>(entry.clone())
                        .expect("a retained output decodes")
                };
                match key.as_str() {
                    "retained" if entries.contains_key("text") => prints.push(decoded()),
                    "final_output_retained" => finals.push(decoded()),
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

/// The Restate double over a fresh PostgreSQL store set, or `None` when no
/// database URL is set. `LASH_REQUIRE_POSTGRES=1` makes a missing URL a
/// failure, so a gate that promises the PostgreSQL leg cannot skip it.
async fn postgres_double() -> Option<(lash_core::Backend, Box<dyn std::any::Any>)> {
    let Ok(url) = std::env::var("LASH_POSTGRES_DATABASE_URL") else {
        assert!(
            std::env::var("LASH_REQUIRE_POSTGRES").as_deref() != Ok("1"),
            "LASH_REQUIRE_POSTGRES=1 but LASH_POSTGRES_DATABASE_URL is not set"
        );
        eprintln!("skipping the PostgreSQL leg: LASH_POSTGRES_DATABASE_URL is not set");
        return None;
    };
    let database = lash_postgres_store::testing::IsolatedDatabase::create(&url).await;
    let storage = lash_postgres_store::PostgresStorage::connect(database.url())
        .await
        .expect("connect to PostgreSQL");
    let attachments = tempfile::tempdir().expect("PostgreSQL attachment directory");
    let stores = Arc::new(lash_postgres_store::PostgresStoreSet::new(
        &storage,
        Arc::new(lash_core::facade_support::FileAttachmentStore::new(
            attachments.path(),
        )),
    )) as Arc<dyn lash_core::StoreSet>;
    let backend =
        double_backend_over(lash_restate_test::ServerConfig::default(), move |_| stores).await;
    Some((backend, Box::new((database, attachments, storage))))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oversized_tool_output_is_retained_before_it_enters_history_on_sqlite() -> Result<()> {
    oversized_tool_output_is_retained_before_it_enters_history(double_backend().await).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oversized_tool_output_is_retained_before_it_enters_history_on_postgres() -> Result<()> {
    let Some((backend, _held)) = postgres_double().await else {
        return Ok(());
    };
    oversized_tool_output_is_retained_before_it_enters_history(backend).await
}

#[cfg(feature = "rlm")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oversized_rlm_print_and_final_value_are_retained_before_they_enter_history_on_sqlite()
-> Result<()> {
    oversized_rlm_print_and_final_value_are_retained_before_they_enter_history(
        double_backend().await,
    )
    .await
}

#[cfg(feature = "rlm")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oversized_rlm_print_and_final_value_are_retained_before_they_enter_history_on_postgres()
-> Result<()> {
    let Some((backend, _held)) = postgres_double().await else {
        return Ok(());
    };
    oversized_rlm_print_and_final_value_are_retained_before_they_enter_history(backend).await
}
