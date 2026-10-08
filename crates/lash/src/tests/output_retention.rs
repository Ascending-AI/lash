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
        )
        .expect("valid declared tool schemas")
        .with_execution(std::time::Duration::from_secs(120)),
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
async fn sweep_without_grace(backend: &lash_core::Backend) {
    lash_core::facade_support::reclaim_unreferenced_attachments(
        backend.session_store_factory().as_ref(),
        backend.attachment_store().as_ref(),
        lash_core::AttachmentReclamationPolicy::new(0, lash_core::EmptyRootSetPolicy::Refuse),
    )
    .await
    .expect("sweep the attachments");
}

async fn stored_text(backend: &lash_core::Backend, reference: &lash_core::AttachmentRef) -> String {
    let stored = backend
        .attachment_store()
        .get(&reference.id, 32 * 1024 * 1024)
        .await
        .expect("the retained attachment survives the sweep");
    assert_eq!(stored.bytes.len() as u64, reference.byte_len);
    String::from_utf8(stored.bytes).expect("retained text")
}

async fn oversized_tool_output_is_retained_before_it_enters_history(
    backend: lash_core::Backend,
) -> Result<()> {
    const SESSION: &str = "standard-output-retention";
    let core = explicit_ephemeral_facets(LashCore::standard_builder(backend.clone()))
        .data_retention(crate::DataRetention {
            attachments: crate::persistence::AttachmentPolicy {
                output_retention: POLICY,
                ..crate::persistence::AttachmentPolicy::standard()
            },
            ..crate::DataRetention::standard()
        })
        .serve_test_llm_profile(tool_calling_provider(), mock_llm_profile_spec())
        .tools(Arc::new(RetentionTools))
        .plugin(Arc::new(StaticPluginFactory::new(
            lash_core::plugin::PluginDeclaration::initial("output-retention-appendix"),
            lash_core::plugin::PluginSpec::new()
                .with_presentation_step(crate::hook_key!("presentation-step-1"), appendix_step()),
        )))
        .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(crate::SessionId::parse(SESSION).expect("nonblank host identity"))
        .created()
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
    let core = explicit_ephemeral_facets(rlm_core_builder_over(backend.clone()))
        .data_retention(crate::DataRetention {
            attachments: crate::persistence::AttachmentPolicy {
                output_retention: POLICY,
                ..crate::persistence::AttachmentPolicy::standard()
            },
            ..crate::DataRetention::standard()
        })
        .serve_test_llm_profile(
            queued_text_provider(vec![typescript_block(
                r#"
const rows = [];
for (let i = 0; i < 3000; i++) {
  rows.push({ index: i, text: "a row the cell prints and finishes with" });
}
print(rows);
finish({ rows });"#,
            )]),
            mock_llm_profile_spec(),
        )
        .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(crate::SessionId::parse(SESSION).expect("nonblank host identity"))
        .created()
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

    for retained in [print, finished] {
        let edges = backend
            .session_store_factory()
            .attachment_referrers(&retained.reference.id)
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
        stored_text(&backend, &finished.reference).await,
        serde_json::to_string(&value).expect("encode the final value")
    );
    assert_eq!(
        serde_json::from_str::<Vec<lash_core::CellPrint>>(
            &stored_text(&backend, &print.reference).await
        )
        .expect("the retained archive is JSON")[0]
            .value,
        value["rows"]
    );
    for retained in [print, finished] {
        assert!(retained.witness.len() <= 512);
    }
    Ok(())
}

/// Every cell's retained prints (`prints_retained`) and retained finish value
/// (a `finished` result's `retained`) anywhere in `value`, the committed frame.
#[cfg(feature = "rlm")]
fn collect_retained(
    value: &serde_json::Value,
    prints: &mut Vec<lash_core::RetainedOutput>,
    finals: &mut Vec<lash_core::RetainedOutput>,
) {
    match value {
        serde_json::Value::Object(entries) => {
            for (key, entry) in entries {
                let decoded = |retained: &serde_json::Value| {
                    serde_json::from_value::<lash_core::RetainedOutput>(retained.clone())
                        .expect("a retained output decodes")
                };
                match key.as_str() {
                    "prints_retained" if !entry.is_null() => prints.push(decoded(entry)),
                    "result"
                        if entry["kind"] == "finished"
                            && entry["value"]["retained"].is_object() =>
                    {
                        finals.push(decoded(&entry["value"]["retained"]));
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
async fn postgres_backend() -> (lash_core::Backend, Box<dyn std::any::Any>) {
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
        .data_retention(crate::DataRetention {
            attachments: crate::persistence::AttachmentPolicy {
                output_retention: POLICY,
                ..crate::persistence::AttachmentPolicy::standard()
            },
            ..crate::DataRetention::standard()
        })
        .serve_test_llm_profile(
            queued_text_provider(vec![
                typescript_block(
                    r#"
for (let i = 0; i < 200; i++) {
  print({ index: i, text: "small printed value é🙂" });
}
"#,
                ),
                typescript_block("finish(200);"),
            ]),
            mock_llm_profile_spec(),
        )
        .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(crate::SessionId::parse("aggregate-prints").expect("nonblank host identity"))
        .created()
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
    let retained: lash_core::RetainedOutput =
        serde_json::from_value(found.pop().unwrap()).expect("archive");
    assert!(retained.witness.len() <= POLICY.witness_bytes as usize);
    sweep_without_grace(&backend).await;
    let observations: Vec<lash_core::CellPrint> =
        serde_json::from_str(&stored_text(&backend, &retained.reference).await)
            .expect("full observations");
    assert_eq!(observations.len(), 200);
    for (i, observation) in observations.iter().enumerate() {
        let serialized = serde_json::to_value(observation).expect("observation");
        assert_eq!(
            serialized["value"],
            serde_json::json!({"index": i, "text": "small printed value é🙂"})
        );
    }
    Ok(())
}

#[cfg(feature = "rlm")]
fn archive_reader_cell() -> &'static str {
    r#"
for (let i = 0; i < history.length; i++) {
  const step = history[i];
  if (step.kind === "lashlang_step" && step.output_archive) {
    finish(await control.read_output({ archive: step.output_archive.attachment }));
  }
}
finish("missing archive");
"#
}

#[cfg(feature = "rlm")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "FIG-5318: a durable turn's commit roots no attachment its history references"]
async fn step_archive_refetch_survives_cold_reopen_branch_and_continue_as_on_sqlite() -> Result<()>
{
    let backend = sqlite_memory_store_backend().await;
    let expected: Vec<serde_json::Value> = (0..200)
        .map(|i| {
            serde_json::json!({
                "index": i, "text": "exact é🙂\nvalue", "null": null, "nested": [i, false],
            })
        })
        .collect();
    let core = explicit_ephemeral_facets(rlm_core_builder_over(backend.clone()))
        .data_retention(crate::DataRetention {
            attachments: crate::persistence::AttachmentPolicy {
                output_retention: POLICY,
                ..crate::persistence::AttachmentPolicy::standard()
            },
            ..crate::DataRetention::standard()
        })
        .serve_test_llm_profile(queued_text_provider(vec![
            typescript_block(r#"for (let i = 0; i < 200; i++) { print({index: i, text: "exact é🙂\nvalue", null: null, nested: [i, false]}); } finish(200);"#),
        ]), mock_llm_profile_spec())
        .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(crate::SessionId::parse("archive-history").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    let output = session
        .send(TurnInput::text("archive the prints"))
        .output()
        .await?;
    assert_eq!(output.final_value(), Some(&serde_json::json!(200)));
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
        .attachment_referrers(&archive.reference.id)
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
        .data_retention(crate::DataRetention {
            attachments: crate::persistence::AttachmentPolicy {
                output_retention: POLICY,
                ..crate::persistence::AttachmentPolicy::standard()
            },
            ..crate::DataRetention::standard()
        })
        .serve_test_llm_profile(queued_text_provider(vec![
            typescript_block(archive_reader_cell()),
            typescript_block(archive_reader_cell()),
            typescript_block(r#"
for (let i = 0; i < history.length; i++) {
  const step = history[i];
  if (step.kind === "lashlang_step" && step.output_archive) {
    await control.continue_as({ task: "read shared archive in fresh frame", seed: { archive: step.output_archive.attachment } });
  }
}
"#),
            typescript_block("finish(await control.read_output({ archive }));"),
        ]), mock_llm_profile_spec())
        .build(crate::testing::runtime_lease_owner())?;
    sweep_without_grace(&backend).await;
    let session = core
        .session(crate::SessionId::parse("archive-history").expect("nonblank host identity"))
        .open()
        .await?;
    let cold = session
        .send(TurnInput::text("read after cold reopen"))
        .output()
        .await?;
    assert_eq!(cold.final_value(), Some(&serde_json::json!(expected)));
    core.fork_at(
        &SessionId::fixture("archive-history"),
        lash_core::Target::Revision(revision),
        crate::ForkRequest {
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
        .session(crate::SessionId::parse("archive-branch").expect("nonblank host identity"))
        .open()
        .await?;
    let branched = branch
        .send(TurnInput::text("read shared history on a branch"))
        .output()
        .await?;
    assert_eq!(branched.final_value(), Some(&serde_json::json!(expected)));
    branch.close().await.expect("close branch runtime");
    // The switch answers its send; the frame's task runs next as its own
    // run (FIG-5232), reading the archive its seed carries.
    let switched = session
        .send(TurnInput::text("switch frames then read"))
        .output()
        .await?;
    let lash_core::facade_support::TurnOutcome::AgentFrameSwitch { frame_key, .. } =
        &switched.result.outcome
    else {
        panic!("the switch answers its send: {:?}", switched.result.outcome);
    };
    let continued = session
        .attach_id(lash_core::runtime::durable::session_mail::frame_task_run(
            frame_key,
        ))
        .output()
        .await?;
    assert_eq!(continued.final_value(), Some(&serde_json::json!(expected)));
    sweep_without_grace(&backend).await;
    let observations: Vec<lash_core::CellPrint> =
        serde_json::from_str(&stored_text(&backend, &archive.reference).await)
            .expect("archive survives frame switch");
    assert_eq!(
        observations
            .into_iter()
            .map(|observation| observation.value)
            .collect::<Vec<_>>(),
        expected
    );
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
    let shared: Vec<lash_core::CellPrint> =
        serde_json::from_str(&stored_text(&backend, &archive.reference).await)
            .expect("branch-protected archive");
    assert_eq!(
        shared
            .into_iter()
            .map(|print| print.value)
            .collect::<Vec<_>>(),
        expected
    );
    delete_session_and_await(&core, "archive-branch").await?;
    assert_eq!(
        factory.session_referrer_state(&source).await?,
        lash_core::store::SessionReferrerState::DeletedRetired,
        "the last shared-history reader has retired"
    );
    // Retirement alone does not delete bytes: the host explicitly releases
    // the source hold, prunes settled execution evidence, and sweeps bytes.
    stored_text(&backend, &archive.reference).await;
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
            .attachment_referrers(&archive.reference.id)
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
                .get(&archive.reference.id, 32 * 1024 * 1024)
                .await,
            Err(lash_core::AttachmentStoreError::NotFound(_))
        ),
        "the explicit retention lever reclaims the archive"
    );
    Ok(())
}
