use super::*;

use crate::tests::host::durable::*;

/// FIG-6171: presentation limits never discard committed call identities or
/// bodies. A cold reader can resolve every call beyond the prompt's 128 tail.
#[cfg(feature = "rlm")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_cell_call_beyond_the_prompt_bound_survives_a_reopen() -> Result<()> {
    fn core(backend: lash_execution_contracts::backend::Backend) -> Result<LashCore> {
        explicit_ephemeral_facets(rlm_core_builder_over(backend))
            .serve_test_llm_profile(
                part_provider(
                    "all-cell-calls",
                    typescript_block(
                        "for (let i = 0; i < 129; i++) { await tools.appLookup({}); }\n\
                         await control.finish(\"all calls\");",
                    ),
                ),
                mock_llm_profile_spec(),
            )
            .tools(crate::tool_provider_key!("tools"), Arc::new(AppTools))
            .build(crate::tests::host::runtime_lease_owner())
    }
    let backend = sqlite_memory_store_backend().await;
    let session_id = id("all-cell-calls");
    let running = core(backend.clone())?;
    let session = created_session(&running, session_id.clone())
        .await
        .open()
        .await?;
    let events = RecordingEvents::default();
    let run = turn("all-cell-calls-run");
    session
        .send(crate::TurnInput::text("all calls"))
        .id(run.clone())
        .output_into(&events)
        .await?;
    let transcript = lash_core::testing::turn_transcript(
        running.effect_host().durable_reads()?,
        &session_id,
        &run,
    )
    .await?;
    let sizes = lash_core::testing::cell_ledger_bytes(
        &transcript,
        &session.durable().read_turn_tool_calls(&run).await?,
    )
    .expect("the stored ledger sizes read");
    eprintln!(
        "FIG-6171 130 calls: report={} held={} previous_copy={}",
        sizes.0, sizes.1, sizes.2
    );
    assert!(
        sizes.1 < sizes.2,
        "the full retained call ledger shrinks: {sizes:?}"
    );

    let live = events
        .snapshot()
        .await
        .into_iter()
        .filter_map(|activity| match activity.event {
            crate::TurnEvent::ToolCallCompleted {
                call_id,
                provider_call_id,
                name,
                args,
                output,
                ..
            } => Some(lash_core::ToolCallRecord {
                execution: Default::default(),
                read: None,
                call_id,
                provider_call_id,
                tool: name,
                args,
                output,
            }),
            _ => None,
        })
        .collect::<Vec<_>>();
    drop(session);
    // Stop aborts the node rather than draining it or invoking plugin shutdown,
    // the same crash cut used by the facade recovery laws.
    running.node.stop().await;
    drop(running);

    let reopened = core(backend)?;
    let durable = reopened.session(session_id).durable().await?;
    let transcript = durable.transcript().await?;
    let (cell_turn, cell) = transcript
        .visible()
        .find_map(|entry| match &entry.item {
            crate::transcript::TranscriptItem::Cell(cell) => Some((
                entry.provenance.turn_id.as_ref().expect("the cell's turn"),
                cell,
            )),
            _ => None,
        })
        .expect("one committed cell");
    assert_eq!(
        cell.calls.len(),
        130,
        "every committed call stays addressable"
    );
    let bodies = durable.read_cell_tool_calls(cell_turn, &cell.calls).await?;
    assert_eq!(bodies.len(), 130);
    for (call, body) in cell.calls.iter().zip(bodies) {
        let crate::ToolCallRead::Settled { record } = body else {
            panic!("committed call has no body");
        };
        assert_eq!(call.call_id.as_ref(), Some(&record.call_id));
        assert!(record.output.is_success());
        if call.operation == "tools.app_lookup" {
            assert_eq!(record.tool, "tools.app_lookup");
            assert_eq!(record.args, serde_json::json!({}));
        }
    }
    let restored = durable
        .read_cell_tool_calls(cell_turn, &cell.calls)
        .await?
        .into_iter()
        .map(|body| {
            let crate::ToolCallRead::Settled { record } = body else {
                panic!("all calls settled");
            };
            *record
        })
        .collect::<Vec<_>>();
    assert_eq!(
        restored, live,
        "the reprojected requests and recorded outputs equal the live calls"
    );
    let last = cell
        .calls
        .last()
        .expect("the finish call")
        .call_id
        .as_ref()
        .expect("recorded call");
    assert!(matches!(
        durable
            .read_tool_call(&crate::TurnToolCallRef {
                turn_id: cell_turn.clone(),
                call_id: last.clone()
            })
            .await?,
        crate::ToolCallRead::Settled { .. }
    ));
    assert!(matches!(
        durable
            .read_tool_call(&crate::TurnToolCallRef {
                turn_id: cell_turn.clone(),
                call_id: crate::ToolCallId::fixture("not-in-this-session")
            })
            .await?,
        crate::ToolCallRead::NotFound { .. }
    ));
    reopened.shutdown().await?;
    Ok(())
}

/// FIG-7114: owner reads publish the committed actor owner across nodes,
/// including release and takeover, without acquiring or changing ownership.
#[tokio::test]
async fn a_session_owner_read_names_the_owning_node() -> Result<()> {
    use crate::durable::{ActorKey, CommitLabel, FormatSet, MailTx, NodeId, NodeSpec};

    let backend = sqlite_memory_store_backend().await;
    let core = |name: &str| {
        standard_core_builder_over(backend.clone())
            // Drive claims explicitly so no activation races the read's snapshot.
            .serve_sessions(false)
            .build(crate::persistence::LeaseOwnerIdentity::opaque(
                crate::persistence::LeaseOwnerId::new(name),
                crate::persistence::LeaseIncarnationId::new(format!("{name}-boot")),
            ))
            .expect("reader core")
    };
    let core_a = core("owner-A");
    let core_b = core("owner-B");
    let session_id = id("owner-read");
    let process_id = crate::ProcessId::fixture("owner-read");
    let session = core_b.session(session_id.clone()).durable().await?;
    let processes = core_b.processes();
    let store = backend.durable();
    let session_actor = ActorKey::session(session_id.as_str()).unwrap();
    let process_actor = ActorKey::process(process_id.as_str()).unwrap();
    let formats = FormatSet::new("owner-read");
    let spec = |core: &LashCore| NodeSpec {
        node: NodeId::new(core.runtime_owner.owner_id.clone()),
        decodes: vec![formats.clone()],
        serves: Vec::new(),
        capabilities: Default::default(),
        ttl_millis: 60_000,
    };
    let a = store.register_node(&spec(&core_a)).await?;
    let b = store.register_node(&spec(&core_b)).await?;

    assert_eq!(session.owner_node().await?, None);
    assert_eq!(processes.owner_node(&process_id).await?, None);
    let mut mail = MailTx::new();
    mail.create_actor(session_actor.clone(), formats.clone());
    mail.create_actor(process_actor.clone(), formats);
    store.commit_mail(mail, CommitLabel::TURN_ADMIT).await?;
    assert_eq!(store.claim(&a, 2).await?.len(), 2);

    // Reuse B's same handles throughout: no owner fact may be cached.
    for expected in [Some(a.owner.node.clone()), None, Some(b.owner.node.clone())] {
        let before_session = store.actor(&session_actor).await?;
        let before_process = store.actor(&process_actor).await?;
        assert_eq!(session.owner_node().await?, expected);
        assert_eq!(processes.owner_node(&process_id).await?, expected);
        assert_eq!(store.actor(&session_actor).await?, before_session);
        assert_eq!(store.actor(&process_actor).await?, before_process);
        if expected.as_ref() == Some(&a.owner.node) {
            assert_eq!(store.release_node(&a).await?.len(), 2);
        } else if expected.is_none() {
            assert_eq!(store.claim(&b, 2).await?.len(), 2);
        }
    }
    store.release_node(&b).await?;
    Ok(())
}
