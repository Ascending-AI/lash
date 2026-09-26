use super::*;
use lash_core::{ProcessEventLogTestSupport as _, ProcessLeases as _};

async fn read_all_event_metadata<R>(
    registry: &R,
    process_id: &lash_sansio::ProcessId,
    mode: lash_core::ProcessEventQueryMode,
) -> Result<(u64, Vec<usize>), lash_core::PluginError>
where
    R: lash_core::ProcessEventLog + ?Sized,
{
    let limit = std::num::NonZeroUsize::new(127).unwrap_or(std::num::NonZeroUsize::MIN);
    let mut after_sequence = 0;
    let mut expected_sequence = 1;
    let mut page_lengths = Vec::new();
    loop {
        let outcome = registry
            .event_page_after(process_id, after_sequence, limit, mode)
            .await?;
        let lash_core::ProcessEventReadOutcome::Retained(page) = outcome else {
            panic!("seeded event history must remain retained");
        };
        assert!(
            page.events.len() <= limit.get(),
            "a backend returned more rows than the requested page bound"
        );
        page_lengths.push(page.events.len());
        assert!(
            page_lengths.len() <= 80,
            "event-page continuation did not advance through 10,000 rows"
        );
        let metadata = match page.events {
            lash_core::ProcessEventPageEvents::Full(page_events) => page_events
                .into_iter()
                .map(|event| (event.sequence, event.event_type))
                .collect::<Vec<_>>(),
            lash_core::ProcessEventPageEvents::Lite(page_events) => page_events
                .into_iter()
                .map(|event| (event.sequence, event.event_type))
                .collect::<Vec<_>>(),
        };
        for (sequence, event_type) in metadata {
            assert_eq!(sequence, expected_sequence);
            assert_eq!(event_type, "page.event");
            expected_sequence += 1;
        }
        after_sequence = match page.more {
            lash_core::ProcessEventPageMore::Complete => break,
            lash_core::ProcessEventPageMore::More { after_sequence } => after_sequence,
        };
    }
    Ok((expected_sequence - 1, page_lengths))
}

#[expect(
    clippy::expect_used,
    reason = "cross-backend fixture: setup failures must stop the differential at their source"
)]
pub(super) async fn compare_bounded_process_event_pages(
    sqlite_root: &Path,
    postgres: &PostgresStorage,
    run_nonce: &str,
) {
    // The differential verifies ordered Full/Lite pages of at most 127 events over 10,000 rows on both backends; bounded memory is inferred from the limited SQL reads (rendered-SQL pin), not measured.
    const EVENT_COUNT: u64 = 10_000;
    let registration = || {
        lash_core::ProcessRegistration::new(
            lash_core::ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            lash_core::RecoveryContract::ExternallyOwned,
            lash_core::ProcessProvenance::host(),
            lash_core::ProcessLifecyclePolicy::new(
                lash_core::ParentScope::Host,
                lash_core::OnParentEnd::Abandon,
            ),
        )
        .with_extra_event_types([lash_core::ProcessEventType {
            name: "page.event".to_string(),
            payload_schema: lash_core::LashSchema::any(),
            semantics: lash_core::ProcessEventSemanticsSpec::default(),
        }])
    };

    let sqlite_path = sqlite_root.join("process-event-pages.db");
    let sqlite = lash_sqlite_store::SqliteProcessRegistry::open(
        &sqlite_path,
        sqlite_root.join("process-event-page-sessions"),
    )
    .await
    .expect("open SQLite process-event page fixture");
    let (sqlite_mint, postgres_mint) = paired_process_id_mints();
    let sqlite = sqlite.with_process_id_mint_for_testing(sqlite_mint);
    let postgres_registry = postgres
        .process_registry()
        .with_process_id_mint_for_testing(postgres_mint);
    let sqlite_record = sqlite
        .register_process(registration())
        .await
        .expect("register SQLite page process");
    let postgres_record = postgres_registry
        .register_process(registration())
        .await
        .expect("register PostgreSQL page process");
    assert_eq!(
        sqlite_record.id, postgres_record.id,
        "the paired mints name both rows alike"
    );
    let process_id = sqlite_record.id.clone();

    let request = || {
        lash_core::ProcessEventAppendRequest::new(
            "page.event",
            serde_json::json!({"body": "payload excluded by lite projection"}),
        )
        .with_replay_key("page-event-1")
    };
    let first = sqlite
        .append_event(&process_id, request())
        .await
        .expect("append SQLite seed event")
        .event;
    postgres_registry
        .append_event(&process_id, request())
        .await
        .expect("append PostgreSQL seed event");

    {
        let mut connection = rusqlite::Connection::open(&sqlite_path)
            .expect("open SQLite page fixture for bulk seed");
        let transaction = connection
            .transaction()
            .expect("begin SQLite page seed transaction");
        let mut insert = transaction
            .prepare(
                "INSERT INTO process_events
                 (process_id, sequence, event_type, idempotency_key, event_json)
                 VALUES (?1, ?2, ?3, NULL, ?4)",
            )
            .expect("prepare SQLite page seed");
        for sequence in 2..=EVENT_COUNT {
            let mut event = first.clone();
            event.sequence = sequence;
            insert
                .execute(rusqlite::params![
                    process_id.as_str(),
                    sequence as i64,
                    "page.event",
                    serde_json::to_string(&event).expect("encode SQLite seed event"),
                ])
                .expect("insert SQLite seed event");
        }
        drop(insert);
        transaction.commit().expect("commit SQLite page seed");
    }
    sqlx::query(
        "INSERT INTO lash_process_events
         (process_id, sequence, event_type, idempotency_key, event_json)
         SELECT $1, sequence, $2, NULL,
                jsonb_set($3::jsonb, '{sequence}', to_jsonb(sequence))::text
           FROM generate_series(2, $4) AS sequence",
    )
    .bind(process_id.as_str())
    .bind("page.event")
    .bind(serde_json::to_string(&first).expect("encode PostgreSQL seed event"))
    .bind(EVENT_COUNT as i64)
    .execute(postgres.pool())
    .await
    .expect("seed PostgreSQL process events");

    for mode in [
        lash_core::ProcessEventQueryMode::Full,
        lash_core::ProcessEventQueryMode::Lite,
    ] {
        let sqlite_events = read_all_event_metadata(&sqlite, &process_id, mode)
            .await
            .expect("read bounded SQLite process-event pages");
        let postgres_events = read_all_event_metadata(&postgres_registry, &process_id, mode)
            .await
            .expect("read bounded PostgreSQL process-event pages");
        assert_eq!(sqlite_events.0, EVENT_COUNT);
        assert_eq!(sqlite_events, postgres_events, "{mode:?} pages diverged");
    }

    let effect_label = format!("effect-pages-{run_nonce}");
    let effect_registration = || {
        lash_core::ProcessRegistration::new(
            lash_core::ProcessInput::Engine {
                kind: "effect-differential".to_string(),
                payload: serde_json::Value::Null,
            },
            lash_core::RecoveryContract::Rerunnable,
            lash_core::ProcessProvenance::host(),
            lash_core::ProcessLifecyclePolicy::new(
                lash_core::ParentScope::Host,
                lash_core::OnParentEnd::Abandon,
            ),
        )
        .with_execution_env_ref(Some(lash_core::ProcessExecutionEnvRef::new(
            "effect-differential-env",
        )))
        .with_admitted_identity(lash_core::AdmittedProcessIdentity::for_testing(
            lash_core::ProcessIdentity::for_definition(
                lash_core::ProcessDefinitionRef::unclaimed(
                    "effect-differential",
                    serde_json::Value::Null,
                ),
                Some(effect_label.as_str()),
            ),
        ))
    };
    let sqlite_effect_record = sqlite
        .register_process(effect_registration())
        .await
        .expect("register SQLite effect process");
    let postgres_effect_record = postgres_registry
        .register_process(effect_registration())
        .await
        .expect("register PostgreSQL effect process");
    assert_eq!(
        sqlite_effect_record.id, postgres_effect_record.id,
        "the paired mints name both effect rows alike"
    );
    let effect_id = sqlite_effect_record.id.clone();
    let owner =
        lash_core::LeaseOwnerIdentity::opaque("effect-differential", "effect-differential:1");
    let sqlite_lease = sqlite
        .claim_process_lease(&effect_id, &owner, 60_000)
        .await
        .expect("claim SQLite effect lease")
        .acquired()
        .expect("SQLite effect lease available");
    let postgres_lease = postgres_registry
        .claim_process_lease(&effect_id, &owner, 60_000)
        .await
        .expect("claim PostgreSQL effect lease")
        .acquired()
        .expect("PostgreSQL effect lease available");
    // Ten occurrences of one node: the writer records the first
    // `PROCESS_EFFECT_OCCURRENCE_CAP` one by one and counts the rest, by
    // class, in one omission record.
    let mut omitted = lash_core::ProcessEffectOmittedCounts::default();
    let mut recorded = Vec::new();
    for occurrence in 1..=10 {
        let replay_key = format!("fixture-effect:{occurrence}");
        let is_failure = occurrence == 7 || occurrence == 9;
        let class = if is_failure {
            lash_core::ProcessEffectOutcomeClass::Failure
        } else {
            lash_core::ProcessEffectOutcomeClass::Success
        };
        if !lash_core::ProcessEffectSummaryOccurrence::is_within_cap(occurrence) {
            omitted.record(class);
            continue;
        }
        recorded.push(
            lash_core::ProcessEffectSummaryOccurrence::new(
                "repeated-node",
                occurrence,
                if is_failure {
                    "triggers.fixture"
                } else {
                    "now"
                },
                class,
                is_failure.then(|| {
                    lash_core::TriggerOperationError::Invalid {
                        message: "fixture refusal".to_string(),
                    }
                    .failure_code()
                }),
                replay_key,
                lash_core::FleetFormat::current(),
            )
            .append_request(),
        );
    }
    let omissions = lash_core::ProcessEffectOmissions::new(
        std::collections::BTreeMap::from([("repeated-node".to_string(), omitted)]),
        lash_core::FleetFormat::current(),
    );
    // The run commits its summary at its boundaries (FIG-3571): a bare
    // batch, a wait's enter and its clear, each with a prelude, and the
    // terminal batch closing with the omission record. Each boundary is
    // written twice, as a redrive after a lost acknowledgement writes it.
    let wait = lash_core::WaitState {
        since_ms: 1,
        kind: lash_core::WaitKind::Signal {
            name: "fixture".to_string(),
            event_type: "signal.fixture".to_string(),
            key: format!("{effect_id}:signal.fixture:1"),
            ordinal: 1,
        },
    };
    let terminal = lash_core::ProcessAwaitOutput::from_tool_output(
        lash_core::ToolCallOutput::success(serde_json::json!({ "summary": "committed" })),
    );
    for (registry, lease) in [
        (&sqlite as &dyn lash_core::ProcessRegistry, &sqlite_lease),
        (
            &postgres_registry as &dyn lash_core::ProcessRegistry,
            &postgres_lease,
        ),
    ] {
        let authority = lash_core::ProcessExecutionWriteAuthority::lease(lease.clone());
        for _ in 0..2 {
            let receipts = registry
                .append_events(&effect_id, recorded[..4].to_vec(), &authority)
                .await
                .expect("append a summary batch under execution authority");
            assert_eq!(receipts.len(), 4);
        }
        for _ in 0..2 {
            registry
                .set_process_wait_with_authority(
                    &effect_id,
                    wait.clone(),
                    recorded[4..6].to_vec(),
                    &authority,
                )
                .await
                .expect("enter a wait with its summary prelude");
        }
        for _ in 0..2 {
            registry
                .clear_process_wait_with_authority(&effect_id, recorded[6..].to_vec(), &authority)
                .await
                .expect("clear the wait with its summary prelude");
        }
        for _ in 0..2 {
            registry
                .complete_process_with_prelude(
                    &effect_id,
                    terminal.clone(),
                    vec![omissions.append_request("fixture-effect:omissions")],
                    lash_core::ProcessCompletionAuthority::workflow_key(effect_id.to_string()),
                )
                .await
                .expect("complete with the terminal batch, then recover it");
        }
    }
    let logs = {
        let mut logs = Vec::new();
        for registry in [
            &sqlite as &dyn lash_core::ProcessRegistry,
            &postgres_registry as &dyn lash_core::ProcessRegistry,
        ] {
            logs.push(
                registry
                    .full_event_window(&effect_id, 0)
                    .await
                    .expect("read the boundary log")
                    .into_iter()
                    .map(|event| (event.event_type, event.sequence, event.payload))
                    .collect::<Vec<_>>(),
            );
        }
        logs
    };
    assert_eq!(logs[0], logs[1], "the boundary batches diverged");
    let mut summaries = Vec::new();
    for registry in [
        &sqlite as &dyn lash_core::ProcessRegistry,
        &postgres_registry as &dyn lash_core::ProcessRegistry,
    ] {
        let mut summary = lash_core::ProcessEffectSummary::default();
        for event in registry
            .full_event_window(&effect_id, 0)
            .await
            .expect("read effect events")
        {
            summary
                .fold_event(
                    &event.event_type,
                    &event.payload,
                    lash_core::FleetFormat::current(),
                )
                .expect("fold effect event");
        }
        summaries.push(summary);
    }
    assert_eq!(summaries[0], summaries[1], "effect projections diverged");
    let node = summaries[0].node("repeated-node").expect("effect node");
    assert_eq!(node.occurrences.len(), 8);
    assert_eq!(
        node.omitted.failure, 1,
        "the omitted failure keeps its class"
    );
    assert_eq!(node.omitted.success, 1);
    assert_eq!(
        node.occurrences[6].outcome_class,
        lash_core::ProcessEffectOutcomeClass::Failure
    );
    sqlx::query("DELETE FROM lash_processes WHERE process_id = $1")
        .bind(process_id.as_str())
        .execute(postgres.pool())
        .await
        .expect("clean up PostgreSQL page process");
    sqlx::query("DELETE FROM lash_processes WHERE process_id = $1")
        .bind(effect_id.as_str())
        .execute(postgres.pool())
        .await
        .expect("clean up PostgreSQL effect process");
}
