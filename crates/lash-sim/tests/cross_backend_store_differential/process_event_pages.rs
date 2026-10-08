fn page_wait(id: &lash_sansio::ProcessId, label: &str) -> lash_core::ProcessEventAppendRequest {
    lash_core::ProcessEventAppendRequest::wait_entered(
        id,
        &lash_core::WaitState {
            since_ms: 1,
            kind: lash_core::WaitKind::Call {
                call_id: lash_sansio::ToolCallId::fixture(label),
                tool_id: lash_sansio::ToolId::new("page_fixture"),
            },
        },
    )
}

use super::*;
use lash_core::ProcessEventLogTestSupport as _;

#[expect(
    clippy::expect_used,
    reason = "differential fixture: setup and oracle failures stop the comparison"
)]
async fn release_observations(
    registry: &dyn lash_core::ProcessRegistry,
) -> (lash_sansio::ProcessId, Vec<lash_core::ProcessEventRelease>) {
    let process_id = registry
        .register_process(lash_core::testing::held_engine_registration(
            serde_json::Value::Null,
            lash_core::ProcessProvenance::host(),
            lash_core::Lifetime::Detached,
        ))
        .await
        .expect("register release process")
        .id;
    let request = |ordinal: u64| {
        page_wait(
            &process_id,
            &format!("release-{ordinal}:{}", "x".repeat(4096)),
        )
        .with_replay_key(format!("release-event-{ordinal}"))
    };
    for ordinal in 1..=3 {
        assert_eq!(
            registry
                .append_event(&process_id, request(ordinal))
                .await
                .expect("append release event")
                .event
                .sequence,
            ordinal
        );
    }
    let mut releases = Vec::new();
    for through in [2, 1, u64::MAX] {
        releases.push(
            registry
                .release_process_events(&process_id, through)
                .await
                .expect("release compared prefix"),
        );
    }
    let limit = std::num::NonZeroUsize::MIN;
    for mode in [
        lash_core::ProcessEventQueryMode::Full,
        lash_core::ProcessEventQueryMode::Lite,
    ] {
        assert_eq!(
            registry
                .event_page_after(&process_id, 0, limit, mode)
                .await
                .expect("read released prefix"),
            lash_core::ProcessEventReadOutcome::NoLongerRetained(
                lash_core::ProcessEventHistoryRetention::Released {
                    released_through: 3,
                }
            )
        );
    }
    let replay = registry
        .append_event(&process_id, request(1))
        .await
        .expect("replay released event");
    assert_eq!(replay.event.sequence, 1);
    assert_eq!(replay.event.fact.payload(), request(1).fact.payload());
    assert_eq!(replay.realization, lash_core::StoreRealization::Coalesced);
    assert_eq!(
        registry
            .append_event(&process_id, request(4))
            .await
            .expect("append retained suffix")
            .event
            .sequence,
        4
    );
    (process_id, releases)
}

#[tokio::test]
async fn event_release_differential_on_sqlite_memory_and_file() {
    let directory = tempfile::tempdir().expect("SQLite directory");
    let memory = lash_sqlite_store::SqliteStoreSet::memory()
        .await
        .expect("SQLite memory store set");
    let file = lash_sqlite_store::SqliteStoreSet::open(directory.path().join("file.db"))
        .await
        .expect("SQLite file store set");
    lash_core::testing::process_execution_env_fixture(memory.process_env_store().as_ref()).await;
    lash_core::testing::process_execution_env_fixture(file.process_env_store().as_ref()).await;
    let (_, memory_releases) = release_observations(memory.process_registry().as_ref()).await;
    let (_, file_releases) = release_observations(file.process_registry().as_ref()).await;
    assert_eq!(memory_releases, file_releases);
    assert_eq!(
        memory_releases,
        vec![
            lash_core::ProcessEventRelease {
                released_through: 2,
                released_events: 2,
            },
            lash_core::ProcessEventRelease {
                released_through: 2,
                released_events: 0,
            },
            lash_core::ProcessEventRelease {
                released_through: 3,
                released_events: 1,
            },
        ]
    );
}

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
                .map(|event| (event.sequence, event.fact.event_type()))
                .collect::<Vec<_>>(),
            lash_core::ProcessEventPageEvents::Lite(page_events) => page_events
                .into_iter()
                .map(|event| (event.sequence, event.kind.as_str()))
                .collect::<Vec<_>>(),
        };
        for (sequence, event_type) in metadata {
            assert_eq!(sequence, expected_sequence);
            assert_eq!(event_type, "process.waiting");
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
        lash_core::testing::held_engine_registration(
            serde_json::Value::Null,
            lash_core::ProcessProvenance::host(),
            lash_core::Lifetime::Detached,
        )
    };

    let sqlite_path = sqlite_root.join("process-event-pages.db");
    let sqlite =
        lash_sqlite_store::SqliteProcessRegistry::open_standalone_for_testing(&sqlite_path)
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
        page_wait(&process_id, "payload excluded by lite projection")
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
                    "process.waiting",
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
    .bind("process.waiting")
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
            lash_core::ProcessProvenance::host(),
            lash_core::Lifetime::Detached,
        )
        .with_execution_env_ref(Some(lash_core::testing::process_execution_env_fixture_ref()))
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
    let authority = lash_core::ProcessExecutionWriteAuthority::invocation(
        effect_id.clone(),
        "effect-differential:1",
    )
    .bind_attempt(1);
    for registry in [
        &sqlite as &dyn lash_core::ProcessRegistry,
        &postgres_registry as &dyn lash_core::ProcessRegistry,
    ] {
        registry
            .record_first_started_with_authority(
                &effect_id,
                authority
                    .invocation_started()
                    .expect("bound differential invocation has a started fact"),
                &authority,
            )
            .await
            .expect("record differential invocation start");
    }
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
        if !lash_core::ProcessEffectOccurrence::is_within_cap(occurrence) {
            omitted.record(class);
            continue;
        }
        recorded.push(
            lash_core::ProcessEffectOccurrence::new(
                "repeated-node",
                occurrence,
                if is_failure { "fixture.write" } else { "now" },
                class,
                is_failure
                    .then(|| lash_core::FailureCode::from(&lash_core::RuntimeErrorCode::Plugin)),
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
        kind: lash_core::WaitKind::Call {
            call_id: lash_sansio::ToolCallId::fixture("process-wait-law"),
            tool_id: lash_sansio::ToolId::new("process_wait"),
        },
    };
    let terminal = lash_core::ProcessAwaitOutput::from_tool_output(
        lash_core::ToolCallOutput::success(serde_json::json!({ "summary": "committed" })),
    );
    for registry in [
        &sqlite as &dyn lash_core::ProcessRegistry,
        &postgres_registry as &dyn lash_core::ProcessRegistry,
    ] {
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
                    .map(|event| {
                        (
                            event.fact.event_type().to_owned(),
                            event.sequence,
                            event.fact.payload(),
                        )
                    })
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
        let mut summary = lash_core::ProcessEffectReport::default();
        for event in registry
            .full_event_window(&effect_id, 0)
            .await
            .expect("read effect events")
        {
            summary
                .fold_event(&event.fact, lash_core::FleetFormat::current())
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
    let (_, sqlite_releases) = release_observations(&sqlite).await;
    let (postgres_release_id, postgres_releases) = release_observations(&postgres_registry).await;
    assert_eq!(
        sqlite_releases, postgres_releases,
        "event releases diverged"
    );
    sqlx::query("DELETE FROM lash_processes WHERE process_id = $1")
        .bind(postgres_release_id.as_str())
        .execute(postgres.pool())
        .await
        .expect("clean up PostgreSQL release process");
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

async fn prepared_registration_race(
    registry: &dyn lash_core::ProcessRegistry,
    nonce: &str,
) -> String {
    let registration = lash_core::testing::held_engine_registration(
        serde_json::json!({"start": "race"}),
        lash_core::ProcessProvenance::host(),
        lash_core::Lifetime::Detached,
    )
    .with_start_key(Some(lash_core::StartKey::for_host(format!(
        "{nonce}:prepared-race"
    ))));
    let first = registry
        .prepare_process_registration(registration.clone(), &[])
        .await
        .unwrap_or_else(|error| panic!("first read-only plan: {error}"));
    let second = registry
        .prepare_process_registration(registration, &[])
        .await
        .unwrap_or_else(|error| panic!("second read-only plan: {error}"));
    assert_ne!(first.process_id(), second.process_id());
    assert!(
        registry
            .get_process(first.process_id())
            .await
            .unwrap_or_else(|error| panic!("pure prepare: {error}"))
            .is_none()
    );
    assert!(
        registry
            .get_process(second.process_id())
            .await
            .unwrap_or_else(|error| panic!("pure prepare: {error}"))
            .is_none()
    );
    let first_anchor = lash_core::TraceAnchor::Context(
        lash_core::TraceCarrier::parse_w3c(
            "00-11111111111111111111111111111111-2222222222222222-01",
            None,
        )
        .unwrap_or_else(|error| panic!("first anchor: {error}")),
    );
    let second_anchor = lash_core::TraceAnchor::Context(
        lash_core::TraceCarrier::parse_w3c(
            "00-33333333333333333333333333333333-4444444444444444-01",
            None,
        )
        .unwrap_or_else(|error| panic!("second anchor: {error}")),
    );
    let (a, b) = tokio::join!(
        registry.commit_process_registration(first, first_anchor.clone()),
        registry.commit_process_registration(second, second_anchor.clone())
    );
    let (a, b) = (
        a.unwrap_or_else(|error| panic!("first commit: {error}")),
        b.unwrap_or_else(|error| panic!("second commit: {error}")),
    );
    assert_ne!(a.is_created(), b.is_created());
    let (winner, loser, expected) = if a.is_created() {
        (a, b, first_anchor)
    } else {
        (b, a, second_anchor)
    };
    assert_eq!(winner.record.id, loser.record.id);
    assert_eq!(winner.record.trace, loser.record.trace);
    assert_eq!(
        winner
            .record
            .trace
            .as_ref()
            .unwrap_or_else(|| panic!("retained scope"))
            .anchor,
        expected
    );
    let retained = registry
        .get_process(&winner.record.id)
        .await
        .unwrap_or_else(|error| panic!("read winner: {error}"))
        .unwrap_or_else(|| panic!("retained winner"));
    assert_eq!(retained.trace, winner.record.trace);
    "prepared=read-only writers=one anchor=winner".into()
}
#[tokio::test]
async fn prepared_registration_race_keeps_the_first_scope() {
    let stores = lash_sqlite_store::SqliteStoreSet::memory()
        .await
        .unwrap_or_else(|error| panic!("SQLite memory: {error}"));
    lash_core::testing::process_execution_env_fixture(stores.process_env_store().as_ref()).await;
    assert_eq!(
        prepared_registration_race(stores.process_registry().as_ref(), "memory").await,
        "prepared=read-only writers=one anchor=winner"
    );
}
#[tokio::test]
#[ignore = "compares three durable backends; requires PostgreSQL in a with-service.sh pg gate"]
async fn prepared_registration_scope_matches_across_backends() {
    let (_lock, postgres, _url) = open_postgres_differential()
        .await
        .unwrap_or_else(|error| panic!("PostgreSQL gate: {error}"));
    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("SQLite file root: {error}"));
    let memory = lash_sqlite_store::SqliteStoreSet::memory()
        .await
        .unwrap_or_else(|error| panic!("SQLite memory: {error}"));
    let file = lash_sqlite_store::SqliteStoreSet::open(root.path().join("lash.db"))
        .await
        .unwrap_or_else(|error| panic!("SQLite file: {error}"));
    let attachments =
        tempfile::tempdir().unwrap_or_else(|error| panic!("attachment root: {error}"));
    let pg = lash_postgres_store::PostgresStoreSet::new(
        &postgres,
        lash_sqlite_store::SqliteStoreSet::open((attachments.path()).join("attachments.db"))
            .await
            .expect("SQLite attachment store")
            .attachment_store(),
    );
    let nonce = run_nonce();
    lash_core::testing::process_execution_env_fixture(memory.process_env_store().as_ref()).await;
    lash_core::testing::process_execution_env_fixture(file.process_env_store().as_ref()).await;
    let expected = prepared_registration_race(memory.process_registry().as_ref(), &nonce).await;
    assert_eq!(
        prepared_registration_race(file.process_registry().as_ref(), &nonce).await,
        expected
    );
    assert_eq!(
        prepared_registration_race(pg.process_registry().as_ref(), &nonce).await,
        expected
    );
}
