//! FIG-5020 / ADR 0051: a host constructs facade records using facade paths.
use crate::tracing::{TraceContext, TraceEvent, TraceRecord};

#[test]
fn host_owned_trace_records_supply_fresh_identity_and_timestamp() {
    let context = TraceContext {
        run_id: Some("host-run".into()),
        ..Default::default()
    };
    let event = TraceEvent::TurnStarted {
        metadata: Default::default(),
    };
    let before = std::time::SystemTime::now();
    let first = TraceRecord::host_owned(context.clone(), event.clone()).unwrap();
    let second = TraceRecord::host_owned(context.clone(), event.clone()).unwrap();
    assert_ne!(first.id, second.id);
    assert_eq!(first.id.len(), 32);
    assert_eq!(first.context, context);
    assert_eq!(first.event, event);
    assert!(std::time::SystemTime::from(first.timestamp) >= before);
    assert_eq!(
        serde_json::from_value::<TraceRecord>(serde_json::to_value(&first).unwrap()).unwrap(),
        first
    );
}

#[test]
fn run_identity_validates_and_retains_operation_spelling() {
    assert!(crate::RunId::parse(" ").is_err());
    let id = crate::RunId::parse("shift-operation:task-17").unwrap();
    assert_eq!(id.as_str(), "shift-operation:task-17");
    assert_eq!(
        serde_json::to_value(&id).unwrap(),
        serde_json::json!("shift-operation:task-17")
    );
    assert_eq!(crate::RunId::from(crate::TurnId::from(id.clone())), id);
}

#[test]
fn process_tool_accessor_selects_each_capability_contract() {
    use crate::process_controls::{ProcessControlTool as Tool, process_tool_definition};
    for (tool, name) in [
        (Tool::Start, "start_process"),
        (Tool::List, "list_process_handles"),
        (Tool::Await, "await_process"),
        (Tool::Get, "get_process_definition"),
        (Tool::Cancel, "cancel_process"),
    ] {
        assert_eq!(process_tool_definition(tool).name(), name);
    }
}

/// FIG-5020: every durable facade feed has a bounded page and an explicit
/// continuation, including an empty page; time cutoffs are validated instants.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn facade_change_feeds_share_bounded_pages_and_typed_cutoffs() -> crate::Result<()> {
    let core = super::explicit_ephemeral_facets(crate::LashCore::standard_builder(
        super::sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(super::mock_provider(), super::mock_llm_profile_spec())
    .build(crate::testing::runtime_lease_owner())?;
    let limit = std::num::NonZeroUsize::new(1).unwrap();
    let turns: crate::ChangePage<
        crate::persistence::TurnChange,
        crate::persistence::TurnChangeCursor,
    > = core
        .turns_changed_since(crate::persistence::TurnChangeCursor::initial(), limit)
        .await?;
    assert!(turns.changes.is_empty());
    assert_eq!(turns.next, crate::persistence::TurnChangeCursor::initial());
    assert_eq!(turns.retained_after, Some(turns.next));
    // Standing-fault pages retain the last item as their continuation; an
    // empty page retains the caller's cursor instead of restarting the feed.
    for label in ["fault-a", "fault-b"] {
        core.session(crate::SessionId::parse(label).expect("nonblank host identity"))
            .create(crate::SessionCreation::root(super::mock_session_spec()))
            .await?;
        let record = crate::SessionFaultRecord::new(
            crate::SessionFaultOrigin::DriveAdmission,
            &crate::runtime::RuntimeError::new(
                crate::runtime::RuntimeErrorCode::RuntimeStoreCorrupt,
                "corrupt",
            ),
        );
        core.store_factory
            .record_session_fault(&crate::SessionId::parse(label).unwrap(), &record, 1)
            .await?;
    }
    let first = core.session_faults(None, limit).await?;
    assert_eq!(first.changes.len(), 1);
    assert_eq!(first.next.as_ref().unwrap().as_str(), "fault-a");
    let second = core.session_faults(first.next.as_ref(), limit).await?;
    assert_eq!(second.changes.len(), 1);
    assert_eq!(second.next.as_ref().unwrap().as_str(), "fault-b");
    let after = crate::SessionId::parse("last-fault").unwrap();
    let faults: crate::ChangePage<crate::SessionFault, Option<crate::SessionId>> =
        core.session_faults(Some(&after), limit).await?;
    assert!(faults.changes.is_empty());
    assert_eq!(faults.next, Some(after));
    assert_eq!(faults.retained_after, None);
    Ok(())
}
