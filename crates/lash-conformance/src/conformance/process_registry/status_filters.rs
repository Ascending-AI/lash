use super::*;
use pretty_assertions::assert_eq;

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn process_roster_pages_match_status_sets_and_definition(
    seeding: Arc<dyn crate::ConformanceProcessRegistry>,
) {
    let registry: Arc<dyn ProcessRegistry> = seeding.clone();
    let record = registry
        .register_process(crate::started_until_starter(
            executed_registration("filter-target")
                .with_process_provenance(
                    ProcessProvenance::session(SessionScope::new("filter-origin")).with_caused_by(
                        Some(CausalRef::Process {
                            process_id: crate::ProcessId::fixture("filter-cause"),
                        }),
                    ),
                )
                .with_admitted_identity(
                    lash_execution_contracts::process::AdmittedProcessIdentity::for_testing(
                        definition_identity(
                            lash_execution_contracts::process::ProcessDefinitionRef::unclaimed(
                                "indexed-filter-kind",
                                serde_json::json!({"definition": "target"}),
                            ),
                            Some("filter-label"),
                        ),
                    ),
                ),
            lash_core::ScopeId::turn(
                SessionId::from("filter-origin"),
                crate::TurnId::from("filter-turn"),
            ),
        ))
        .await
        .expect("register filter target");
    let process_id = record.id.clone();
    seeding
        .append_event_for_testing(
            &process_id,
            ProcessEventAppendRequest::wait_entered(
                &process_id,
                &WaitState {
                    since_ms: i64::try_from(record.created_at_ms).unwrap_or(i64::MAX),
                    kind: crate::WaitKind::Call {
                        resume_at_ms: None,
                        call_id: lash_protocol_types::ToolCallId::fixture("process-wait-law"),
                        tool_id: lash_vocabulary::tool_contract::ToolId::new("process_wait"),
                    },
                    site: None,
                },
            ),
        )
        .await
        .expect("set filter target waiting");
    let decoy_id = registry
        .register_process(registration("filter-decoy"))
        .await
        .expect("register filter decoy")
        .id;

    let matches = lash_core::testing::process_roster_records_for_fixture(
        registry.as_ref(),
        &ProcessListFilter {
            definition_id: record.identity.definition_id.clone(),
            status: ProcessStatusFilter::any_of([ProcessStatus::Waiting]),

            originator: Some(ProcessOriginatorFilter::session("filter-origin")),
            until: record.lifetime.scope().cloned(),
            cancel_pending_before_ms: None,
            identity_kind: Some("indexed-filter-kind".to_string()),
            identity_label: Some("filter-label".to_string()),
            created_at_start_ms: Some(record.created_at_ms),
            created_at_end_ms: Some(record.created_at_ms.saturating_add(1)),
            retired_since_ms: None,
        },
    )
    .await
    .expect("list with all filters");
    assert_eq!(
        matches
            .into_iter()
            .map(|record| record.id)
            .collect::<Vec<_>>(),
        vec![process_id.to_string()]
    );
    for (status, target, decoy) in [
        (ProcessStatusFilter::default(), false, true),
        (
            ProcessStatusFilter::any_of([
                crate::ProcessStatus::Running,
                crate::ProcessStatus::Waiting,
            ]),
            true,
            true,
        ),
        (
            ProcessStatusFilter::any_of([
                crate::ProcessStatus::Waiting,
                crate::ProcessStatus::Completed,
            ]),
            true,
            false,
        ),
        (
            ProcessStatusFilter::any_of([
                crate::ProcessStatus::Running,
                crate::ProcessStatus::Completed,
                crate::ProcessStatus::Failed,
                crate::ProcessStatus::Cancelled,
                crate::ProcessStatus::Abandoned,
            ]),
            false,
            true,
        ),
        (ProcessStatusFilter::Any, true, true),
        (ProcessStatusFilter::any_of([]), false, false),
    ] {
        let records = lash_core::testing::process_roster_records_for_fixture(
            registry.as_ref(),
            &ProcessListFilter {
                status,
                ..Default::default()
            },
        )
        .await
        .expect("status set query");
        assert_eq!(records.iter().any(|row| row.id == process_id), target);
        assert_eq!(records.iter().any(|row| row.id == decoy_id), decoy);
    }
}

#[expect(
    clippy::expect_used,
    reason = "conformance fixture uses a valid descriptor without dependencies"
)]
fn definition_identity(
    reference: lash_execution_contracts::process::ProcessDefinitionRef,
    label: Option<impl Into<String>>,
) -> lash_execution_contracts::process::ProcessIdentity {
    let id = lash_execution_contracts::process::ProcessDefinitionDraft::new(
        reference.engine_kind.clone(),
        reference.definition.as_json().clone(),
        [],
    )
    .expect("fixture descriptor")
    .id();
    let mut identity =
        lash_execution_contracts::process::ProcessIdentity::labelled(reference.engine_kind, label);
    identity.definition_id = Some(id);
    identity
}
