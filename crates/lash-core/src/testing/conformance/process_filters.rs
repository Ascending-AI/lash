use super::process_registry::registration;
use super::*;

pub(super) async fn list_processes_filters_by_enriched_fields(registry: Arc<dyn ProcessRegistry>) {
    async fn filtered_ids(
        registry: &Arc<dyn ProcessRegistry>,
        filter: ProcessListFilter,
    ) -> Vec<String> {
        registry
            .list_processes(&filter)
            .await
            .expect("list processes")
            .into_iter()
            .map(|record| record.id)
            .collect()
    }

    async fn assert_rust_parity(registry: &Arc<dyn ProcessRegistry>, filter: ProcessListFilter) {
        let all = registry
            .list_processes(&ProcessListFilter {
                status: ProcessStatusFilter::Any,
                ..ProcessListFilter::default()
            })
            .await
            .expect("list reference processes");
        let expected = all
            .iter()
            .filter(|record| filter.matches_record(record))
            .map(|record| record.id.clone())
            .collect::<Vec<_>>();
        let actual = filtered_ids(registry, filter).await;
        assert_eq!(
            actual, expected,
            "SQL pushdown must match the Rust predicate"
        );
    }

    let scope = SessionScope::for_agent_frame("filter-session", "filter-frame");
    let originator_id = scope.session_id.clone();
    let target = registry
        .register_process(
            registration("proc-filter-target")
                .with_identity(ProcessIdentity::new("filter-kind").with_label(Some("target-label")))
                .with_process_provenance(ProcessProvenance::session(scope).with_caused_by(Some(
                    CausalRef::TriggerOccurrence {
                        occurrence_id: "occurrence-target".to_string(),
                        subscription_id: Some("subscription-target".to_string()),
                        subscription_incarnation: None,
                        subscription_revision: None,
                    },
                ))),
        )
        .await
        .expect("register target");
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    registry
        .register_process(
            registration("proc-filter-other")
                .with_identity(ProcessIdentity::new("other-kind").with_label(Some("other-label"))),
        )
        .await
        .expect("register other");
    for (suffix, definition) in [
        ("null", serde_json::Value::Null),
        ("string", serde_json::json!("scalar-definition")),
        ("number", serde_json::json!(17)),
        ("boolean", serde_json::json!(true)),
        ("object", serde_json::json!({"nested": "definition"})),
    ] {
        registry
            .register_process(
                registration(&format!("proc-filter-definition-{suffix}")).with_identity(
                    ProcessIdentity::new("definition-kind")
                        .with_label(Some(&format!("definition-{suffix}")))
                        .with_definition(Some(definition)),
                ),
            )
            .await
            .expect("register definition parity process");
    }

    assert_eq!(
        filtered_ids(
            &registry,
            ProcessListFilter {
                status: ProcessStatusFilter::Any,
                originator_id: Some(originator_id),
                ..ProcessListFilter::default()
            }
        )
        .await,
        vec!["proc-filter-target".to_string()]
    );
    assert_eq!(
        filtered_ids(
            &registry,
            ProcessListFilter {
                status: ProcessStatusFilter::Any,
                identity_kind: Some("filter-kind".to_string()),
                ..ProcessListFilter::default()
            }
        )
        .await,
        vec!["proc-filter-target".to_string()]
    );
    assert_eq!(
        filtered_ids(
            &registry,
            ProcessListFilter {
                status: ProcessStatusFilter::Any,
                identity_label: Some("target-label".to_string()),
                ..ProcessListFilter::default()
            }
        )
        .await,
        vec!["proc-filter-target".to_string()]
    );
    assert_eq!(
        filtered_ids(
            &registry,
            ProcessListFilter {
                status: ProcessStatusFilter::Any,
                caused_by_occurrence_id: Some("occurrence-target".to_string()),
                ..ProcessListFilter::default()
            }
        )
        .await,
        vec!["proc-filter-target".to_string()]
    );
    assert_eq!(
        filtered_ids(
            &registry,
            ProcessListFilter {
                status: ProcessStatusFilter::Any,
                caused_by_subscription_id: Some("subscription-target".to_string()),
                ..ProcessListFilter::default()
            }
        )
        .await,
        vec!["proc-filter-target".to_string()]
    );
    assert_eq!(
        filtered_ids(
            &registry,
            ProcessListFilter {
                status: ProcessStatusFilter::Any,
                created_at_start_ms: Some(target.created_at_ms),
                created_at_end_ms: Some(target.created_at_ms.saturating_add(1)),
                ..ProcessListFilter::default()
            }
        )
        .await,
        vec!["proc-filter-target".to_string()],
        "created-at range is start-inclusive and end-exclusive"
    );

    for definition in [
        serde_json::Value::Null,
        serde_json::json!("scalar-definition"),
        serde_json::json!(17),
        serde_json::json!(true),
        serde_json::json!({"nested": "definition"}),
    ] {
        assert_rust_parity(
            &registry,
            ProcessListFilter {
                status: ProcessStatusFilter::Any,
                definition: Some(definition),
                ..ProcessListFilter::default()
            },
        )
        .await;
    }
    for filter in [
        ProcessListFilter {
            status: ProcessStatusFilter::Any,
            identity_label: Some("target".to_string()),
            ..ProcessListFilter::default()
        },
        ProcessListFilter {
            status: ProcessStatusFilter::Any,
            created_at_start_ms: Some(u64::MAX),
            ..ProcessListFilter::default()
        },
        ProcessListFilter {
            status: ProcessStatusFilter::Any,
            created_at_end_ms: Some(u64::MAX),
            ..ProcessListFilter::default()
        },
        ProcessListFilter {
            status: ProcessStatusFilter::Any,
            created_at_start_ms: Some(target.created_at_ms),
            created_at_end_ms: Some(target.created_at_ms),
            ..ProcessListFilter::default()
        },
    ] {
        assert_rust_parity(&registry, filter).await;
    }
}
