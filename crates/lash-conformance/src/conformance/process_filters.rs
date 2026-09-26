use super::process_registry::registration;
use super::*;
use lash_sansio::ProcessId;
use pretty_assertions::assert_eq;

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn list_processes_filters_by_enriched_fields(registry: Arc<dyn ProcessRegistry>) {
    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: each result is established by the setup above"
    )]
    async fn filtered_ids(
        registry: &Arc<dyn ProcessRegistry>,
        filter: ProcessListFilter,
    ) -> Vec<ProcessId> {
        registry
            .list_processes(&filter)
            .await
            .expect("list processes")
            .into_iter()
            .map(|record| record.id)
            .collect()
    }

    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: each result is established by the setup above"
    )]
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

    let scope = SessionScope::for_agent_frame(
        "filter-session",
        crate::session_graph::frame_node_id(&SessionId::from("filter-session"), "filter-frame"),
    );
    let originator_id = scope.session_id.clone();
    let target = registry
        .register_process(
            registration("proc-filter-target")
                .with_admitted_identity(lash_core::AdmittedProcessIdentity::for_testing(
                    ProcessIdentity::labelled("filter-kind", Some("target-label")),
                ))
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
        .register_process(registration("proc-filter-other").with_admitted_identity(
            lash_core::AdmittedProcessIdentity::for_testing(ProcessIdentity::labelled(
                "other-kind",
                Some("other-label"),
            )),
        ))
        .await
        .expect("register other");
    for (suffix, definition) in [
        ("null", serde_json::Value::Null),
        ("string", serde_json::json!("scalar-definition")),
        ("integer-one", serde_json::json!(1)),
        ("real-one", serde_json::json!(1.0)),
        ("integer-zero", serde_json::json!(0)),
        ("true", serde_json::json!(true)),
        ("false", serde_json::json!(false)),
        ("string-one", serde_json::json!("1")),
        ("object", serde_json::json!({"nested": "definition"})),
        (
            "nested-object",
            serde_json::json!({"z": {"b": 2, "a": 1}, "a": [3, 2, 1]}),
        ),
    ] {
        registry
            .register_process(
                registration(&format!("proc-filter-definition-{suffix}")).with_admitted_identity(
                    lash_core::AdmittedProcessIdentity::for_testing(
                        ProcessIdentity::for_definition(
                            lash_core::ProcessDefinitionRef::unclaimed(
                                "definition-kind",
                                definition,
                            ),
                            Some(&format!("definition-{suffix}")),
                        ),
                    ),
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
                originator: Some(ProcessOriginatorFilter::session(originator_id.clone())),
                ..ProcessListFilter::default()
            }
        )
        .await,
        vec![target.id.clone()]
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
        vec![target.id.clone()]
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
        vec![target.id.clone()]
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
        vec![target.id.clone()]
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
        vec![target.id.clone()]
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
        vec![target.id.clone()],
        "created-at range is start-inclusive and end-exclusive"
    );

    for definition in [
        serde_json::Value::Null,
        serde_json::json!("scalar-definition"),
        serde_json::json!(1),
        serde_json::json!(1.0),
        serde_json::json!(0),
        serde_json::json!(true),
        serde_json::json!(false),
        serde_json::json!("1"),
        serde_json::json!({"nested": "definition"}),
        serde_json::from_str(r#"{"a":[3,2,1],"z":{"a":1,"b":2}}"#).expect("nested object filter"),
    ] {
        assert_rust_parity(
            &registry,
            ProcessListFilter {
                status: ProcessStatusFilter::Any,
                definition: Some(definition.into()),
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
            created_at_start_ms: Some((i64::MAX as u64) + 1),
            ..ProcessListFilter::default()
        },
        ProcessListFilter {
            status: ProcessStatusFilter::Any,
            created_at_end_ms: Some((i64::MAX as u64) + 1),
            ..ProcessListFilter::default()
        },
        ProcessListFilter {
            status: ProcessStatusFilter::Any,
            created_at_start_ms: Some(0),
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

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn list_processes_bounds_retired_rows_without_hiding_live_rows(
    registry: Arc<dyn ProcessRegistry>,
) {
    const KIND: &str = "recent-retired-filter-kind";
    let mut recent_ids = Vec::new();
    for label in ["recent-filter-running", "recent-filter-old"] {
        recent_ids.push(
            registry
                .register_process_with_observers(
                    registration(label).with_admitted_identity(
                        lash_core::AdmittedProcessIdentity::for_testing(ProcessIdentity::labelled(
                            KIND,
                            Some(label),
                        )),
                    ),
                    &[SessionId::from("recent-filter-observer".to_string())],
                )
                .await
                .expect("register recent-retired fixture")
                .id,
        );
    }
    registry
        .complete_process(
            &recent_ids[1],
            ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                serde_json::json!({"age": "old"}),
            )),
            ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete old terminal process");
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let recent_filter_fresh_id = registry
        .register_process_with_observers(
            registration("recent-filter-fresh").with_admitted_identity(
                lash_core::AdmittedProcessIdentity::for_testing(ProcessIdentity::labelled(
                    KIND,
                    Some("recent-filter-fresh"),
                )),
            ),
            &[SessionId::from("recent-filter-observer".to_string())],
        )
        .await
        .expect("register fresh terminal process")
        .id;
    let fresh = registry
        .complete_process(
            &recent_filter_fresh_id,
            ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                serde_json::json!({"age": "fresh"}),
            )),
            ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete fresh terminal process")
        .stored()
        .clone();

    for (status, expected) in [
        (
            ProcessStatusFilter::Any,
            vec!["recent-filter-fresh", "recent-filter-running"],
        ),
        (
            ProcessStatusFilter::any_of([crate::ProcessStatus::Completed]),
            vec!["recent-filter-fresh"],
        ),
        (
            ProcessStatusFilter::any_of([crate::ProcessStatus::Running]),
            vec!["recent-filter-running"],
        ),
        (ProcessStatusFilter::any_of([]), vec![]),
    ] {
        let filter = ProcessListFilter {
            status,
            retired_since_ms: Some(fresh.updated_at_ms),
            ..Default::default()
        };
        let observed = registry
            .list_observed_by(&SessionId::from("recent-filter-observer"), &filter)
            .await
            .expect("bounded observer list");
        assert_eq!(recent_labels(&observed), expected);
        assert!(
            registry
                .list_observed_by(&SessionId::from("unrelated-observer"), &filter)
                .await
                .expect("unrelated observer list")
                .is_empty()
        );
    }
    let all_observed = registry
        .list_observed_by(
            &SessionId::from("recent-filter-observer"),
            &ProcessListFilter {
                status: ProcessStatusFilter::Any,
                ..Default::default()
            },
        )
        .await
        .expect("unbounded observer list");
    assert_eq!(
        all_observed.len(),
        3,
        "unbounded observer list retains old retired rows"
    );
    let filtered = registry
        .list_observed_by(
            &SessionId::from("recent-filter-observer"),
            &ProcessListFilter {
                status: ProcessStatusFilter::Any,
                identity_kind: Some("unrelated-kind".to_string()),
                retired_since_ms: Some(fresh.updated_at_ms),
                ..Default::default()
            },
        )
        .await
        .expect("observer identity filter");
    assert!(
        filtered.is_empty(),
        "remaining filters still apply conjunctively"
    );

    let recent = registry
        .list_processes(&ProcessListFilter {
            status: ProcessStatusFilter::Any,
            identity_kind: Some(KIND.to_string()),
            retired_since_ms: Some(fresh.updated_at_ms),
            ..ProcessListFilter::default()
        })
        .await
        .expect("list live plus recently retired processes");
    assert_eq!(
        recent_labels(&recent),
        ["recent-filter-fresh", "recent-filter-running"],
        "the bounded read must retain old live rows and exclude old retired rows"
    );

    let all = registry
        .list_processes(&ProcessListFilter {
            status: ProcessStatusFilter::Any,
            identity_kind: Some(KIND.to_string()),
            ..ProcessListFilter::default()
        })
        .await
        .expect("list all recent-retired fixtures");
    assert_eq!(
        recent_labels(&all),
        [
            "recent-filter-fresh",
            "recent-filter-old",
            "recent-filter-running"
        ],
        "the unbounded list must preserve every status"
    );
}

/// The labels of `records`, sorted: the recent-retired fixtures register
/// under labels, and a list orders rows by the id the registrar minted.
fn recent_labels(records: &[crate::ProcessRecord]) -> Vec<&str> {
    let mut labels = records
        .iter()
        .map(|record| record.identity.label.as_deref().unwrap_or_default())
        .collect::<Vec<_>>();
    labels.sort_unstable();
    labels
}

/// Parent-scope and pending-cancel narrowing, and the agent-frame half of the
/// typed originator filter.
///
/// Both new filters are index-served conjuncts rather than
/// `(? IS NULL OR ...)` disjunctions, so the pushdown and the Rust predicate
/// can disagree silently; every assertion below is therefore paired with a
/// parity check against `ProcessListFilter::matches_record`.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn list_processes_filters_by_until_scope_and_pending_cancel(
    registry: Arc<dyn ProcessRegistry>,
) {
    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: each result is established by the setup above"
    )]
    async fn filtered_ids(
        registry: &Arc<dyn ProcessRegistry>,
        filter: &ProcessListFilter,
    ) -> Vec<String> {
        registry
            .list_processes(filter)
            .await
            .expect("list processes")
            .iter()
            .map(fixture_label)
            .collect()
    }

    /// The label a fixture registered under, which names the row whatever id
    /// the registrar minted for it.
    fn fixture_label(record: &crate::ProcessRecord) -> String {
        record
            .identity
            .label
            .clone()
            .unwrap_or_else(|| record.id.to_string())
    }

    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: each result is established by the setup above"
    )]
    async fn assert_ids(
        registry: &Arc<dyn ProcessRegistry>,
        filter: ProcessListFilter,
        expected: &[&str],
        message: &str,
    ) {
        let mut actual = filtered_ids(registry, &filter).await;
        actual.sort();
        assert_eq!(
            actual,
            expected
                .iter()
                .map(|id| (*id).to_string())
                .collect::<Vec<_>>(),
            "{message}"
        );
        let all = registry
            .list_processes(&ProcessListFilter {
                status: ProcessStatusFilter::Any,
                ..ProcessListFilter::default()
            })
            .await
            .expect("list reference processes");
        let mut expected_by_predicate = all
            .iter()
            .filter(|record| filter.matches_record(record))
            .map(fixture_label)
            .collect::<Vec<_>>();
        expected_by_predicate.sort();
        assert_eq!(
            actual, expected_by_predicate,
            "SQL pushdown must match the Rust predicate: {message}"
        );
    }

    let session = SessionId::from("scope-filter-session");
    let frame_a = SessionScope::for_agent_frame(
        session.as_str(),
        crate::session_graph::frame_node_id(&session, "scope-frame-a"),
    );
    let frame_b = SessionScope::for_agent_frame(
        session.as_str(),
        crate::session_graph::frame_node_id(&session, "scope-frame-b"),
    );
    let turn_scope =
        lash_core::ScopeId::turn(session.clone(), crate::TurnId::from("scope-turn-one"));
    let other_turn_scope =
        lash_core::ScopeId::turn(session.clone(), crate::TurnId::from("scope-turn-two"));

    let mut scope_ids = std::collections::BTreeMap::new();
    for (id, scope, parent) in [
        ("scope-filter-a-child-one", &frame_a, &turn_scope),
        ("scope-filter-a-child-two", &frame_a, &turn_scope),
        ("scope-filter-a-other-turn", &frame_a, &other_turn_scope),
        ("scope-filter-b-child", &frame_b, &turn_scope),
    ] {
        let mut request = registration(id);
        request.provenance = ProcessProvenance::session(scope.clone());
        let request = crate::started_until_starter(request, parent.clone());
        let record = registry
            .register_process(request)
            .await
            .expect("register parent-scoped process");
        scope_ids.insert(id, record.id);
    }
    let child_one = scope_ids["scope-filter-a-child-one"].clone();
    let mut session_lived = registration("scope-filter-a-session");
    session_lived.provenance = ProcessProvenance::session(frame_a.clone());
    registry
        .register_process(crate::started_until(
            session_lived,
            turn_scope.clone(),
            lash_core::ScopeId::session(session.clone()),
        ))
        .await
        .expect("register a sibling living until the session");

    assert_ids(
        &registry,
        ProcessListFilter {
            status: ProcessStatusFilter::Any,
            until: Some(turn_scope.clone()),
            ..ProcessListFilter::default()
        },
        &[
            "scope-filter-a-child-one",
            "scope-filter-a-child-two",
            "scope-filter-b-child",
        ],
        "a turn parent scope returns exactly that turn's children",
    )
    .await;
    assert_ids(
        &registry,
        ProcessListFilter {
            status: ProcessStatusFilter::Any,
            until: Some(lash_core::ScopeId::session(session.clone())),
            originator: Some(ProcessOriginatorFilter::Session(frame_a.clone())),
            ..ProcessListFilter::default()
        },
        &["scope-filter-a-session"],
        "an `until` scope is a value the filter matches exactly, not a wildcard: \
         the session scope names the one sibling living until it, not the \
         children its starter turn also started",
    )
    .await;

    assert_ids(
        &registry,
        ProcessListFilter {
            status: ProcessStatusFilter::Any,
            originator: Some(ProcessOriginatorFilter::Session(frame_b.clone())),
            ..ProcessListFilter::default()
        },
        &["scope-filter-b-child"],
        "a frame-scoped originator filter returns only that frame's processes",
    )
    .await;
    let mut session_wide = filtered_ids(
        &registry,
        &ProcessListFilter {
            status: ProcessStatusFilter::Any,
            originator: Some(ProcessOriginatorFilter::session(session.clone())),
            ..ProcessListFilter::default()
        },
    )
    .await;
    session_wide.sort();
    assert_eq!(
        session_wide,
        [
            "scope-filter-a-child-one",
            "scope-filter-a-child-two",
            "scope-filter-a-other-turn",
            "scope-filter-a-session",
            "scope-filter-b-child",
        ],
        "a filter that names no frame stays session-wide"
    );

    let cancelled = registry
        .request_process_cancel(
            &child_one,
            lash_core::CancelOrigin::OperatorRequested,
            "actor:scope-filter".to_string(),
            None,
        )
        .await
        .expect("request cancellation");
    let requested_at_ms = cancelled
        .cancel_request
        .as_ref()
        .expect("accepted cancel request")
        .requested_at_ms;

    assert_ids(
        &registry,
        ProcessListFilter {
            status: ProcessStatusFilter::Any,
            cancel_pending_before_ms: Some(requested_at_ms.saturating_add(1)),
            ..ProcessListFilter::default()
        },
        &["scope-filter-a-child-one"],
        "the pending-cancel bound is exclusive of its own instant and finds the row",
    )
    .await;
    assert_ids(
        &registry,
        ProcessListFilter {
            status: ProcessStatusFilter::Any,
            cancel_pending_before_ms: Some(requested_at_ms),
            ..ProcessListFilter::default()
        },
        &[],
        "a bound at the request instant excludes it",
    )
    .await;

    registry
        .complete_process(
            &child_one,
            ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::cancelled(
                lash_core::ToolCancellation::runtime("cancel honoured"),
            )),
            ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("settle the cancelled process");
    assert_ids(
        &registry,
        ProcessListFilter {
            status: ProcessStatusFilter::Any,
            cancel_pending_before_ms: Some(requested_at_ms.saturating_add(1)),
            ..ProcessListFilter::default()
        },
        &[],
        "a settled row is no longer a pending cancel",
    )
    .await;
}
