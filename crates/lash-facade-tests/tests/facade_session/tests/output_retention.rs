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
//! * **exact retrieval** — every retained block's reference resolves to the
//!   complete bytes it stands for;
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
        serde_json::json!({}),
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

/// The cell that finishes a turn with 3,000 rows: far past the 4 KiB policy,
/// so history keeps the final value retained.
#[cfg(feature = "rlm")]
const ROWS_CELL: &str = r#"
const rows = [];
for (let i = 0; i < 3000; i++) {
  rows.push({ index: i, text: "a row the cell finishes with" });
}
await control.finish({ rows });"#;

/// The prefix of a cell that binds `prior` to the retained final value the
/// session's earlier turn left in history.
#[cfg(feature = "rlm")]
const PRIOR_FINAL_OUTPUT: &str = r#"
let prior;
for (const step of history) {
  if (step.kind === "lash_vm_step" && step.final_output !== undefined) {
    prior = step.final_output;
  }
}
const rows = prior.shape.fields.find((field) => field.key === "rows").shape;"#;

/// Runs [`ROWS_CELL`] as one turn of a fresh session, lets `between` act on
/// the backend and the retained value, then runs `reader` as the next turn
/// and answers its finish value with the records of the tool calls it made.
#[cfg(feature = "rlm")]
async fn read_back_retained_rows<F, Fut>(
    session_id: &str,
    reader: &str,
    between: F,
) -> Result<(serde_json::Value, serde_json::Value)>
where
    F: FnOnce(lash_execution_contracts::backend::Backend, lash_core::RetainedValue) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
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
                typescript_block(ROWS_CELL),
                typescript_block(&format!("{PRIOR_FINAL_OUTPUT}\n{reader}")),
            ]),
            mock_llm_profile_spec(),
        )
        .build(lash_facade_tests::runtime_lease_owner())?;
    let session = created_session(
        &core,
        lash_vocabulary::identity::SessionId::parse(session_id).expect("nonblank host identity"),
    )
    .await
    .open()
    .await?;
    session
        .send(TurnInput::text("finish with the rows"))
        .output()
        .await?;
    let (_, window) = committed_frame(&backend, session_id).await;
    let mut finals = Vec::new();
    collect_retained(
        &serde_json::to_value(&window.window).expect("the frame serializes"),
        &mut Vec::new(),
        &mut finals,
    );
    let [finished] = finals.as_slice() else {
        panic!("one retained final value: {finals:#?}");
    };
    between(backend, finished.clone()).await;
    let output = session
        .send(TurnInput::text("read the rows back"))
        .output()
        .await?;
    let value = output
        .finished()
        .map(|(_, value)| value)
        .cloned()
        .expect("the reading turn finishes with a value");
    let output_tool_calls =
        lash::testing::tool_calls_for_activity(&session.durable(), &output.activities).await;
    Ok((
        value,
        serde_json::to_value(lash_protocol_types::durable_serde::Durable(
            &output_tool_calls,
        ))
        .expect("the records serialize"),
    ))
}

/// Whether `value` holds `wanted` anywhere: as itself, or below.
#[cfg(feature = "rlm")]
fn holds(value: &serde_json::Value, wanted: &serde_json::Value) -> bool {
    value == wanted
        || match value {
            serde_json::Value::Object(entries) => {
                entries.values().any(|entry| holds(entry, wanted))
            }
            serde_json::Value::Array(entries) => entries.iter().any(|entry| holds(entry, wanted)),
            _ => false,
        }
}

/// FIG-1439: a value retained out of history reaches a later cell as its
/// shape, not as a cut of its text. The cell learns how many rows the value
/// holds and which keys each row has without reading it, then reads it whole
/// when it needs it. Before the fix the finishing step was folded out of
/// `history` behind the turn's reply, a copy of the value's text cut at
/// 16,384 characters: a later cell found no step holding the value, only a
/// truncated prefix of its JSON to misread.
#[cfg(feature = "rlm")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_retained_value_is_read_in_place_as_its_shape_and_whole_on_request_on_sqlite()
-> Result<()> {
    let (read, _) = read_back_retained_rows(
        "retained-shape",
        r#"
const whole = await control.readOutput({ archive: prior.attachment });
await control.finish({
  described: rows.length,
  keys: rows.items.fields.map((field) => field.key),
  read: whole.rows.length,
});"#,
        |_, _| async {},
    )
    .await?;
    assert_eq!(
        read,
        serde_json::json!({"described": 3000, "keys": ["index", "text"], "read": 3000})
    );
    Ok(())
}

/// FIG-1439: asking for a retained value whose bytes are no longer stored
/// throws in the cell, at the point of asking, as a failure the program
/// catches — and its shape still answers what the value held. The call's
/// record keeps the typed cause, naming the attachment.
#[cfg(feature = "rlm")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn asking_for_a_reclaimed_retained_value_throws_a_catchable_refusal_on_sqlite() -> Result<()>
{
    let reclaimed = Arc::new(std::sync::Mutex::new(None));
    let noted = Arc::clone(&reclaimed);
    let (read, tool_calls) = read_back_retained_rows(
        "retained-reclaimed",
        r#"
try {
  await control.readOutput({ archive: prior.attachment });
  await control.finish("read");
} catch (error) {
  await control.finish({ name: error.name, described: rows.length });
}"#,
        |backend, finished| async move {
            let attachment = finished.output.reference.id;
            backend
                .attachment_store()
                .delete(&attachment)
                .await
                .expect("reclaim the retained value's bytes");
            *noted.lock().expect("note the attachment") = Some(attachment);
        },
    )
    .await?;
    assert_eq!(
        read,
        serde_json::json!({"name": "tool_failed", "described": 3000})
    );
    let attachment = reclaimed
        .lock()
        .expect("read the attachment")
        .clone()
        .expect("the retained value was reclaimed");
    assert!(
        holds(
            &tool_calls,
            &serde_json::to_value(lash_core::ToolFailureCause::RetainedValueUnavailable {
                attachment
            })
            .expect("the cause encodes")
        ),
        "the read_output call records its typed cause"
    );
    Ok(())
}

#[cfg(feature = "rlm")]
fn archive_reader_cell() -> &'static str {
    r#"
for (let i = 0; i < history.length; i++) {
  const step = history[i];
  if (step.kind === "lash_vm_step" && step.output_archive) {
    await control.finish(await control.readOutput({ archive: step.output_archive.attachment }));
  }
}
await control.finish("missing archive");
"#
}

#[cfg(feature = "rlm")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn step_archive_refetch_survives_cold_reopen_branch_and_continue_as_on_sqlite() -> Result<()>
{
    let backend = sqlite_memory_store_backend().await;
    // Each print is the console's text of the object the cell logged.
    let expected: Vec<serde_json::Value> = (0..200)
        .map(|i| {
            serde_json::Value::String(format!(
                r#"{{"index":{i},"text":"exact é🙂\nvalue","null":null,"nested":[{i},false]}}"#
            ))
        })
        .collect();
    let core = explicit_ephemeral_facets(rlm_core_builder_over(backend.clone()))
        .data_retention(lash::DataRetention {
            attachments: lash::persistence::AttachmentPolicy {
                output_retention: POLICY,
                ..lash::persistence::AttachmentPolicy::standard()
            },
            ..lash::DataRetention::standard()
        })
        .serve_test_llm_profile(queued_text_provider(vec![
            typescript_block(r#"for (let i = 0; i < 200; i++) { console.log({index: i, text: "exact é🙂\nvalue", null: null, nested: [i, false]}); } await control.finish(200);"#),
        ]), mock_llm_profile_spec())
        .build(lash_facade_tests::runtime_lease_owner())?;
    let session = created_session(
        &core,
        lash_vocabulary::identity::SessionId::parse("archive-history")
            .expect("nonblank host identity"),
    )
    .await
    .open()
    .await?;
    let output = session
        .send(TurnInput::text("archive the prints"))
        .output()
        .await?;
    assert_eq!(
        output.finished().map(|(_, value)| value),
        Some(&serde_json::json!(200))
    );
    let (_, window) = committed_frame(&backend, "archive-history").await;
    let mut archives = Vec::new();
    collect_retained(
        &serde_json::to_value(window.window).expect("window"),
        &mut archives,
        &mut Vec::new(),
    );
    let [archive] = archives.as_slice() else {
        panic!("one step archive");
    };
    let archive = archive.clone();
    let edges = backend
        .session_store_factory()
        .attachment_referrers(&archive.output.reference.id)
        .await
        .expect("exact referrers");
    assert!(edges.iter().any(|edge| matches!(edge, lash_core::ArtifactReferrer::Session(id) if id.as_str() == "archive-history")), "commit rooted the archive: {edges:?}");
    let revision = session
        .revisions()
        .await?
        .into_iter()
        .find(|revision| revision.head)
        .expect("head")
        .head_revision;
    session.close().await.expect("release resident executor");
    drop(core);

    let core = explicit_ephemeral_facets(rlm_core_builder_over(backend.clone()))
        .data_retention(lash::DataRetention {
            attachments: lash::persistence::AttachmentPolicy {
                output_retention: POLICY,
                ..lash::persistence::AttachmentPolicy::standard()
            },
            ..lash::DataRetention::standard()
        })
        .serve_test_llm_profile(queued_text_provider(vec![
            typescript_block(archive_reader_cell()),
            typescript_block(archive_reader_cell()),
            typescript_block(r#"
for (let i = 0; i < history.length; i++) {
  const step = history[i];
  if (step.kind === "lash_vm_step" && step.output_archive) {
    await control.continueAs({ task: "read shared archive in fresh frame", seed: { archive: step.output_archive.attachment } });
  }
}
"#),
            typescript_block("await control.finish(await control.readOutput({ archive }));"),
        ]), mock_llm_profile_spec())
        .build(lash_facade_tests::runtime_lease_owner())?;
    sweep_without_grace(&backend).await;
    let session = core
        .session(
            lash_vocabulary::identity::SessionId::parse("archive-history")
                .expect("nonblank host identity"),
        )
        .open()
        .await?;
    let cold = session
        .send(TurnInput::text("read after cold reopen"))
        .output()
        .await?;
    assert_eq!(
        cold.finished().map(|(_, value)| value),
        Some(&serde_json::json!(expected))
    );
    core.fork_at(
        &SessionId::fixture("archive-history"),
        lash_core::Target::Revision(revision),
        lash::ForkRequest {
            session_id: SessionId::fixture("archive-branch"),
            relation: lash_core::SessionRelation::Fork {
                source_session_id: SessionId::fixture("archive-history"),
                source_node_id: None,
            },
            observed_processes: Vec::new(),
        },
    )
    .await?;
    let branch = core
        .session(
            lash_vocabulary::identity::SessionId::parse("archive-branch")
                .expect("nonblank host identity"),
        )
        .open()
        .await?;
    let branched = branch
        .send(TurnInput::text("read shared history on a branch"))
        .output()
        .await?;
    assert_eq!(
        branched.finished().map(|(_, value)| value),
        Some(&serde_json::json!(expected))
    );
    branch.close().await.expect("close branch runtime");
    // The switch answers its send; the frame's task runs next as its own
    // run (FIG-5232), reading the archive its seed carries.
    let switched = session
        .send(TurnInput::text("switch frames then read"))
        .output()
        .await?;
    let lash_core::facade_support::TurnOutcome::AgentFrameSwitch { frame_key, .. } =
        &switched.outcome
    else {
        panic!("the switch answers its send: {:?}", switched.outcome);
    };
    let continued = session
        .attach_id(lash_core::runtime::durable::session_mail::frame_task_run(
            frame_key,
        ))
        .output()
        .await?;
    assert_eq!(
        continued.finished().map(|(_, value)| value),
        Some(&serde_json::json!(expected))
    );
    sweep_without_grace(&backend).await;
    let values: Vec<serde_json::Value> =
        serde_json::from_str(&stored_text(&backend, &archive.output.reference).await)
            .expect("archive survives frame switch");
    assert_eq!(values, expected);
    session.close().await.expect("close fresh frame");
    delete_session_and_await(&core, "archive-history").await?;
    let factory = backend.session_store_factory();
    let source = SessionId::fixture("archive-history");
    assert_eq!(
        factory.session_referrer_state(&source).await?,
        lash_core::store::SessionReferrerState::DeletedRetained,
        "a surviving branch protects the source-owned archive edge"
    );
    sweep_without_grace(&backend).await;
    let shared: Vec<serde_json::Value> =
        serde_json::from_str(&stored_text(&backend, &archive.output.reference).await)
            .expect("branch-protected archive");
    assert_eq!(shared, expected);
    delete_session_and_await(&core, "archive-branch").await?;
    assert_eq!(
        factory.session_referrer_state(&source).await?,
        lash_core::store::SessionReferrerState::DeletedRetired,
        "the last shared-history reader has retired"
    );
    // Retirement alone does not delete bytes: the host explicitly releases
    // the source hold, prunes settled execution evidence, and sweeps bytes.
    stored_text(&backend, &archive.output.reference).await;
    factory
        .end_attachment_referrer(&lash_core::ArtifactReferrer::Session(source))
        .await?;
    factory
        .reclaim_retained_evidence(lash_core::RetentionBound {
            committed_before_epoch_ms: u64::MAX,
            turn_watermark: lash_core::store::TurnProjectionWatermark::NoProjector,
        })
        .await
        .expect("host prunes settled execution evidence");
    // The core's cleanup relay ends the producer holds the deletions armed
    // as `ArtifactCleanup` (ADR 0132 §12). Evidence pruning alone cannot
    // release those holds.
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while !factory
            .attachment_referrers(&archive.output.reference.id)
            .await
            .expect("remaining archive holds")
            .is_empty()
        {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the relayed cleanups release the archive producer holds");
    let reclaimed = lash_core::facade_support::reclaim_unreferenced_attachments(
        factory.as_ref(),
        backend.attachment_store().as_ref(),
        lash_core::AttachmentReclamationPolicy::new(
            0,
            lash_core::EmptyRootSetPolicy::AuthorizeDeleteAll,
        ),
    )
    .await
    .expect("the host authorizes attachment reclamation");
    assert!(reclaimed.reclaimed_count > 0, "{reclaimed:?}");
    assert!(
        matches!(
            backend
                .attachment_store()
                .get(&archive.output.reference.id, 32 * 1024 * 1024)
                .await,
            Err(lash_core::AttachmentStoreError::NotFound(_))
        ),
        "the explicit retention lever reclaims the archive"
    );
    Ok(())
}

#[cfg(feature = "rlm")]
struct ControlSession {
    core: LashCore,
    session: lash::LashSession,
    requests: Arc<StdMutex<Vec<String>>>,
    responses: Arc<tokio::sync::Mutex<std::collections::VecDeque<LlmResponse>>>,
}

#[cfg(feature = "rlm")]
async fn control_session(
    dialect: lash::rlm::CellDialect,
    channel: lash::rlm::RlmChannel,
    cells: Vec<String>,
) -> Result<ControlSession> {
    let backend = sqlite_memory_store_backend().await;
    let responses = Arc::new(tokio::sync::Mutex::new(std::collections::VecDeque::from(
        cells
            .into_iter()
            .enumerate()
            .map(|(index, code)| control_cell_response(&dialect, channel, index, code))
            .collect::<Vec<_>>(),
    )));
    let scripted = responses.clone();
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let captured = requests.clone();
    let provider = lash::testing::TestProvider::builder().kind("restricted-controls")
        .complete(move |request| {
            let responses = scripted.clone();
            let captured = captured.clone();
            async move {
                captured.lock_recover().push(serde_json::json!({"instructions": request.instructions, "messages": request.messages}).to_string());
                Ok(responses.lock().await.pop_front().unwrap_or_else(|| {
                    let feedback = request.messages.iter().flat_map(|message| message.blocks.iter())
                        .filter_map(|block| match block {
                            LlmContentBlock::Text { text, .. } => text.split_once("\n\nNext:").map(|(message, _)| message.to_string()),
                            _ => None,
                        }).collect::<Vec<_>>();
                    panic!("scripted control cell exhausted: {feedback:?}");
                }))
            }
        }).build().into_handle();
    let factory = lash::rlm::RlmProtocolPluginFactory::new(
        lash::rlm::RlmProtocolPluginConfig::builder()
            .channel(channel)
            .instruction_limit(lash::rlm::InstructionBound::standard())
            .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
            .build(),
        dialect,
    )
    .with_worker_service(untimed_fixture_workers());
    let core = explicit_ephemeral_facets(LashCore::rlm_builder(backend.clone(), factory))
        .data_retention(lash::DataRetention {
            attachments: lash::persistence::AttachmentPolicy {
                output_retention: POLICY,
                ..lash::persistence::AttachmentPolicy::standard()
            },
            ..lash::DataRetention::standard()
        })
        .serve_test_llm_profile(provider, mock_llm_profile_spec())
        .build(lash_facade_tests::runtime_lease_owner())?;
    let id = SessionId::fixture("restricted-controls");
    core.session(id.clone())
        .create(lash::SessionCreation::root(mock_session_spec()))
        .await?;
    let session = core.session(id).open().await?;
    Ok(ControlSession {
        core,
        session,
        requests,
        responses,
    })
}

/// FIG-6045: the public reader definition grants access to the whole retained
/// value, on both transports and with either dialect's callable spelling.
#[cfg(feature = "rlm")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_output_definition_reads_retained_value_on_sqlite() -> Result<()> {
    for dialect in [
        lash::rlm::CellDialect::typescript(),
        lash::rlm::CellDialect::python(),
    ] {
        for channel in [
            lash::rlm::RlmChannel::Cell,
            lash::rlm::RlmChannel::NativeTool,
        ] {
            let python = dialect.name() == "python";
            let rows = if python {
                "rows = []\nfor i in range(3000):\n    rows.append({'index': i, 'text': 'a retained row'})\nawait control.finish({'rows': rows})".to_string()
            } else {
                ROWS_CELL.to_string()
            };
            let ControlSession {
                core,
                session,
                requests,
                responses,
            } = control_session(dialect.clone(), channel, vec![rows]).await?;
            let original = session
                .send(TurnInput::text("retain rows"))
                .output()
                .await?;
            let reader = if python {
                "archive = history[1]['final_output']['attachment']\nwhole = await control.read_output({'archive': archive})\nawait control.finish(whole)".to_string()
            } else {
                format!(
                    "{PRIOR_FINAL_OUTPUT}\nawait control.finish(await control.readOutput({{ archive: prior.attachment }}));"
                )
            };
            responses
                .lock()
                .await
                .push_back(control_cell_response(&dialect, channel, 1, reader));
            let read = session
                .send(TurnInput::text("read whole retained value"))
                .output()
                .await?;
            assert!(
                read.finished().is_some(),
                "{} {channel:?}: {:?}",
                dialect.name(),
                read.outcome
            );
            assert_eq!(
                read.finished(),
                original.finished(),
                "retrieval preserves the complete retained value"
            );
            let call_path = if python {
                "control.read_output"
            } else {
                "control.readOutput"
            };
            {
                let requests = requests.lock_recover();
                assert_eq!(requests.len(), 2);
                assert!(
                    requests[1].contains(&format!("{call_path}(")),
                    "the offered reader has a dialect-correct signature"
                );
                assert!(
                    requests[1].contains("Read a value retained out of history whole."),
                    "the offered catalog teaches retrieval"
                );
            }
            session.close().await?;
            drop(core);
        }
    }
    Ok(())
}
