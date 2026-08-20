// Remote-trigger round-trip assertions, split out of `tests.rs` when that file
// reached its line budget. Included rather than declared as a module, matching
// the other test sections here.

    async fn assert_remote_trigger_subscription_records_round_trip(
        data_dir: &std::path::Path,
        session_id: &str,
    ) -> Vec<lash::triggers::TriggerSubscriptionRecord> {
        let store = lash_sqlite_store::SqliteTriggerStore::open(&data_dir.join("triggers.db"))
            .await
            .expect("open trigger store for remote DTO round trip");
        let filter = lash::triggers::TriggerSubscriptionFilter::for_session(session_id);
        let remote_filter =
            lash_remote_protocol::RemoteTriggerSubscriptionFilter::from(filter.clone());
        remote_filter
            .validate()
            .expect("remote trigger subscription filter should validate");
        let round_trip_filter: lash::triggers::TriggerSubscriptionFilter = remote_filter
            .try_into()
            .expect("remote trigger subscription filter should convert back");
        assert_eq!(round_trip_filter, filter);

        let records = lash::triggers::TriggerStore::list_subscriptions(&store, filter)
            .await
            .expect("list persisted trigger subscriptions for remote DTO round trip");
        let remote_list =
            lash_remote_protocol::RemoteTriggerListSubscriptionsResponse::try_from(records.clone())
                .expect("remote trigger subscription list");
        remote_list
            .validate()
            .expect("remote trigger subscription list should validate");
        let round_trip_records: Vec<lash::triggers::TriggerSubscriptionRecord> = remote_list
            .try_into()
            .expect("remote trigger subscription list should convert back");
        assert_eq!(round_trip_records, records);

        for record in &records {
            let remote_record =
                lash_remote_protocol::RemoteTriggerSubscriptionRecord::try_from(record.clone())
                    .expect("remote trigger subscription record");
            remote_record
                .validate("WorkbenchTriggerSubscription")
                .expect("remote trigger subscription record should validate");
            let round_trip_record: lash::triggers::TriggerSubscriptionRecord = remote_record
                .try_into()
                .expect("remote trigger subscription record should convert back");
            assert_eq!(&round_trip_record, record);

            let remote_result =
                lash_remote_protocol::RemoteTriggerRegisterSubscriptionReceipt::try_from(
                    record.clone(),
                )
                .expect("remote trigger register result");
            remote_result
                .validate()
                .expect("remote trigger register result should validate");
            let round_trip_result: lash::triggers::TriggerSubscriptionRecord = remote_result
                .try_into()
                .expect("remote trigger register result should convert back");
            assert_eq!(&round_trip_result, record);
        }

        records
    }

    fn assert_remote_trigger_emit_report_round_trip(report: &lash::triggers::TriggerEmitReport) {
        let remote = lash_remote_protocol::RemoteTriggerEmitReport::from(report.clone());
        remote
            .validate()
            .expect("remote trigger emit report should validate");
        let round_trip: lash::triggers::TriggerEmitReport = remote
            .try_into()
            .expect("remote trigger emit report should convert back");
        assert_eq!(&round_trip, report);
    }

    async fn assert_remote_started_process_surface(
        core: &LashCore,
        registry: &dyn lash::process::ProcessRegistry,
        session_id: &str,
        process_ids: &[String],
    ) {
        let filter = lash::process::ProcessListFilter {
            definition: None,
            status: lash::process::ProcessStatusFilter::Any,
            waiting: None,
            ..Default::default()
        };
        let observed = core
            .processes()
            .list(&filter)
            .await
            .expect("list observed processes for remote DTO round trip");
        let remote_list =
            lash_remote_protocol::RemoteProcessListResponse::try_from(observed.clone())
                .expect("observed process list should convert to remote DTO");
        remote_list
            .validate()
            .expect("remote process list should validate");
        let round_trip_observed: Vec<lash::process::ObservedProcess> = remote_list
            .try_into()
            .expect("remote process list should convert back");
        for process_id in process_ids {
            assert!(
                round_trip_observed
                    .iter()
                    .any(|process| process.process_id == *process_id),
                "remote process list did not include started process {process_id}"
            );
        }

        let snapshot = core
            .processes()
            .session_snapshot(session_id)
            .await
            .expect("capture process work snapshot for remote DTO round trip");
        let remote_snapshot = lash_remote_protocol::RemoteProcessWorkSnapshot::try_from(snapshot)
            .expect("process work snapshot should convert to remote DTO");
        remote_snapshot
            .validate()
            .expect("remote process work snapshot should validate");
        let round_trip_snapshot: lash::process::ProcessWorkSnapshot = remote_snapshot
            .try_into()
            .expect("remote process work snapshot should convert back");
        assert_eq!(round_trip_snapshot.session_id, session_id);

        for process_id in process_ids {
            let record = registry
                .get_process(process_id)
                .await
                .expect("process read should succeed")
                .expect("started process record should exist");
            let remote_record = lash_remote_protocol::RemoteProcessRecord::try_from(record)
                .expect("started process record should convert to remote DTO");
            remote_record
                .validate("WorkbenchStartedProcessRecord")
                .expect("remote started process record should validate");
            let round_trip_record: lash::process::ProcessRecord = remote_record
                .try_into()
                .expect("remote started process record should convert back");
            assert_eq!(&round_trip_record.id, process_id);

            let events = registry
                .recent_events(process_id, 32)
                .await
                .expect("load started process event tail for remote DTO round trip");
            let expected_tail = events
                .iter()
                .map(|event| (event.sequence, event.event_type.clone()))
                .collect::<Vec<_>>();
            let remote_events = lash_remote_protocol::RemoteProcessEventsResponse::try_from((
                process_id.clone(),
                events,
            ))
            .expect("process events serialize for the remote protocol");
            remote_events
                .validate()
                .expect("remote started process event tail should validate");
            let (round_trip_process_id, round_trip_events): (
                String,
                Vec<lash::process::ProcessEvent>,
            ) = remote_events
                .try_into()
                .expect("remote started process event tail should convert back");
            let round_trip_tail = round_trip_events
                .iter()
                .map(|event| (event.sequence, event.event_type.clone()))
                .collect::<Vec<_>>();
            assert_eq!(round_trip_process_id, *process_id);
            assert_eq!(round_trip_tail, expected_tail);
        }
        let observed_ids = observed
            .iter()
            .map(|process| process.process_id.as_str())
            .collect::<Vec<_>>();
        let round_trip_ids = round_trip_observed
            .iter()
            .map(|process| process.process_id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(round_trip_ids, observed_ids);
    }
