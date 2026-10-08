use super::*;

/// The committed rows the transcript renderer law feeds: a user row carrying
/// two stored-image parts, and the RLM trajectory entry of a durable tool
/// projection, committed through the durable session's own admin append on an
/// in-memory store set. Canonical records come from committed history, never
/// a fixture authored beside the test (FIG-1530).
async fn durable_transcript_projection_fixture() -> Vec<crate::ChatRow> {
    let stores = lash::sqlite::SqliteStoreSet::memory()
        .await
        .expect("an in-memory store set opens");
    let backend = lash::durable::DurableBackendBuilder::new(Arc::new(stores))
        .build()
        .expect("the durable backend builds");
    let provider = lash::testing::TestProvider::builder()
        .kind("transcript-projection")
        .complete(|_request| async {
            Ok(lash::provider::LlmResponse {
                parts: vec![lash::direct::LlmOutputPart::Text {
                    text: "unused".to_owned(),
                    response_meta: None,
                }],
                ..Default::default()
            })
        })
        .build()
        .into_handle();
    let rlm_factory = lash::rlm::RlmProtocolPluginFactory::new(
        lash::rlm::RlmProtocolPluginConfig::builder()
            .channel(lash::rlm::RlmChannel::Cell)
            .instruction_limit(lash::rlm::InstructionBound::instructions(1_000_000))
            .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
            .build(),
        Arc::new(lash::rlm::TypescriptDialect),
        &backend,
    );
    let core = lash::LashCore::rlm_builder(backend, rlm_factory)
        .serve_test_llm_profile(
            provider,
            lash::LlmProfileMetadata::builder("transcript-projection-model")
                .context_window_tokens(200_000)
                .build()
                .expect("the model's metadata"),
        )
        .commit_budget(lash::CommitBudget::bounded(16 * 1024 * 1024, 4096))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .tool_source_policy(lash::tools::ToolSourcePolicy::Tolerate)
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "transcript-projection",
            "transcript-projection-boot",
        ))
        .expect("the core builds");
    let session_id = lash::SessionId::from("transcript-projection");
    let spec = lash::SessionSpec::new(
        "transcript-projection-model",
        lash::TurnBudget::Unbounded,
        lash::MaxToolCalls::new(8),
    )
    .no_progress_budget(lash::NoProgressBudget::bounded(12))
    .plugin(
        lash::rlm::RLM_PROTOCOL_PLUGIN_ID,
        lash::rlm::RlmCreateExtras::default(),
    )
    .expect("the RLM session options encode");
    core.session(session_id.clone())
        .create(lash::SessionCreation::root(
            lash::plugins::SessionToolAccess::ambient(),
            spec,
        ))
        .await
        .expect("the session is created");
    let session = core
        .session(session_id.clone())
        .open()
        .await
        .expect("the session opens");

    // RLM's printed-image projection can commit more than one stored image
    // part on a single message.
    let printed_images = ["sha256:rlm-printed-image-a", "sha256:rlm-printed-image-b"]
        .into_iter()
        .map(|id| lash::attachments::AttachmentRef {
            id: lash::attachments::AttachmentId::parse(
                format!("{:02x}", id.bytes().fold(0u8, u8::wrapping_add)).repeat(32),
            )
            .expect("valid attachment id"),
            media_type: lash::attachments::MediaType::parse("image/png").expect("PNG media type"),
            byte_len: 68,
            type_metadata: None,
            label: None,
        })
        .collect::<Vec<_>>();
    let mut committed =
        lash::plugins::PluginMessage::text(lash::messages::MessageRole::User, "two printed images")
            .with_id("rlm-printed-images");
    for attachment in &printed_images {
        committed.parts.push(lash::messages::Part::attachment_part(
            String::new(),
            String::new(),
            Some(lash::messages::PartAttachment {
                reference: attachment.clone(),
            }),
        ));
    }
    let trajectory = lash::rlm::RlmTrajectoryEntry {
        id: "durable-tool-trajectory".to_string(),
        protocol_iteration: 1,
        code: "durable.tool_projection()".to_string(),
        output_archive: Some(Box::new(lash::attachments::RetainedOutput {
            reference: lash::attachments::AttachmentRef {
                id: lash::attachments::content_id(b"durable-print-archive"),
                media_type: "application/json".parse().expect("media type"),
                byte_len: 90_000,
                type_metadata: None,
                label: None,
            },
            witness: "durable projection".to_string(),
        })),
        calls: vec![
            lash::persistence::ExecutedCall {
                operation: "durable.success".to_string(),
                outcome: lash::persistence::ExecutedCallOutcome::Ok,
                call_id: None,
            },
            lash::persistence::ExecutedCall {
                operation: "durable.failure".to_string(),
                outcome: lash::persistence::ExecutedCallOutcome::Err,
                call_id: None,
            },
        ],
        calls_omitted: 3,
        images: printed_images,
        ..lash::rlm::RlmTrajectoryEntry::default()
    };
    let outcome = session
        .admin()
        .state()
        .append_session_nodes(lash::plugins::AppendSessionNodesRequest {
            operation_id: "transcript-projection-fixture".to_string(),
            nodes: vec![
                lash::plugins::SessionAppendNode::message(committed),
                lash::plugins::SessionAppendNode::protocol_event(lash::rlm::rlm_protocol_event(
                    lash::rlm::RlmProtocolEvent::RlmTrajectoryEntry(trajectory),
                    lash::formats::RLM_PROTOCOL_EVENT_VERSION,
                )),
            ],
            requires_ancestor_node_id: None,
        })
        .await
        .expect("the fixture append answers")
        .settle_with(
            &session.admin().commands(),
            lash::testing::admin_fixture_outcome,
        )
        .await
        .expect("fixture mutation settled");
    assert!(
        matches!(
            outcome,
            lash::plugins::AppendSessionNodesOutcome::Appended { .. }
        ),
        "the fixture nodes must commit: {outcome:?}"
    );
    session.close().await.expect("the session closes");

    let rows = crate::ChatRow::all(
        core.session(session_id)
            .durable()
            .await
            .expect("the durable session opens")
            .transcript()
            .await
            .expect("the committed transcript decodes")
            .entries(),
    );
    assert!(
        rows.iter()
            .any(|row| row.kind == crate::ChatRowKind::CodeBlock
                && row.content.output.as_deref() == Some("durable projection")),
        "the committed durable tool trajectory projects a code row"
    );
    core.shutdown().await.expect("the core shuts down");
    rows
}

/// The committed transcript rows the session recorded survive every
/// production renderer the projection registry names
/// (scripts/transcript-projection-sites.toml): the node harness runs the
/// registered surfaces over real committed history (FIG-1530).
#[tokio::test]
async fn canonical_rows_survive_every_registered_production_renderer() {
    let rows = durable_transcript_projection_fixture().await;
    let script =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/transcript_projection.mjs");
    let node = std::env::var_os("LASH_WORKBENCH_TEST_NODE").unwrap_or_else(|| "node".into());
    let output = std::process::Command::new(node)
        .arg("--test")
        .arg(script)
        .env(
            "LASH_WORKBENCH_DURABLE_TOOL_TRANSCRIPT",
            serde_json::to_string(&rows).expect("serialize committed canonical rows"),
        )
        .output()
        .expect("Node.js is required for the registered transcript renderer law");
    assert!(
        output.status.success(),
        "registered transcript renderer law failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

/// The browser projection suite's env fixture is a pure typed value the
/// facade produces: the typed Stop terminal a cancel receipt carries. The
/// suite pins the workbench's projection state and rail rendering over it.
#[test]
fn workbench_browser_recovery_projection_preserves_rows_and_scopes_session_cursors() {
    let script =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/browser_projection.mjs");
    let node = std::env::var_os("LASH_WORKBENCH_TEST_NODE").unwrap_or_else(|| "node".into());
    let output = std::process::Command::new(node)
        .arg("--test")
        .arg(script)
        .env(
            "LASH_WORKBENCH_STOP_TERMINAL",
            serde_json::to_string(&lash::TurnTerminal::Committed {
                stop: Some(lash::TurnStop::Cancelled {
                    evidence: lash::TurnCancellationEvidence {
                        request_id: "workbench-stop-browser-projection".into(),
                        origin: Some("user".into()),
                        reason: Some("workbench Stop control".into()),
                        undelivered: Default::default(),
                        mode: lash::TurnCancelMode::AfterStep,
                        honoured_after_step: Some(0),
                    },
                }),
            })
            .expect("serialize the typed Stop terminal"),
        )
        .output()
        .expect("Node.js is required for the agent-workbench browser projection gate");
    assert!(
        output.status.success(),
        "browser projection gate failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
