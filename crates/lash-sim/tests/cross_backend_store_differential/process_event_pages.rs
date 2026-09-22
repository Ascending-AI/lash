use super::*;

async fn read_all_event_metadata<R>(
    registry: &R,
    process_id: &lash_sansio::ProcessId,
    mode: lash_core::ProcessEventQueryMode,
) -> Result<(u64, Vec<usize>), lash_core::PluginError>
where
    R: lash_core::ProcessEventLog + ?Sized,
{
    let limit = std::num::NonZeroUsize::new(127).unwrap_or(std::num::NonZeroUsize::MIN);
    let mut continuation = None;
    let mut expected_sequence = 1;
    let mut page_lengths = Vec::new();
    loop {
        let outcome = registry
            .event_page(process_id, limit, mode, continuation)
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
        continuation = match page.more {
            lash_core::ProcessEventPageMore::Complete => break,
            lash_core::ProcessEventPageMore::More { continuation } => Some(continuation),
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
    let process_id = lash_sansio::ProcessId::from(format!("event-pages-{run_nonce}"));
    let registration = || {
        lash_core::ProcessRegistration::new(
            process_id.clone(),
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
    let postgres_registry = postgres.process_registry();
    let sqlite_record = sqlite
        .register_process(registration())
        .await
        .expect("register SQLite page process");
    let postgres_record = postgres_registry
        .register_process(registration())
        .await
        .expect("register PostgreSQL page process");
    assert_eq!(sqlite_record.incarnation, postgres_record.incarnation);

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
                 (process_id, process_incarnation, sequence, event_type, idempotency_key, event_json)
                 VALUES (?1, ?2, ?3, ?4, NULL, ?5)",
            )
            .expect("prepare SQLite page seed");
        for sequence in 2..=EVENT_COUNT {
            let mut event = first.clone();
            event.sequence = sequence;
            insert
                .execute(rusqlite::params![
                    process_id.as_str(),
                    sqlite_record.incarnation.registration_sequence() as i64,
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
         (process_id, process_incarnation, sequence, event_type, idempotency_key, event_json)
         SELECT $1, $2, sequence, $3, NULL,
                jsonb_set($4::jsonb, '{sequence}', to_jsonb(sequence))::text
           FROM generate_series(2, $5) AS sequence",
    )
    .bind(process_id.as_str())
    .bind(postgres_record.incarnation.registration_sequence() as i64)
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

    sqlx::query("DELETE FROM lash_processes WHERE process_id = $1")
        .bind(process_id.as_str())
        .execute(postgres.pool())
        .await
        .expect("clean up PostgreSQL page process");
}
