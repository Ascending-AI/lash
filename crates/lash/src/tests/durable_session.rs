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
                text_provider(
                    "all-cell-calls",
                    typescript_block(
                        "for (let i = 0; i < 129; i++) { await tools.app_lookup({}); }\n\
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
            assert_eq!(record.tool, "app_lookup");
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
