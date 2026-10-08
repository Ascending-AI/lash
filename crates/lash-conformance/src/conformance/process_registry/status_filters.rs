use super::*;
use pretty_assertions::assert_eq;

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn list_filters_match_extracted_and_json_fields(registry: Arc<dyn ProcessRegistry>) {
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
                .with_admitted_identity(lash_core::AdmittedProcessIdentity::for_testing(
                    definition_identity(
                        lash_core::ProcessDefinitionRef::unclaimed(
                            "indexed-filter-kind",
                            serde_json::json!({"definition": "target"}),
                        ),
                        Some("filter-label"),
                    ),
                )),
            lash_core::ScopeId::turn(
                SessionId::from("filter-origin"),
                crate::TurnId::from("filter-turn"),
            ),
        ))
        .await
        .expect("register filter target");
    let process_id = record.id.clone();
    let authority = crate::ProcessExecutionWriteAuthority::invocation(
        process_id.clone(),
        "filter-target:execution",
    )
    .bind_attempt(1);
    registry
        .record_first_started_with_authority(
            &process_id,
            authority
                .invocation_started()
                .expect("a bound invocation names its execution"),
            &authority,
        )
        .await
        .expect("record filter target execution start");
    registry
        .set_process_wait_with_authority(
            &process_id,
            WaitState {
                since_ms: record.created_at_ms,
                kind: crate::WaitKind::Call {
                    call_id: lash_sansio::ToolCallId::fixture("process-wait-law"),
                    tool_id: lash_sansio::ToolId::new("process_wait"),
                },
            },
            Vec::new(),
            &authority,
        )
        .await
        .expect("set filter target waiting");
    let decoy_id = registry
        .register_process(registration("filter-decoy"))
        .await
        .expect("register filter decoy")
        .id;

    let matches = registry
        .list_processes(&ProcessListFilter {
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
        })
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
        let records = registry
            .list_processes(&ProcessListFilter {
                status,
                ..Default::default()
            })
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
    reference: lash_core::ProcessDefinitionRef,
    label: Option<impl Into<String>>,
) -> lash_core::ProcessIdentity {
    let id = lash_core::ProcessDefinitionDraft::new(
        reference.engine_kind.clone(),
        reference.definition.as_json().clone(),
        [],
    )
    .expect("fixture descriptor")
    .id();
    let mut identity = lash_core::ProcessIdentity::labelled(reference.engine_kind, label);
    identity.definition_id = Some(id);
    identity
}
