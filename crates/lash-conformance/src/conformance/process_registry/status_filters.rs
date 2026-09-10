use super::*;
use pretty_assertions::assert_eq;

pub(super) async fn list_filters_match_extracted_and_json_fields(
    registry: Arc<dyn ProcessRegistry>,
) {
    let process_id = "filter-target";
    let record = registry
        .register_process(
            ProcessRegistration::new(
                process_id,
                ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                RecoveryContract::ExternallyOwned,
                ProcessProvenance::session(SessionScope::new("filter-origin")).with_caused_by(
                    Some(crate::CausalRef::TriggerOccurrence {
                        occurrence_id: "indexed-occurrence-target".to_string(),
                        subscription_id: Some("indexed-subscription-target".to_string()),
                        subscription_incarnation: None,
                        subscription_revision: None,
                    }),
                ),
            )
            .with_identity(
                ProcessIdentity::new("indexed-filter-kind")
                    .with_label(Some("filter-label"))
                    .with_definition(Some(serde_json::json!({"definition": "target"}))),
            ),
        )
        .await
        .expect("register filter target");
    registry
        .set_process_wait(
            process_id,
            WaitState {
                since_ms: record.created_at_ms,
                kind: WaitKind::Signal {
                    name: "ready".to_string(),
                    event_type: "signal.ready".to_string(),
                    key: "filter-target:signal.ready:1".to_string(),
                    ordinal: 1,
                },
            },
        )
        .await
        .expect("set filter target waiting");
    registry
        .register_process(registration("filter-decoy"))
        .await
        .expect("register filter decoy");

    let matches = registry
        .list_processes(&ProcessListFilter {
            definition: Some(serde_json::json!({"definition": "target"})),
            status: ProcessStatusFilter::any_of([ProcessStatus::Waiting]),

            originator_id: Some(record.originator_id()),
            identity_kind: Some("indexed-filter-kind".to_string()),
            identity_label: Some("filter-label".to_string()),
            caused_by_occurrence_id: Some("indexed-occurrence-target".to_string()),
            caused_by_subscription_id: Some("indexed-subscription-target".to_string()),
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
                crate::ProcessStatus::CallerDeparted,
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
        assert_eq!(records.iter().any(|row| row.id == "filter-decoy"), decoy);
    }
}
